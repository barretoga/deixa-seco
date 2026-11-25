//! Interface: tela de volumes, progresso da varredura e mapa com o painel de sugestões.

pub mod cleanup_flow;
pub mod confirm;
pub mod map_view;
pub mod nav;
pub mod search;
pub mod sidebar;
pub mod suggestions;
pub mod volumes_view;

pub use cleanup_flow::MapScreen;
pub use map_view::{MapAction, MapState, PreviewMap};
pub use nav::Nav;

use crate::format::{format_bytes, format_count};
use crate::insights::{Insight, Program};
use crate::kind::FileKind;
use crate::scan::{ScanEvent, ScanMethod, start_scan};
use crate::tree::Tree;
use crate::volumes::{VolumeInfo, list_volumes};
use eframe::egui::{self, Key, PointerButton, RichText};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use volumes_view::VolumesAction;

/// O que a tela de progresso recebe. `ScanEvent` mais a montagem do mapa e as sugestões, que
/// também rodam fora da thread da UI (cores por tipo dominante e `analyze` custam passes na
/// árvore toda).
pub enum ScanUpdate {
    Progress {
        files: u64,
        bytes: u64,
    },
    /// A varredura acabou; o mapa está sendo montado.
    Preparing,
    Ready(Box<MapState>, Vec<Insight>, ScanMethod),
    Failed(String),
}

/// Vaga única do mapa parcial entre o repassador e a UI: cada preview novo substitui o
/// anterior, e a UI pega o que houver a cada quadro. Nada se acumula se a UI parar de
/// redesenhar (janela minimizada), ao contrário de um canal.
#[derive(Clone, Default)]
pub struct PreviewSlot(Arc<Mutex<Option<Box<PreviewMap>>>>);

impl PreviewSlot {
    /// Guarda `map` no lugar do que estiver lá.
    pub fn put(&self, map: PreviewMap) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(Box::new(map));
    }

    /// Tira o preview guardado, se houver um novo.
    pub fn take(&self) -> Option<Box<PreviewMap>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Inicia a varredura e uma thread que repassa os eventos e monta o `MapState`. O mapa
/// parcial não passa pelo canal: vai para a `PreviewSlot` devolvida.
/// Descartar o `Receiver` devolvido para tudo: o repassador perde o envio, sai e descarta
/// o `Receiver` da varredura, que então se cancela.
pub fn start_scan_job(root: PathBuf, try_mft: bool) -> (Receiver<ScanUpdate>, PreviewSlot) {
    let inner = start_scan(root, try_mft);
    let (tx, rx) = mpsc::channel();
    let slot = PreviewSlot::default();
    let relay_slot = slot.clone();
    std::thread::spawn(move || relay(inner, &tx, &relay_slot));
    (rx, slot)
}

/// Corpo do repassador: até a varredura acabar ou a UI largar o `Receiver`.
fn relay(inner: Receiver<ScanEvent>, tx: &Sender<ScanUpdate>, slot: &PreviewSlot) {
    let mut stashed = None;
    loop {
        let ev = match stashed.take() {
            Some(ev) => ev,
            None => match inner.recv() {
                Ok(ev) => ev,
                Err(_) => return,
            },
        };
        let update = match ev {
            ScanEvent::Progress { files, bytes } => ScanUpdate::Progress { files, bytes },
            ScanEvent::Preview { tree, kinds } => {
                // Só o mais recente: os que já estão na fila ficaram velhos.
                let ((tree, kinds), next) = latest_preview((tree, kinds), &inner);
                stashed = next;
                slot.put(PreviewMap::new(tree, kinds));
                continue;
            }
            ScanEvent::Done(tree, method) => {
                if tx.send(ScanUpdate::Preparing).is_err() {
                    return;
                }
                let insights = cleanup_flow::analyze_now(&tree);
                let mut state = MapState::new(tree);
                state.set_explainable(suggestions::explainable_set(&insights));
                ScanUpdate::Ready(Box::new(state), insights, method)
            }
            ScanEvent::Failed(e) => ScanUpdate::Failed(e),
        };
        if tx.send(update).is_err() {
            return;
        }
    }
}

/// Esvazia o que já chegou de `Preview` depois de `tree` e fica com o último. O primeiro
/// evento de outro tipo para a busca e é devolvido para ser tratado em seguida.
fn latest_preview(
    mut preview: (Tree, Vec<FileKind>),
    inner: &Receiver<ScanEvent>,
) -> ((Tree, Vec<FileKind>), Option<ScanEvent>) {
    loop {
        match inner.try_recv() {
            Ok(ScanEvent::Preview { tree, kinds }) => preview = (tree, kinds),
            Ok(other) => return (preview, Some(other)),
            Err(_) => return (preview, None),
        }
    }
}

/// Lista os volumes numa thread (pode demorar com unidade de rede lenta).
fn start_list_volumes() -> Receiver<Vec<VolumeInfo>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(list_volumes());
    });
    rx
}

pub enum Screen {
    Volumes,
    Scanning {
        rx: Receiver<ScanUpdate>,
        /// Onde o repassador deixa o mapa parcial mais recente.
        preview_slot: PreviewSlot,
        files: u64,
        bytes: u64,
        root: PathBuf,
        /// Letra, quando a varredura é de um volume inteiro (para a dica de admin).
        volume: Option<char>,
        started: Instant,
        preparing: bool,
        /// Último mapa parcial (varredura de pastas); `None` até o primeiro chegar.
        preview: Option<Box<PreviewMap>>,
    },
    Map(Box<MapScreen>),
}

/// Como foi a última varredura (mostrado no rodapé do mapa).
struct ScanSummary {
    method: ScanMethod,
    elapsed: Duration,
}

/// Segundos com uma casa decimal e vírgula (`3,2 s`).
pub fn format_secs(d: Duration) -> String {
    let tenths = (d.as_millis() + 50) / 100;
    format!("{},{} s", tenths / 10, tenths % 10)
}

pub struct App {
    screen: Screen,
    elevated: bool,
    volumes: Vec<VolumeInfo>,
    /// Lista de volumes sendo lida (mostra "carregando…").
    volumes_rx: Option<Receiver<Vec<VolumeInfo>>>,
    /// Aviso na tela atual (falha de varredura, UAC recusado…).
    message: Option<String>,
    last_scan: Option<ScanSummary>,
    /// Varreu um volume NTFS pelas pastas sem admin: oferece a varredura rápida.
    fast_hint: Option<char>,
    /// Painel de sugestões aberto (vale para a sessão toda).
    sidebar_open: bool,
    /// Programas instalados sendo lidos (começa junto com a varredura).
    programs_rx: Option<Receiver<Vec<Program>>>,
}

impl App {
    /// `scan_on_start`: volume pedido com `--scan` (o app relançado como admin começa varrendo).
    pub fn new(scan_on_start: Option<char>) -> App {
        let mut app = App {
            screen: Screen::Volumes,
            elevated: crate::elevation::is_elevated(),
            volumes: Vec::new(),
            volumes_rx: Some(start_list_volumes()),
            message: None,
            last_scan: None,
            fast_hint: None,
            sidebar_open: true,
            programs_rx: None,
        };
        if let Some(letter) = scan_on_start {
            app.screen = app.begin_scan(PathBuf::from(format!("{letter}:\\")), true, Some(letter));
        }
        app
    }

    fn reload_volumes(&mut self) {
        self.volumes.clear();
        self.volumes_rx = Some(start_list_volumes());
    }

    fn poll_volumes(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.volumes_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(v) => {
                self.volumes = v;
                self.volumes_rx = None;
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
            Err(TryRecvError::Disconnected) => self.volumes_rx = None,
        }
    }

    /// `Some(true/false)` se já se sabe se o volume é NTFS; `None` enquanto a lista carrega.
    fn is_ntfs(&self, letter: char) -> Option<bool> {
        if self.volumes_rx.is_some() {
            return None;
        }
        Some(
            self.volumes
                .iter()
                .any(|v| v.letter == letter && volumes_view::is_ntfs(v)),
        )
    }

    fn begin_scan(&mut self, root: PathBuf, try_mft: bool, volume: Option<char>) -> Screen {
        self.message = None;
        self.fast_hint = None;
        self.last_scan = None;
        self.programs_rx = Some(cleanup_flow::start_programs());
        let (rx, preview_slot) = start_scan_job(root.clone(), try_mft);
        Screen::Scanning {
            rx,
            preview_slot,
            files: 0,
            bytes: 0,
            root,
            volume,
            started: Instant::now(),
            preparing: false,
            preview: None,
        }
    }

    fn relaunch(&mut self, letter: char) {
        // Só volta se o UAC foi recusado ou a chamada falhou; no sucesso o processo encerra.
        if let Err(e) = crate::elevation::relaunch_elevated(Some(letter)) {
            self.message = Some(format!("Não foi possível abrir como administrador: {e}"));
        }
    }

    fn volumes_screen(&mut self, ui: &mut egui::Ui) -> Screen {
        let action = volumes_view::volumes_view(
            ui,
            &self.volumes,
            self.volumes_rx.is_some(),
            self.elevated,
            self.message.as_deref(),
        );
        match action {
            Some(VolumesAction::Scan(letter)) => {
                let root = PathBuf::from(format!("{letter}:\\"));
                return self.begin_scan(root, true, Some(letter));
            }
            Some(VolumesAction::PickFolder) => {
                let picked = rfd::FileDialog::new()
                    .set_title("Analisar pasta")
                    .pick_folder();
                if let Some(dir) = picked {
                    return self.begin_scan(dir, false, None);
                }
            }
            Some(VolumesAction::Elevate(letter)) => self.relaunch(letter),
            Some(VolumesAction::Refresh) => {
                self.message = None;
                self.reload_volumes();
            }
            None => {}
        }
        Screen::Volumes
    }

    fn scanning_screen(&mut self, ui: &mut egui::Ui, screen: Screen) -> Screen {
        let Screen::Scanning {
            rx,
            preview_slot,
            mut files,
            mut bytes,
            root,
            volume,
            started,
            mut preparing,
            mut preview,
        } = screen
        else {
            return screen;
        };
        // Esvazia a fila sem bloquear: varredura e montagem do mapa rodam em outra thread.
        loop {
            match rx.try_recv() {
                Ok(ScanUpdate::Progress { files: f, bytes: b }) => (files, bytes) = (f, b),
                Ok(ScanUpdate::Preparing) => preparing = true,
                Ok(ScanUpdate::Ready(state, insights, method)) => {
                    self.last_scan = Some(ScanSummary {
                        method,
                        elapsed: started.elapsed(),
                    });
                    let walked = method == ScanMethod::Walk && !self.elevated;
                    self.fast_hint = volume.filter(|&l| walked && self.is_ntfs(l) == Some(true));
                    let programs = self.programs_rx.take();
                    return Screen::Map(Box::new(MapScreen::new(*state, insights, programs)));
                }
                Ok(ScanUpdate::Failed(e)) => {
                    self.message = Some(format!("A análise de {} falhou: {e}", root.display()));
                    return Screen::Volumes;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.message = Some(format!(
                        "A análise de {} terminou sem resultado.",
                        root.display()
                    ));
                    return Screen::Volumes;
                }
            }
        }

        if let Some(p) = preview_slot.take() {
            preview = Some(p);
        }
        let mut cancel = false;
        let heading = format!("Analisando {}", root.display());
        let counts = format!("{} arquivos · {}", format_count(files), format_bytes(bytes));
        let elapsed = format_secs(started.elapsed());
        let method = if preparing {
            "Montando o mapa…"
        } else if preview.is_some() {
            // Só a varredura de pastas manda mapa parcial.
            "Método: varredura de pastas"
        } else {
            match (volume.map(|l| self.is_ntfs(l)), self.elevated) {
                (Some(Some(true)), true) => "Método: leitura da MFT (rápida)",
                (Some(None), true) => "Método: leitura da MFT, se o disco for NTFS",
                _ => "Método: varredura de pastas",
            }
        };
        if let Some(map) = preview.as_deref_mut() {
            // Contadores e Cancelar numa barra; abaixo, o mapa parcial (só leitura).
            egui::Panel::top("varredura").show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(18.0));
                    ui.label(RichText::new(&heading).strong());
                    ui.separator();
                    ui.label(&counts);
                    ui.separator();
                    ui.label(RichText::new(&elapsed).weak());
                    ui.separator();
                    ui.label(RichText::new(method).weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancelar").clicked() {
                            cancel = true;
                        }
                    });
                });
                ui.add_space(4.0);
            });
            egui::Panel::bottom("varredura-rodape").show(ui, |ui| {
                ui.add_space(3.0);
                ui.label(
                    RichText::new("mapa parcial — ainda varrendo")
                        .weak()
                        .italics(),
                );
                ui.add_space(3.0);
            });
            egui::CentralPanel::no_frame().show(ui, |ui| map_view::preview_view(ui, map));
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.3).max(20.0));
                    ui.heading(&heading);
                    ui.add_space(12.0);
                    ui.add(egui::Spinner::new().size(32.0));
                    ui.add_space(12.0);
                    ui.label(RichText::new(&counts).size(18.0));
                    ui.label(RichText::new(&elapsed).weak());
                    ui.label(RichText::new(method).weak());
                    ui.add_space(16.0);
                    if ui.button("Cancelar").clicked() {
                        cancel = true;
                    }
                });
            });
        }
        if cancel {
            // Descartar o Receiver (junto com esta tela) avisa as threads para parar.
            return Screen::Volumes;
        }
        ui.ctx().request_repaint_after(Duration::from_millis(100));
        Screen::Scanning {
            rx,
            preview_slot,
            files,
            bytes,
            root,
            volume,
            started,
            preparing,
            preview,
        }
    }

    fn map_screen(&mut self, ui: &mut egui::Ui, mut screen: Box<MapScreen>) -> Screen {
        screen.poll(ui.ctx());
        let modal = screen.dialog_open();
        let total = suggestions::headline_total(screen.sugg.list());
        // Trocar de tela larga o MapScreen e o trabalho em andamento (desinstalação em segundo
        // plano, limpeza, reanálise): "Nova análise" espera.
        let busy = screen.busy();
        let state = &mut screen.map;
        // Atalhos: Backspace / Alt+← volta, Alt+→ avança, botões laterais do mouse. Desligados
        // com o diálogo aberto.
        let typing = modal || ui.ctx().egui_wants_keyboard_input();
        let (back, fwd) = ui.input(|i| {
            let alt = i.modifiers.alt && !modal;
            (
                (!typing && i.key_pressed(Key::Backspace))
                    || (alt && i.key_pressed(Key::ArrowLeft))
                    || (!modal && i.pointer.button_pressed(PointerButton::Extra1)),
                (alt && i.key_pressed(Key::ArrowRight))
                    || (!modal && i.pointer.button_pressed(PointerButton::Extra2)),
            )
        });
        if back {
            state.back();
        } else if fwd {
            state.forward();
        }

        // Ctrl+F abre a busca, Esc fecha.
        let now = ui.input(|i| i.time);
        if !modal && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::F)) {
            state.search.open();
        }
        if !modal && state.search.open && ui.input(|i| i.key_pressed(Key::Escape)) {
            state.search.close();
        }
        let mut found = None;
        if state.search.open {
            let s = &mut *state;
            found = s
                .search
                .poll(now, &s.search_index().clone(), &s.tree, ui.ctx());
        }

        let mut new_scan = false;
        let top = egui::Panel::top("topo").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let b = ui
                    .add_enabled(state.nav.can_back(), egui::Button::new("←"))
                    .on_hover_text("Voltar (Backspace, Alt+←)");
                if b.clicked() {
                    state.back();
                }
                let f = ui
                    .add_enabled(state.nav.can_forward(), egui::Button::new("→"))
                    .on_hover_text("Avançar (Alt+→)");
                if f.clicked() {
                    state.forward();
                }
                ui.separator();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = format!("Sugestões · {}", format_bytes(total));
                    ui.toggle_value(&mut self.sidebar_open, label)
                        .on_hover_text("Mostrar ou esconder o painel de sugestões");
                    let again = ui
                        .add_enabled(!busy, egui::Button::new("Nova análise"))
                        .on_hover_text("Voltar para a lista de discos")
                        .on_disabled_hover_text(
                            "Espere terminar o que está em andamento (limpeza, desinstalação \
                             ou atualização das sugestões)",
                        );
                    if again.clicked() && !busy {
                        new_scan = true;
                    }
                    if let Some(letter) = self.fast_hint {
                        if ui.small_button("✕").on_hover_text("Dispensar").clicked() {
                            self.fast_hint = None;
                        }
                        let fast = ui
                            .button("⚡ Varredura rápida disponível como admin")
                            .on_hover_text(format!(
                                "Reabre como administrador e analisa {letter}: pela MFT"
                            ));
                        if fast.clicked() {
                            self.relaunch(letter);
                        }
                    }
                    let depth = egui::Slider::new(&mut state.opts.max_depth, 1..=6);
                    ui.add(depth.text("profundidade"));
                    ui.separator();
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        egui::ScrollArea::horizontal()
                            .stick_to_right(true)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    if let Some(id) = nav::path_bar(ui, &state.tree, &state.nav) {
                                        state.go(id);
                                    }
                                });
                            });
                    });
                });
            });
            if state.search.open {
                ui.add_space(2.0);
                found = found.or(search::search_bar(ui, &mut state.search, now));
            }
            ui.add_space(2.0);
        });
        if state.search.open {
            let rect = top.response.rect;
            let clicked = search::results_list(
                ui.ctx(),
                rect.left_bottom(),
                rect.width(),
                &state.tree,
                &mut state.search,
            );
            found = found.or(clicked);
        }
        if let Some(id) = found {
            state.reveal(id, now);
            state.search.close();
        }

        egui::Panel::bottom("rodape").show(ui, |ui| {
            ui.add_space(3.0);
            map_view::legend(ui);
            ui.horizontal(|ui| {
                let shown = state
                    .selected
                    .filter(|&id| state.tree.node(id).alive)
                    .unwrap_or(state.nav.current());
                ui.label(map_view::footer_text(&state.tree, shown));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(s) = &self.last_scan {
                        let method = match s.method {
                            ScanMethod::Mft => "MFT",
                            ScanMethod::Walk => "pastas",
                        };
                        let text = format!("varredura: {method} · {}", format_secs(s.elapsed));
                        ui.label(RichText::new(text).weak());
                    }
                    if let Some(msg) = state.notice.as_ref().or(self.message.as_ref()) {
                        ui.colored_label(ui.visuals().warn_fg_color, msg);
                    }
                });
            });
            ui.add_space(1.0);
        });

        if self.sidebar_open {
            let elevated = self.elevated;
            egui::Panel::right("sugestoes")
                .resizable(true)
                .default_size(340.0)
                .min_size(260.0)
                .show(ui, |ui| screen.sidebar(ui, elevated));
        }

        let mut action = MapAction::None;
        egui::CentralPanel::no_frame().show(ui, |ui| {
            action = map_view::map_view(ui, &mut screen.map);
        });
        match action {
            MapAction::None => {}
            MapAction::Recycle(id) => screen.ask_recycle(id),
            MapAction::Explain(id) => {
                if screen.explain(id) {
                    self.sidebar_open = true;
                }
            }
        }
        screen.show_dialog(ui.ctx());

        if new_scan {
            self.message = None;
            self.fast_hint = None;
            self.reload_volumes();
            return Screen::Volumes;
        }
        Screen::Map(screen)
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_volumes(ui.ctx());
        let screen = std::mem::replace(&mut self.screen, Screen::Volumes);
        self.screen = match screen {
            Screen::Volumes => self.volumes_screen(ui),
            s @ Screen::Scanning { .. } => self.scanning_screen(ui, s),
            Screen::Map(screen) => self.map_screen(ui, screen),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secs_pt_br() {
        assert_eq!(format_secs(Duration::from_millis(3240)), "3,2 s");
        assert_eq!(format_secs(Duration::from_millis(0)), "0,0 s");
        assert_eq!(format_secs(Duration::from_millis(59_960)), "60,0 s");
    }

    #[test]
    fn scan_job_builds_map_off_the_ui_thread() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("Fotos de São Paulo");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("coração.jpg"), vec![0u8; 8192]).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let (rx, _slot) = start_scan_job(dir.path().to_path_buf(), false);
        let mut saw_preparing = false;
        let (state, insights, method) = loop {
            match rx.recv_timeout(Duration::from_secs(30)).expect("evento") {
                ScanUpdate::Preparing => saw_preparing = true,
                ScanUpdate::Ready(s, i, m) => break (s, i, m),
                ScanUpdate::Failed(e) => panic!("{e}"),
                ScanUpdate::Progress { .. } => {}
            }
        };
        assert!(saw_preparing, "avisa que está montando o mapa");
        assert_eq!(method, ScanMethod::Walk);
        assert_eq!(state.tree.node(state.tree.root()).files, 2);
        assert_eq!(
            state.dominant_kind(state.tree.root()),
            crate::kind::FileKind::Image
        );
        assert_eq!(
            *state.explainable(),
            suggestions::explainable_set(&insights),
            "sugestões calculadas junto com o mapa"
        );
    }

    fn tree_of(size: u64) -> Tree {
        let mut t = Tree::new("C:\\".into());
        t.add(t.root(), "f", false, size, None);
        t.preview(3)
    }

    fn preview_of(size: u64) -> ScanEvent {
        let (tree, kinds) = tree_of(size).preview_with_kinds(3);
        ScanEvent::Preview { tree, kinds }
    }

    #[test]
    fn previews_do_not_pile_up_when_ui_does_not_drain() {
        let (scan_tx, scan_rx) = mpsc::channel();
        for s in 1..=50 {
            scan_tx.send(preview_of(s)).unwrap();
        }
        drop(scan_tx);
        let (tx, rx) = mpsc::channel();
        let slot = PreviewSlot::default();
        // A UI não lê nada enquanto o repassador roda (fim: varredura descartou o canal).
        relay(scan_rx, &tx, &slot);
        assert_eq!(rx.try_iter().count(), 0, "nenhum preview na fila da UI");
        let held = slot.take().expect("um preview guardado");
        assert_eq!(held.tree().node(held.tree().root()).size, 50, "o último");
        assert!(slot.take().is_none(), "só um");
    }

    #[test]
    fn slot_keeps_only_the_latest_put() {
        let slot = PreviewSlot::default();
        for s in 1..=100 {
            let (t, k) = tree_of(s).preview_with_kinds(3);
            slot.put(PreviewMap::new(t, k));
        }
        let held = slot.take().unwrap();
        assert_eq!(held.tree().node(held.tree().root()).size, 100);
        assert!(slot.take().is_none());
    }

    #[test]
    fn relay_keeps_only_latest_preview() {
        let (tx, rx) = mpsc::channel();
        for s in [2, 3] {
            tx.send(preview_of(s)).unwrap();
        }
        tx.send(ScanEvent::Progress { files: 1, bytes: 9 }).unwrap();
        tx.send(preview_of(4)).unwrap();
        let ((t, _), next) = latest_preview((tree_of(1), Vec::new()), &rx);
        assert_eq!(t.node(t.root()).size, 3, "fica o último da sequência");
        assert!(matches!(next, Some(ScanEvent::Progress { bytes: 9, .. })));
        let ((t, _), next) = latest_preview((tree_of(0), Vec::new()), &rx);
        assert_eq!(t.node(t.root()).size, 4);
        assert!(next.is_none(), "fila vazia");
    }

    #[test]
    fn list_volumes_runs_in_background() {
        let rx = start_list_volumes();
        let v = rx.recv_timeout(Duration::from_secs(30)).expect("lista");
        assert!(v.iter().all(|v| v.letter.is_ascii_uppercase()));
    }
}
