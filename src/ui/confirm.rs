//! Diálogo de confirmação (limpeza, Lixeira, desinstalação, remoção de app da Store), execução
//! numa thread com progresso e o resumo no fim.

use crate::cleanup::{CleanupPlan, CleanupReport, Op, allocated_size, execute, recycle_bin_size};
use crate::format::{format_bytes, format_count};
use crate::insights::{Program, ProgramSource};
use crate::tree::{NodeId, Tree};
use eframe::egui::{self, RichText};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

/// Texto de uma operação na lista do diálogo.
pub fn op_label(op: &Op) -> String {
    match op {
        Op::DeletePermanent(p) | Op::Recycle(p) => p.display().to_string(),
        Op::DeleteContents(p) => format!("Conteúdo de {}", p.display()),
        Op::EmptyRecycleBin => "Esvaziar a Lixeira".to_string(),
        Op::Uninstall { name, .. } => format!("Desinstalar {name}"),
        Op::RemoveStorePackage { name, .. } => format!("Remover {name} (Microsoft Store)"),
        Op::CompactVhdx { path, .. } => format!("Compactar {}", path.display()),
        Op::DockerPrune => "Limpar o Docker (imagens, contêineres e cache sem uso)".to_string(),
    }
}

/// A operação que tira o programa: desinstalador do registro ou remoção pela Store.
pub fn uninstall_op(p: &Program) -> Op {
    match &p.source {
        ProgramSource::Registry => Op::Uninstall {
            name: p.name.clone(),
            command: p.uninstall.clone(),
        },
        ProgramSource::Store { full_name } => Op::RemoveStorePackage {
            name: p.name.clone(),
            full_name: full_name.clone(),
        },
    }
}

/// Textos do fluxo de desinstalação, conforme a origem do programa.
#[derive(Debug, PartialEq, Eq)]
pub struct UninstallTexts {
    pub title: String,
    pub body: &'static str,
    pub primary: &'static str,
    pub running: String,
    pub done: String,
}

pub fn uninstall_texts(p: &Program) -> UninstallTexts {
    let name = &p.name;
    if p.is_store() {
        UninstallTexts {
            title: format!("Remover o app {name} da Microsoft Store?"),
            body: "O Windows remove o app e os dados dele deste usuário. Dá para instalar de \
                   novo pela Microsoft Store.",
            primary: "Remover",
            running: format!("Removendo {name}…"),
            done: format!("{name} foi removido."),
        }
    } else {
        UninstallTexts {
            title: format!("Abrir o desinstalador de {name}?"),
            body: "O desinstalador do próprio programa vai abrir; siga as instruções dele. \
                   O deixa-seco não apaga a pasta do programa e espera o desinstalador terminar.",
            primary: "Abrir desinstalador",
            running: format!("Esperando o desinstalador de {name} terminar…"),
            done: format!("O desinstalador de {name} terminou."),
        }
    }
}

/// Aviso quando a desinstalação/remoção falhou (`why` em pt-BR).
pub fn uninstall_failed(p: &Program, why: &str) -> String {
    if p.is_store() {
        format!("Não consegui remover {} ({why}).", p.name)
    } else {
        format!("O desinstalador de {} não terminou bem ({why}).", p.name)
    }
}

/// O que o diálogo mostra, já separado: o que sai de vez e o que vai para a Lixeira.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ConfirmModel {
    /// Apagado de vez (inclui o que é grande demais para a Lixeira).
    pub bytes_now: u64,
    pub bytes_bin: u64,
    pub now: Vec<(String, u64)>,
    pub bin: Vec<(String, u64)>,
    /// `Recycle` maior que a Lixeira: o Windows apaga de vez.
    pub too_big: Vec<(String, u64)>,
    pub rejected: Vec<String>,
    /// Compactar e limpar o Docker: quanto liberam só se sabe depois de executar (o texto já
    /// traz o tamanho atual do disco ou a estimativa do Docker).
    pub later: Vec<String>,
    /// Avisos dessas operações (desligar o WSL/Docker, o que o prune remove).
    pub warnings: Vec<&'static str>,
    /// Observações sobre o que saiu da seleção (prune e compactação juntos).
    pub notes: Vec<&'static str>,
}

impl ConfirmModel {
    pub fn can_run(&self) -> bool {
        !(self.now.is_empty()
            && self.bin.is_empty()
            && self.too_big.is_empty()
            && self.later.is_empty())
    }
}

pub const WARN_COMPACT: [&str; 2] = [
    "Isso desliga o WSL e o Docker. Salve o que estiver aberto neles.",
    "Feche o Docker Desktop antes, se estiver aberto.",
];
pub const WARN_PRUNE: &str = "Remove imagens sem contêiner, contêineres parados e cache de \
                              build. Volumes não são tocados.";

/// Título da seção do que só se mede no fim.
pub fn later_title() -> &'static str {
    "Libera depois de executar"
}

/// Avisos da operação no diálogo de confirmação.
fn op_warnings(op: &Op) -> &'static [&'static str] {
    match op {
        Op::CompactVhdx { .. } => &WARN_COMPACT,
        Op::DockerPrune => std::slice::from_ref(&WARN_PRUNE),
        _ => &[],
    }
}

/// Por que o `plan` recusou a operação (texto curto da lista "Não vou mexer nisto:"). Para
/// compactar, os mesmos testes do `plan`: extensão, arquivo existe, proteção.
pub fn reject_reason(op: &Op) -> &'static str {
    match op {
        Op::CompactVhdx { path, .. } => {
            let lower = path.to_string_lossy().to_lowercase();
            if !(lower.ends_with(".vhdx") || lower.ends_with(".vhd")) {
                "não é disco virtual"
            } else if !path.is_file() {
                "arquivo não encontrado"
            } else {
                "pasta do sistema"
            }
        }
        _ => "pasta do sistema",
    }
}

/// Monta o modelo a partir do plano e das operações com tamanho que o originaram (na mesma
/// ordem: `plan` mantém a ordem e só tira as rejeitadas).
pub fn confirm_model(plan: &CleanupPlan, sized: &[(Op, u64)]) -> ConfirmModel {
    let too_big: HashSet<&PathBuf> = plan.too_big_for_bin.iter().collect();
    let mut m = ConfirmModel::default();
    let mut next = 0;
    for (op, size) in sized {
        if plan.ops.get(next) != Some(op) {
            m.rejected
                .push(format!("{} — {}", op_label(op), reject_reason(op)));
            continue;
        }
        next += 1;
        for w in op_warnings(op) {
            if !m.warnings.contains(w) {
                m.warnings.push(w);
            }
        }
        let item = (op_label(op), *size);
        match op {
            Op::CompactVhdx { .. } => m.later.push(format!(
                "{} — tamanho atual {}",
                item.0,
                format_bytes(*size)
            )),
            Op::DockerPrune => {
                m.later
                    .push(format!("{} — estimativa {}", item.0, format_bytes(*size)))
            }
            Op::Recycle(p) if too_big.contains(p) => {
                m.bytes_now += size;
                m.too_big.push(item);
            }
            Op::Recycle(_) => {
                m.bytes_bin += size;
                m.bin.push(item);
            }
            _ => {
                m.bytes_now += size;
                m.now.push(item);
            }
        }
    }
    m
}

/// "Liberado X · Y na Lixeira · N itens pulados" (partes zeradas ficam de fora, menos a
/// primeira).
pub fn summary_line(r: &CleanupReport) -> String {
    let mut parts = vec![format!("Liberado {}", format_bytes(r.freed_now))];
    if r.sent_to_bin > 0 {
        parts.push(format!("{} na Lixeira", format_bytes(r.sent_to_bin)));
    }
    match r.skipped.len() {
        0 => {}
        1 => parts.push("1 item pulado".to_string()),
        n => parts.push(format!("{} itens pulados", format_count(n as u64))),
    }
    parts.join(" · ")
}

/// Tira da árvore o que saiu do disco. Para `DeleteContents`, `done` tem os filhos que saíram
/// (a pasta fica, e o que foi pulado continua nela). Entradas que não são caminhos (Lixeira,
/// nome de programa) são ignoradas. Devolve quantos nós saíram.
/// Usado como referência nos testes (a UI usa `resolve_done` + `Tree::remove_many`).
pub fn apply_done(tree: &mut Tree, report: &CleanupReport) -> usize {
    let ids = resolve_done(tree, report);
    tree.remove_many(&ids);
    ids.len()
}

/// Nós vivos dos caminhos de `done`, sem mudar a árvore (vazio = nada a fazer nela). Agrupa
/// por pasta-pai: cada pasta é achada uma vez e os filhos dela vão para um mapa por nome em
/// minúsculas, então uma `Temp` com dezenas de milhares de itens continua linear.
pub fn resolve_done(tree: &Tree, report: &CleanupReport) -> Vec<NodeId> {
    let mut by_parent: HashMap<&Path, Vec<String>> = HashMap::new();
    for p in report.done.iter().filter(|p| p.is_absolute()) {
        if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
            by_parent
                .entry(parent)
                .or_default()
                .push(name.to_string_lossy().to_lowercase());
        }
    }
    let mut out = Vec::new();
    for (parent, names) in by_parent {
        let Some(dir) = tree.find(parent) else {
            continue;
        };
        let kids: HashMap<String, NodeId> = tree
            .children(dir)
            .iter()
            .map(|&k| (tree.node(k).name.to_lowercase(), k))
            .collect();
        out.extend(names.iter().filter_map(|n| kids.get(n).copied()));
    }
    out.sort_unstable_by_key(|n| n.0);
    out.dedup();
    out
}

/// Disco do plano que passou pela compactação e o tamanho alocado dele depois (`None`: não
/// deu para ler).
pub type Resized = (PathBuf, Option<u64>);

pub enum JobMsg {
    /// (feitas, total)
    Progress(usize, usize),
    /// Relatório, o tamanho da Lixeira depois (0 se não foi medido) e os discos compactados
    /// com o tamanho novo.
    Done(CleanupReport, u64, Vec<Resized>),
}

/// Tamanho de cada disco do plano depois de compactar (deu certo ou não: só relê o arquivo).
pub fn measure_compacted(
    plan: &CleanupPlan,
    measure: impl Fn(&Path) -> Option<u64>,
) -> Vec<Resized> {
    plan.ops
        .iter()
        .filter_map(|op| match op {
            Op::CompactVhdx { path, .. } => Some((path.clone(), measure(path))),
            _ => None,
        })
        .collect()
}

/// Executa o plano numa thread. Com `measure_bin`, mede a Lixeira no fim (para a oferta de
/// esvaziar). Relê o tamanho dos discos compactados (para o mapa, sem reanalisar).
pub fn start_job(plan: CleanupPlan, measure_bin: bool) -> Receiver<JobMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let report = execute(&plan, &mut |done, total| {
            let _ = tx.send(JobMsg::Progress(done, total));
        });
        let bin = if measure_bin { recycle_bin_size() } else { 0 };
        let resized = measure_compacted(&plan, allocated_size);
        let _ = tx.send(JobMsg::Done(report, bin, resized));
    });
    rx
}

#[derive(Clone, Debug)]
pub enum Kind {
    Cleanup,
    EmptyBin,
    Uninstall(Program),
}

pub enum Dialog {
    Confirm {
        kind: Kind,
        plan: CleanupPlan,
        model: ConfirmModel,
        /// O foco já foi para "Cancelar".
        focused: bool,
    },
    Running {
        kind: Kind,
        rx: Receiver<JobMsg>,
        done: usize,
        total: usize,
    },
    Result {
        kind: Kind,
        report: CleanupReport,
        bin_after: u64,
        show_skipped: bool,
        focused: bool,
    },
}

impl Dialog {
    pub fn confirm(kind: Kind, plan: CleanupPlan, sized: &[(Op, u64)]) -> Dialog {
        Self::confirm_with_notes(kind, plan, sized, Vec::new())
    }

    pub fn confirm_with_notes(
        kind: Kind,
        plan: CleanupPlan,
        sized: &[(Op, u64)],
        notes: Vec<&'static str>,
    ) -> Dialog {
        let mut model = confirm_model(&plan, sized);
        model.notes = notes;
        Dialog::Confirm {
            kind,
            plan,
            model,
            focused: false,
        }
    }
}

/// O que o diálogo pede para a tela do mapa fazer.
pub enum Outcome {
    Keep,
    Close,
    /// "Limpar"/"Esvaziar"/"Abrir desinstalador": rodar o plano.
    Run,
    /// "A Lixeira tem Z. Esvaziar agora?"
    OfferEmptyBin,
    /// "Continuar em segundo plano" (desinstalação rodando).
    Background,
}

/// Cliques no fundo escuro só fecham o diálogo depois deste tempo aberto (um duplo clique em
/// "Limpar selecionados" não pode fechá-lo na hora).
pub const BACKDROP_GRACE: std::time::Duration = std::time::Duration::from_millis(300);

fn item_list(ui: &mut egui::Ui, id: &str, items: &[(String, u64)]) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(150.0)
        .show(ui, |ui| {
            for (label, size) in items {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format_bytes(*size)).monospace());
                    ui.label(label);
                });
            }
        });
}

fn section(ui: &mut egui::Ui, id: &str, title: String, items: &[(String, u64)]) {
    if items.is_empty() {
        return;
    }
    egui::CollapsingHeader::new(RichText::new(title).strong())
        .id_salt(id)
        .default_open(items.len() <= 5)
        .show(ui, |ui| item_list(ui, id, items));
}

/// Botões no rodapé, da direita para a esquerda: `primary` e depois "Cancelar" (que recebe o
/// foco no primeiro quadro). Devolve (cancelar, primário).
fn buttons(ui: &mut egui::Ui, primary: &str, enabled: bool, focused: &mut bool) -> (bool, bool) {
    let mut out = (false, false);
    ui.add_space(8.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let go = ui.add_enabled(enabled, egui::Button::new(RichText::new(primary).strong()));
        let cancel = ui.button("Cancelar");
        if !*focused {
            cancel.request_focus();
            *focused = true;
        }
        out = (cancel.clicked(), go.clicked());
    });
    out
}

fn confirm_body(
    ui: &mut egui::Ui,
    kind: &Kind,
    plan: &CleanupPlan,
    model: &ConfirmModel,
    focused: &mut bool,
) -> Outcome {
    let warn = ui.visuals().warn_fg_color;
    let primary = match kind {
        Kind::Cleanup => {
            ui.heading("Confirmar limpeza");
            ui.add_space(6.0);
            let free_now: Vec<(String, u64)> =
                model.now.iter().chain(&model.too_big).cloned().collect();
            section(
                ui,
                "agora",
                format!(
                    "Libera agora: {} (apagado de vez)",
                    format_bytes(model.bytes_now)
                ),
                &free_now,
            );
            section(
                ui,
                "lixeira",
                format!("Vai para a Lixeira: {}", format_bytes(model.bytes_bin)),
                &model.bin,
            );
            if !model.later.is_empty() {
                egui::CollapsingHeader::new(RichText::new(later_title()).strong())
                    .id_salt("depois")
                    .default_open(true)
                    .show(ui, |ui| {
                        for l in &model.later {
                            ui.label(l);
                        }
                    });
            }
            for w in &model.warnings {
                ui.colored_label(warn, format!("⚠ {w}"));
            }
            for n in &model.notes {
                ui.add_space(4.0);
                ui.label(RichText::new(*n).strong());
            }
            if !model.too_big.is_empty() {
                ui.add_space(6.0);
                ui.colored_label(
                    warn,
                    "⚠ Estes itens são grandes demais para a Lixeira e serão apagados de vez:",
                );
                item_list(ui, "grandes", &model.too_big);
            }
            if !model.rejected.is_empty() {
                ui.add_space(6.0);
                ui.label(RichText::new("Não vou mexer nisto:").strong());
                egui::ScrollArea::vertical()
                    .id_salt("rejeitados")
                    .max_height(100.0)
                    .show(ui, |ui| {
                        for r in &model.rejected {
                            ui.label(RichText::new(r).weak());
                        }
                    });
            }
            if !model.can_run() {
                ui.add_space(6.0);
                ui.label("Não sobrou nada que eu possa limpar.");
            }
            "Limpar"
        }
        Kind::EmptyBin => {
            ui.heading("Esvaziar a Lixeira?");
            ui.add_space(6.0);
            ui.label(format!(
                "{} serão apagados de vez, de todas as unidades. Não dá para desfazer.",
                format_bytes(plan.bytes_now)
            ));
            "Esvaziar"
        }
        Kind::Uninstall(p) => {
            let t = uninstall_texts(p);
            ui.heading(t.title);
            ui.add_space(6.0);
            ui.label(t.body);
            t.primary
        }
    };
    let (cancel, go) = buttons(ui, primary, model.can_run(), focused);
    if cancel {
        Outcome::Close
    } else if go {
        Outcome::Run
    } else {
        Outcome::Keep
    }
}

fn running_body(ui: &mut egui::Ui, kind: &Kind, done: usize, total: usize) -> Outcome {
    let title = match kind {
        Kind::Cleanup => "Limpando…".to_string(),
        Kind::EmptyBin => "Esvaziando a Lixeira…".to_string(),
        Kind::Uninstall(p) => uninstall_texts(p).running,
    };
    ui.heading(title);
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.spinner();
        if total > 1 {
            ui.label(format!(
                "{} de {}",
                format_count(done as u64),
                format_count(total as u64)
            ));
        }
    });
    if total > 1 {
        ui.add(egui::ProgressBar::new(done as f32 / total as f32));
    }
    if let Kind::Uninstall(_) = kind {
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Dá para continuar usando o deixa-seco; o resultado aparece no rodapé. \
                 Outras limpezas ficam bloqueadas até ele terminar.",
            )
            .weak(),
        );
        let mut out = Outcome::Keep;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Continuar em segundo plano").clicked() {
                out = Outcome::Background;
            }
        });
        return out;
    }
    Outcome::Keep
}

fn result_body(
    ui: &mut egui::Ui,
    kind: &Kind,
    report: &CleanupReport,
    bin_after: u64,
    show_skipped: &mut bool,
    focused: &mut bool,
) -> Outcome {
    let mut out = Outcome::Keep;
    match kind {
        Kind::Uninstall(p) => {
            ui.heading(if p.is_store() {
                "Remoção"
            } else {
                "Desinstalação"
            });
            ui.add_space(6.0);
            match report.skipped.first() {
                None => ui.label(uninstall_texts(p).done),
                Some((_, why)) => ui.label(format!(
                    "{} Nada foi mudado no mapa.",
                    uninstall_failed(p, why)
                )),
            };
        }
        Kind::Cleanup | Kind::EmptyBin => {
            ui.heading("Pronto");
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(summary_line(report)).strong());
                for n in &report.notes {
                    ui.label(n);
                }
                if !report.skipped.is_empty() {
                    let text = if *show_skipped { "(esconder)" } else { "(ver)" };
                    if ui.link(text).clicked() {
                        *show_skipped = !*show_skipped;
                    }
                }
            });
            if *show_skipped {
                egui::ScrollArea::vertical()
                    .id_salt("pulados")
                    .max_height(180.0)
                    .show(ui, |ui| {
                        for (path, why) in &report.skipped {
                            ui.label(format!("{} — {why}", path.display()));
                        }
                    });
            }
            if matches!(kind, Kind::Cleanup) && bin_after > 0 {
                ui.add_space(8.0);
                let b = ui.button(format!(
                    "A Lixeira tem {}. Esvaziar agora?",
                    format_bytes(bin_after)
                ));
                if b.clicked() {
                    out = Outcome::OfferEmptyBin;
                }
            }
        }
    }
    ui.add_space(8.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let close = ui.button("Fechar");
        if !*focused {
            close.request_focus();
            *focused = true;
        }
        if close.clicked() {
            out = Outcome::Close;
        }
    });
    out
}

/// Desenha o diálogo (modal: o resto da tela escurece e não recebe cliques). Esc ou clique
/// fora cancela/fecha, menos enquanto roda. `allow_backdrop`: o diálogo está aberto há mais de
/// `BACKDROP_GRACE`.
pub fn show(
    ctx: &egui::Context,
    dialog: &mut Dialog,
    generation: u64,
    allow_backdrop: bool,
) -> Outcome {
    let running = matches!(dialog, Dialog::Running { .. });
    if !allow_backdrop {
        // Nos primeiros instantes o foco volta para o botão padrão mesmo que um clique de
        // sobra (o segundo do duplo clique) o tire.
        if let Dialog::Confirm { focused, .. } | Dialog::Result { focused, .. } = dialog {
            *focused = false;
        }
    }
    let modal = egui::Modal::new(egui::Id::unique(("confirmacao", generation))).show(ctx, |ui| {
        ui.set_min_width(380.0);
        ui.set_max_width(560.0);
        match dialog {
            Dialog::Confirm {
                kind,
                plan,
                model,
                focused,
            } => confirm_body(ui, kind, plan, model, focused),
            Dialog::Running {
                kind, done, total, ..
            } => running_body(ui, kind, *done, *total),
            Dialog::Result {
                kind,
                report,
                bin_after,
                show_skipped,
                focused,
            } => result_body(ui, kind, report, *bin_after, show_skipped, focused),
        }
    });
    if running {
        return modal.inner;
    }
    let backdrop = allow_backdrop && modal.backdrop_response.clicked();
    let escape = modal.is_top_modal
        && !modal.any_popup_open
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    match modal.inner {
        Outcome::Keep if backdrop || escape => Outcome::Close,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::{CleanupPlan, CleanupReport, Op};
    use crate::tree::Tree;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn model_moves_too_big_items_to_free_now() {
        let sized = vec![
            (Op::DeleteContents(p(r"C:\T")), 30),
            (Op::Recycle(p(r"C:\Windows\x")), 5),
            (Op::Recycle(p(r"C:\small")), 10),
            (Op::Recycle(p(r"C:\huge")), 1000),
        ];
        let plan = CleanupPlan {
            ops: vec![
                Op::DeleteContents(p(r"C:\T")),
                Op::Recycle(p(r"C:\small")),
                Op::Recycle(p(r"C:\huge")),
            ],
            bytes_now: 30,
            bytes_to_bin: 1010,
            too_big_for_bin: vec![p(r"C:\huge")],
            rejected: vec![p(r"C:\Windows\x")],
        };
        let m = confirm_model(&plan, &sized);
        assert_eq!(m.bytes_now, 1030, "o grande demais é apagado de vez");
        assert_eq!(m.bytes_bin, 10);
        assert_eq!(m.now, vec![(r"Conteúdo de C:\T".to_string(), 30)]);
        assert_eq!(m.bin, vec![(r"C:\small".to_string(), 10)]);
        assert_eq!(m.too_big, vec![(r"C:\huge".to_string(), 1000)]);
        assert_eq!(
            m.rejected,
            vec![r"C:\Windows\x — pasta do sistema".to_string()]
        );
        assert!(m.can_run());
    }

    #[test]
    fn compact_and_prune_free_after_running_with_warnings() {
        let disk = Op::CompactVhdx {
            path: p(r"C:\WSL\ext4.vhdx"),
            distro: Some("Ubuntu".into()),
        };
        let sized = vec![
            (Op::DeleteContents(p(r"C:\T")), 30),
            (Op::DockerPrune, 70),
            (disk.clone(), 6 * 1024 * 1024 * 1024),
        ];
        let plan = CleanupPlan {
            ops: vec![Op::DeleteContents(p(r"C:\T")), Op::DockerPrune, disk],
            bytes_now: 100,
            ..Default::default()
        };
        let m = confirm_model(&plan, &sized);
        assert_eq!(
            m.bytes_now, 30,
            "nem o prune nem a compactação têm número certo"
        );
        assert_eq!(m.now, vec![(r"Conteúdo de C:\T".to_string(), 30)]);
        assert_eq!(
            m.later,
            vec![
                "Limpar o Docker (imagens, contêineres e cache sem uso) — estimativa 70 B"
                    .to_string(),
                r"Compactar C:\WSL\ext4.vhdx — tamanho atual 6,0 GB".to_string(),
            ]
        );
        assert_eq!(
            m.warnings,
            vec![
                "Remove imagens sem contêiner, contêineres parados e cache de build. Volumes não são tocados.",
                "Isso desliga o WSL e o Docker. Salve o que estiver aberto neles.",
                "Feche o Docker Desktop antes, se estiver aberto.",
            ]
        );
        assert!(m.can_run());
        // Só compactar: ainda dá para rodar.
        let only = vec![sized[2].clone()];
        let plan = CleanupPlan {
            ops: vec![only[0].0.clone()],
            ..Default::default()
        };
        let m = confirm_model(&plan, &only);
        assert!(m.can_run());
        assert!(m.now.is_empty());
        assert_eq!(m.warnings.len(), 2);
        assert_eq!(later_title(), "Libera depois de executar");
    }

    #[test]
    fn rejected_compaction_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("ext4.vhdx");
        std::fs::write(&real, b"").unwrap();
        let op = |p: PathBuf| Op::CompactVhdx {
            path: p,
            distro: None,
        };
        assert_eq!(
            reject_reason(&op(p(r"C:\x\disco.vmdk"))),
            "não é disco virtual"
        );
        assert_eq!(
            reject_reason(&op(dir.path().join("sumiu.vhdx"))),
            "arquivo não encontrado"
        );
        assert_eq!(reject_reason(&op(real)), "pasta do sistema");
        assert_eq!(
            reject_reason(&Op::Recycle(p(r"C:\Windows"))),
            "pasta do sistema"
        );
        let sized = vec![(op(p(r"C:\x\disco.vmdk")), 5)];
        let m = confirm_model(&CleanupPlan::default(), &sized);
        assert_eq!(
            m.rejected,
            vec![r"Compactar C:\x\disco.vmdk — não é disco virtual".to_string()]
        );
    }

    #[test]
    fn model_with_everything_rejected_cannot_run() {
        let sized = vec![(Op::Recycle(p(r"C:\Windows")), 5)];
        let plan = CleanupPlan {
            rejected: vec![p(r"C:\Windows")],
            ..Default::default()
        };
        let m = confirm_model(&plan, &sized);
        assert!(!m.can_run());
        assert_eq!(m.rejected.len(), 1);
    }

    #[test]
    fn op_labels_pt_br() {
        assert_eq!(op_label(&Op::EmptyRecycleBin), "Esvaziar a Lixeira");
        assert_eq!(
            op_label(&Op::Uninstall {
                name: "Foo".into(),
                command: "x".into()
            }),
            "Desinstalar Foo"
        );
        assert_eq!(op_label(&Op::DeletePermanent(p(r"C:\a"))), r"C:\a");
        assert_eq!(
            op_label(&Op::RemoveStorePackage {
                name: "Bloco".into(),
                full_name: "x".into()
            }),
            "Remover Bloco (Microsoft Store)"
        );
    }

    fn program(store: bool) -> Program {
        Program {
            name: "Bloco de Notas".into(),
            publisher: String::new(),
            size: None,
            installed: None,
            uninstall: if store { String::new() } else { "u.exe".into() },
            location: None,
            source: if store {
                ProgramSource::Store {
                    full_name: "Acme.Bloco_1_x64__abc".into(),
                }
            } else {
                ProgramSource::Registry
            },
        }
    }

    #[test]
    fn store_confirm_text() {
        let t = uninstall_texts(&program(true));
        assert_eq!(t.title, "Remover o app Bloco de Notas da Microsoft Store?");
        assert_eq!(t.primary, "Remover");
        assert!(t.running.starts_with("Removendo Bloco de Notas"));
        let r = uninstall_texts(&program(false));
        assert_eq!(r.title, "Abrir o desinstalador de Bloco de Notas?");
        assert_eq!(r.primary, "Abrir desinstalador");
    }

    #[test]
    fn uninstall_op_by_source() {
        assert_eq!(
            uninstall_op(&program(true)),
            Op::RemoveStorePackage {
                name: "Bloco de Notas".into(),
                full_name: "Acme.Bloco_1_x64__abc".into()
            }
        );
        assert_eq!(
            uninstall_op(&program(false)),
            Op::Uninstall {
                name: "Bloco de Notas".into(),
                command: "u.exe".into()
            }
        );
    }

    #[test]
    fn summary_line_pt_br() {
        let mut r = CleanupReport {
            freed_now: 3 * 1024 * 1024,
            sent_to_bin: 0,
            ..Default::default()
        };
        assert_eq!(summary_line(&r), "Liberado 3,0 MB");
        r.sent_to_bin = 1024;
        r.skipped = vec![(p("a"), "em uso".into())];
        assert_eq!(
            summary_line(&r),
            "Liberado 3,0 MB · 1,0 KB na Lixeira · 1 item pulado"
        );
        r.skipped.push((p("b"), "em uso".into()));
        assert!(summary_line(&r).ends_with("2 itens pulados"));
    }

    /// C:\ { T\ {a 10, b 20}, x 5 }
    fn tree() -> Tree {
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let d = t.add(r, "T", true, 0, None);
        t.add(d, "a", false, 10, None);
        t.add(d, "b", false, 20, None);
        t.add(r, "x", false, 5, None);
        t.finalize();
        t
    }

    #[test]
    fn apply_done_removes_only_what_left() {
        let mut t = tree();
        let report = CleanupReport {
            // DeleteContents(C:\T): `a` saiu, `b` foi pulado; Lixeira e programa não são nós.
            done: vec![p(r"C:\T\a"), p(r"C:\x"), p("Lixeira"), p("Foo")],
            skipped: vec![(p(r"C:\T\b"), "em uso".into())],
            ..Default::default()
        };
        assert_eq!(apply_done(&mut t, &report), 2);
        let dir = t.find(Path::new(r"C:\T")).expect("a pasta fica");
        assert_eq!(t.node(dir).size, 20);
        assert!(t.find(Path::new(r"C:\T\b")).is_some());
        assert!(t.find(Path::new(r"C:\x")).is_none());
        assert_eq!(t.node(t.root()).size, 20);
    }

    #[test]
    fn resolve_done_is_read_only_and_handles_case() {
        let t = tree();
        let report = CleanupReport {
            done: vec![p(r"c:\t\A"), p(r"C:\nada\z"), p("Lixeira")],
            ..Default::default()
        };
        let ids = resolve_done(&t, &report);
        assert_eq!(ids, vec![t.find(Path::new(r"C:\T\a")).unwrap()]);
        let none = CleanupReport {
            done: vec![p("Lixeira")],
            ..Default::default()
        };
        assert!(
            resolve_done(&t, &none).is_empty(),
            "nada para mudar na árvore"
        );
    }

    #[test]
    fn apply_done_flat_temp_with_20k_entries() {
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let d = t.add(r, "Temp", true, 0, None);
        for i in 0..20_000 {
            t.add(d, &format!("arquivo {i}.tmp"), false, 4096, None);
        }
        t.finalize();
        let report = CleanupReport {
            done: (0..19_990)
                .map(|i| PathBuf::from(format!(r"C:\Temp\arquivo {i}.tmp")))
                .collect(),
            ..Default::default()
        };
        let start = std::time::Instant::now();
        assert_eq!(apply_done(&mut t, &report), 19_990);
        let took = start.elapsed();
        assert_eq!(t.children(d).len(), 10);
        assert_eq!(t.node(t.root()).size, 10 * 4096);
        assert!(took.as_secs() < 5, "aplicar o resultado é linear: {took:?}");
    }

    #[test]
    fn job_runs_off_thread_and_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("lixo de São Paulo.tmp");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        let plan = CleanupPlan {
            ops: vec![Op::DeletePermanent(f.clone())],
            bytes_now: 4096,
            ..Default::default()
        };
        let rx = start_job(plan, false);
        let mut progress = Vec::new();
        let (report, bin) = loop {
            match rx.recv_timeout(Duration::from_secs(30)).expect("mensagem") {
                JobMsg::Progress(d, t) => progress.push((d, t)),
                JobMsg::Done(r, b, resized) => {
                    assert!(resized.is_empty(), "nada compactado");
                    break (r, b);
                }
            }
        };
        assert_eq!(progress.first(), Some(&(0, 1)));
        assert_eq!(progress.last(), Some(&(1, 1)));
        assert_eq!(report.done, vec![f.clone()]);
        assert!(!f.exists());
        assert_eq!(bin, 0, "sem medir a Lixeira");
    }
}
