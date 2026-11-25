//! Painel de sugestões (à direita, recolhível): lista os insights, deixa o usuário marcar o
//! que limpar (nada vem marcado) e monta as operações para o diálogo de confirmação.

use super::MapState;
use super::search::shorten_middle;
use super::suggestions::{
    Pick, Suggestions, can_pick, checkable, clean_label, disk_size_text, headline_total,
    hint_segments, item_size, items, relative_label, resolve_docker_conflict, selected_ops,
    size_text,
};
use crate::format::format_bytes;
use crate::insights::{Action, Insight, Program, Risk};
use eframe::egui::{self, Color32, RichText};
use eframe::epaint::text::{LayoutJob, TextFormat};
use std::collections::HashSet;

/// Itens de uma categoria Revisar mostrados de uma vez (o resto entra pelo "marcar todos").
const MAX_ITEMS_SHOWN: usize = 200;

/// Texto curto da etiqueta de risco. Uma etiqueta colorida, não um ícone: um ✅ ao lado do
/// checkbox vazio parecia "já marcado".
pub fn risk_short(r: Risk) -> &'static str {
    match r {
        Risk::Seguro => "Seguro",
        Risk::Revisar => "Revisar",
        Risk::Informativo => "Info",
    }
}

fn risk_hint(r: Risk) -> &'static str {
    match r {
        Risk::Seguro => "Seguro: se regenera sozinho (cache, temporário)",
        Risk::Revisar => "Revisar: são arquivos seus; confira antes de limpar",
        Risk::Informativo => "Informativo: ocupa espaço, mas não se apaga por aqui",
    }
}

/// Etiqueta de largura fixa (os títulos ficam alinhados).
fn risk_badge(ui: &mut egui::Ui, r: Risk) -> egui::Response {
    let color = risk_color(r, ui.visuals().dark_mode);
    let size = egui::vec2(54.0, ui.spacing().interact_size.y - 4.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        rect.height() / 2.0,
        color.gamma_multiply(0.18),
        egui::Stroke::new(1.0, color),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        risk_short(r),
        egui::FontId::proportional(11.0),
        color,
    );
    resp.on_hover_text(risk_hint(r))
}

fn risk_color(r: Risk, dark: bool) -> Color32 {
    match (r, dark) {
        (Risk::Seguro, true) => Color32::from_rgb(110, 200, 120),
        (Risk::Seguro, false) => Color32::from_rgb(30, 130, 50),
        (Risk::Revisar, true) => Color32::from_rgb(240, 180, 60),
        (Risk::Revisar, false) => Color32::from_rgb(170, 100, 0),
        (Risk::Informativo, true) => Color32::from_rgb(120, 170, 240),
        (Risk::Informativo, false) => Color32::from_rgb(40, 90, 180),
    }
}

/// Estado do painel durante a sessão do mapa.
#[derive(Default)]
pub struct SidebarState {
    pub picks: HashSet<Pick>,
    expanded: HashSet<&'static str>,
    /// Rolar até esta sugestão no próximo quadro.
    scroll_to: Option<&'static str>,
    /// Sugestão cujos nós estão destacados no mapa.
    focused: Option<&'static str>,
}

impl SidebarState {
    /// Abre a sugestão, rola até ela e destaca os nós dela no mapa.
    pub fn reveal(&mut self, ins: &Insight, map: &mut MapState) {
        self.expanded.insert(ins.id);
        self.scroll_to = Some(ins.id);
        self.focus(ins, map);
    }

    fn focus(&mut self, ins: &Insight, map: &mut MapState) {
        self.focused = Some(ins.id);
        map.highlight = ins.nodes.iter().copied().collect();
    }

    /// Esquece marcações de itens que não existem mais.
    pub fn retain_valid(&mut self, list: &[Insight]) {
        self.picks.retain(|p| match *p {
            Pick::Whole(id) => list.iter().any(|i| i.id == id),
            Pick::Item(id, n) => list.iter().any(|i| i.id == id && i.nodes.contains(&n)),
        });
        if let Some(f) = self.focused
            && !list.iter().any(|i| i.id == f)
        {
            self.focused = None;
        }
    }

    fn category_state(&self, ins: &Insight) -> (bool, bool) {
        if per_item(ins) {
            let all = items(ins);
            let n = all
                .iter()
                .filter(|&&n| self.picks.contains(&Pick::Item(ins.id, n)))
                .count();
            (n > 0 && n == all.len(), n > 0 && n < all.len())
        } else {
            (self.picks.contains(&Pick::Whole(ins.id)), false)
        }
    }

    fn set_category(&mut self, ins: &Insight, on: bool) {
        let picks: Vec<Pick> = if per_item(ins) {
            items(ins)
                .into_iter()
                .map(|n| Pick::Item(ins.id, n))
                .collect()
        } else {
            vec![Pick::Whole(ins.id)]
        };
        for p in picks {
            if on {
                self.picks.insert(p);
            } else {
                self.picks.remove(&p);
            }
        }
    }
}

/// A categoria se marca item a item (Lixeira, discos a compactar).
fn per_item(ins: &Insight) -> bool {
    matches!(ins.action, Action::Recycle | Action::CompactVhdx)
}

/// Nota curta embaixo da explicação, conforme a ação.
pub fn action_note(ins: &Insight) -> Option<&'static str> {
    match ins.action {
        Action::DeleteContents => Some("Apagado de vez (não vai para a Lixeira)."),
        Action::Recycle => Some("Vão para a Lixeira."),
        Action::CompactVhdx => Some(
            "Compactar libera o espaço vazio de dentro do disco; quanto, só se sabe no fim. \
             Desliga o WSL e o Docker enquanto roda.",
        ),
        Action::DockerPrune => Some(
            "Remove imagens sem contêiner, contêineres parados e cache de build. \
             Volumes não são tocados.",
        ),
        Action::EmptyRecycleBin | Action::Uninstall { .. } | Action::None => None,
    }
}

/// O que o painel pede para a tela do mapa fazer.
pub enum SidebarAction {
    None,
    /// "Limpar selecionados".
    Clean,
    Uninstall(Program),
}

fn hint_job(ui: &egui::Ui, hint: &str) -> LayoutJob {
    let mut job = LayoutJob::default();
    let color = ui.visuals().text_color();
    let body = egui::TextStyle::Body.resolve(ui.style());
    let mono = egui::TextStyle::Monospace.resolve(ui.style());
    for (text, code) in hint_segments(hint) {
        let format = if code {
            TextFormat {
                font_id: mono.clone(),
                color,
                background: ui.visuals().code_bg_color,
                ..Default::default()
            }
        } else {
            TextFormat::simple(body.clone(), color)
        };
        job.append(text, 0.0, format);
    }
    job.wrap.max_width = ui.available_width();
    job
}

/// Desenha o painel. `busy`: uma limpeza ou desinstalação está rodando.
pub fn sidebar(
    ui: &mut egui::Ui,
    sugg: &Suggestions,
    st: &mut SidebarState,
    map: &mut MapState,
    elevated: bool,
    busy: bool,
) -> SidebarAction {
    let list = sugg.list();
    let mut action = SidebarAction::None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.heading(format!(
            "Sugestões · {}",
            format_bytes(headline_total(list))
        ));
    });
    ui.add_space(4.0);

    let (ops, _) = resolve_docker_conflict(selected_ops(list, &st.picks, &map.tree, elevated));
    egui::Panel::bottom("sugestoes_rodape")
        .show_separator_line(true)
        .show(ui, |ui| {
            ui.add_space(6.0);
            let label = clean_label(&ops);
            let b = ui
                .add_enabled(
                    !ops.is_empty() && !busy,
                    egui::Button::new(RichText::new(label).strong())
                        .min_size(egui::vec2(ui.available_width(), 28.0)),
                )
                .on_disabled_hover_text(if busy {
                    "Espere a operação em andamento terminar"
                } else {
                    "Marque o que quer limpar"
                });
            if b.clicked() {
                action = SidebarAction::Clean;
            }
            ui.add_space(4.0);
        });

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if list.is_empty() && !sugg.loading_programs() {
                ui.label(RichText::new("Nada para sugerir aqui.").weak());
            }
            for ins in list {
                if let Some(a) = insight_row(ui, ins, st, map, elevated, busy) {
                    action = a;
                }
                ui.add_space(2.0);
            }
            if sugg.loading_programs() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("carregando programas…").weak());
                });
            }
        });
    action
}

fn insight_row(
    ui: &mut egui::Ui,
    ins: &Insight,
    st: &mut SidebarState,
    map: &mut MapState,
    elevated: bool,
    busy: bool,
) -> Option<SidebarAction> {
    let expanded = st.expanded.contains(ins.id);
    let allowed = can_pick(ins, elevated);
    let mut out = None;
    let row = ui.horizontal(|ui| {
        if checkable(ins) {
            let (mut on, partial) = st.category_state(ins);
            let cb = ui.add_enabled(
                allowed && !busy,
                egui::Checkbox::without_text(&mut on).indeterminate(partial),
            );
            let cb = if allowed {
                cb
            } else {
                cb.on_disabled_hover_text("Abra o deixa-seco como administrador para limpar isto")
            };
            if cb.changed() {
                st.set_category(ins, on);
            }
        } else {
            ui.add_space(ui.spacing().interact_size.y + ui.spacing().item_spacing.x);
        }
        risk_badge(ui, ins.risk);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(size_text(ins)).strong());
            let arrow = if expanded { "▾" } else { "▸" };
            // O título ocupa o resto da linha, alinhado à esquerda.
            let title = ui
                .with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Button::selectable(
                            st.focused == Some(ins.id),
                            format!("{arrow} {}", ins.title),
                        )
                        .truncate(),
                    )
                })
                .inner
                .on_hover_text("Mostrar no mapa e explicar");
            if title.clicked() {
                if expanded {
                    st.expanded.remove(ins.id);
                    st.focused = None;
                    map.highlight.clear();
                } else {
                    st.expanded.insert(ins.id);
                    st.focus(ins, map);
                }
            }
        });
    });
    if st.scroll_to == Some(ins.id) {
        row.response.scroll_to_me(Some(egui::Align::TOP));
        st.scroll_to = None;
    }
    if !allowed {
        ui.indent(("admin", ins.id), |ui| {
            ui.label(
                RichText::new("precisa de admin")
                    .small()
                    .color(ui.visuals().warn_fg_color),
            );
        });
    }
    if !st.expanded.contains(ins.id) {
        return None;
    }
    ui.indent(("detalhe", ins.id), |ui| {
        ui.label(RichText::new(&ins.explanation).weak());
        if let Some(h) = &ins.hint {
            ui.label(hint_job(ui, h));
        }
        match ins.action {
            Action::Recycle => item_list(ui, ins, st, map, allowed && !busy),
            Action::CompactVhdx => disk_list(ui, ins, st, map, allowed && !busy),
            _ => {}
        }
        if let Some(note) = action_note(ins) {
            ui.label(RichText::new(note).small());
        }
        if !ins.programs.is_empty() {
            out = program_list(ui, ins, busy, elevated);
        }
    });
    out
}

fn item_list(
    ui: &mut egui::Ui,
    ins: &Insight,
    st: &mut SidebarState,
    map: &mut MapState,
    enabled: bool,
) {
    let mut reveal = None;
    let all = items(ins);
    for &n in all.iter().take(MAX_ITEMS_SHOWN) {
        let pick = Pick::Item(ins.id, n);
        let full = map.tree.path(n);
        let mut path = full.display().to_string();
        for &(_, c) in ins.companions.iter().filter(|(a, _)| *a == n) {
            path.push_str(&format!("\n+ {}", map.tree.path(c).display()));
        }
        let shown = relative_label(&full, &map.tree.path(map.tree.root()));
        ui.horizontal(|ui| {
            let mut on = st.picks.contains(&pick);
            if ui
                .add_enabled(enabled, egui::Checkbox::without_text(&mut on))
                .changed()
            {
                if on {
                    st.picks.insert(pick);
                } else {
                    st.picks.remove(&pick);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format_bytes(item_size(ins, &map.tree, n)));
                let link = ui
                    .with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(shorten_middle(&shown, 44))
                                .sense(egui::Sense::click())
                                .truncate(),
                        )
                    })
                    .inner
                    .on_hover_text(format!("{path}\nClique para mostrar no mapa"));
                if link.clicked() {
                    reveal = Some(n);
                }
            });
        });
    }
    if all.len() > MAX_ITEMS_SHOWN {
        ui.label(
            RichText::new(format!(
                "e mais {} itens (marque a categoria para incluir todos)",
                all.len() - MAX_ITEMS_SHOWN
            ))
            .weak(),
        );
    }
    if let Some(n) = reveal {
        let now = ui.input(|i| i.time);
        map.reveal(n, now);
    }
}

/// Um checkbox por disco a compactar: nome ("Ubuntu", "Docker") e o tamanho atual do arquivo.
fn disk_list(
    ui: &mut egui::Ui,
    ins: &Insight,
    st: &mut SidebarState,
    map: &mut MapState,
    enabled: bool,
) {
    let mut reveal = None;
    for d in &ins.disks {
        if !map.tree.node(d.node).alive {
            continue;
        }
        let pick = Pick::Item(ins.id, d.node);
        ui.horizontal(|ui| {
            let mut on = st.picks.contains(&pick);
            if ui
                .add_enabled(enabled, egui::Checkbox::without_text(&mut on))
                .changed()
            {
                if on {
                    st.picks.insert(pick);
                } else {
                    st.picks.remove(&pick);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(disk_size_text(map.tree.node(d.node).size));
                let link = ui
                    .with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&d.label).strong())
                                .sense(egui::Sense::click())
                                .truncate(),
                        )
                    })
                    .inner
                    .on_hover_text(format!("{}\nClique para mostrar no mapa", d.path.display()));
                if link.clicked() {
                    reveal = Some(d.node);
                }
            });
        });
    }
    if let Some(n) = reveal {
        let now = ui.input(|i| i.time);
        map.reveal(n, now);
    }
}

/// Tamanho do programa na lista; desconhecido nunca vira "0 B". App da Store sem admin: a pasta
/// em `WindowsApps` não é legível, e a lista diz isso.
pub fn size_label(p: &Program, elevated: bool) -> String {
    match p.size {
        Some(s) => format_bytes(s),
        None if p.is_store() && !elevated => "tamanho desconhecido (precisa de admin)".to_string(),
        None => "tamanho desconhecido".to_string(),
    }
}

/// Texto do botão que tira o programa.
pub fn remove_label(p: &Program) -> &'static str {
    if p.is_store() {
        "Remover"
    } else {
        "Desinstalar"
    }
}

/// Etiqueta pequena "Store" ao lado do nome.
fn store_pill(ui: &mut egui::Ui) -> egui::Response {
    let color = if ui.visuals().dark_mode {
        Color32::from_rgb(120, 170, 240)
    } else {
        Color32::from_rgb(40, 90, 180)
    };
    let size = egui::vec2(38.0, ui.spacing().interact_size.y - 6.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        rect.height() / 2.0,
        color.gamma_multiply(0.18),
        egui::Stroke::new(1.0, color),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "Store",
        egui::FontId::proportional(10.5),
        color,
    );
    resp.on_hover_text("App da Microsoft Store")
}

/// Separa os apps da Store dos programas do registro, mantendo a ordem de cada um (a da lista:
/// tamanho desc, desconhecidos no fim). Devolve (Store, registro).
pub fn split_store(list: &[Program]) -> (Vec<&Program>, Vec<&Program>) {
    list.iter().partition(|p| p.is_store())
}

pub fn store_section_title(n: usize) -> String {
    format!("Apps da Microsoft Store ({n})")
}

/// Apps da Store numa subseção própria, no topo e recolhida (senão, com tamanho desconhecido,
/// eles iriam para o fim de uma lista longa); depois os programas do registro.
fn program_list(
    ui: &mut egui::Ui,
    ins: &Insight,
    busy: bool,
    elevated: bool,
) -> Option<SidebarAction> {
    let mut out = None;
    let (store, registry) = split_store(&ins.programs);
    if !store.is_empty() {
        ui.separator();
        egui::CollapsingHeader::new(RichText::new(store_section_title(store.len())).strong())
            .id_salt(("apps_store", ins.id))
            .default_open(false)
            .show(ui, |ui| {
                for p in &store {
                    if let Some(a) = program_row(ui, p, busy, elevated) {
                        out = Some(a);
                    }
                }
            });
    }
    for p in registry {
        if let Some(a) = program_row(ui, p, busy, elevated) {
            out = Some(a);
        }
    }
    out
}

fn program_row(
    ui: &mut egui::Ui,
    p: &Program,
    busy: bool,
    elevated: bool,
) -> Option<SidebarAction> {
    let mut out = None;
    ui.separator();
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let hover = if p.is_store() {
                "Remove o app pela Microsoft Store (pede confirmação)"
            } else {
                "Abre o desinstalador do próprio programa"
            };
            let b = ui
                .add_enabled(!busy, egui::Button::new(remove_label(p)))
                .on_hover_text(hover);
            if b.clicked() {
                out = Some(SidebarAction::Uninstall(p.clone()));
            }
            // Desconhecido vai para a linha de baixo: o texto é longo e espremeria o nome.
            if p.size.is_some() {
                ui.label(size_label(p, elevated));
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if p.is_store() {
                    store_pill(ui);
                }
                ui.add(egui::Label::new(RichText::new(&p.name).strong()).truncate())
                    .on_hover_text(&p.name);
            });
        });
    });
    let mut meta = p.publisher.clone();
    if let Some(d) = &p.installed {
        if !meta.is_empty() {
            meta.push_str(" · ");
        }
        meta.push_str(&format!("instalado em {d}"));
    }
    if !meta.is_empty() {
        ui.label(RichText::new(meta).small().weak());
    }
    if p.size.is_none() {
        ui.label(RichText::new(size_label(p, elevated)).small().weak())
            .on_hover_text(unknown_size_hint(p, elevated));
    }
    out
}

/// Dica (hover) de um tamanho desconhecido: diz por que não deu para medir.
pub fn unknown_size_hint(p: &Program, elevated: bool) -> &'static str {
    match (p.is_store(), &p.location) {
        (true, None) => "O Windows não informou a pasta deste app",
        (true, Some(_)) if !elevated => {
            "Não consegui ler a pasta do app em WindowsApps; como administrador dá para medir"
        }
        (true, Some(_)) => "Não consegui ler a pasta do app",
        (false, _) => "A pasta do programa não está na análise, e o registro não diz o tamanho",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::NodeId;
    use crate::ui::suggestions::tests::{id, list, tree, virt_list, virt_tree};

    #[test]
    fn category_checkbox_toggles_all_items() {
        let t = tree();
        let l = list(&t);
        let mut st = SidebarState::default();
        assert_eq!(st.category_state(&l[2]), (false, false));
        st.picks.insert(Pick::Item("dev_artifacts", l[2].nodes[0]));
        assert_eq!(st.category_state(&l[2]), (false, true), "parcial");
        st.set_category(&l[2], true);
        assert_eq!(st.category_state(&l[2]), (true, false));
        st.set_category(&l[2], false);
        assert!(st.picks.is_empty());
        st.set_category(&l[0], true);
        assert!(st.picks.contains(&Pick::Whole("temp")));
    }

    #[test]
    fn disks_and_avds_toggle_by_item() {
        let t = virt_tree();
        let l = virt_list(&t);
        let ubuntu = id(&t, r"C:\WSL\ext4.vhdx");
        let docker = id(&t, r"C:\Docker\docker_data.vhdx");
        let avd = id(&t, r"C:\avd\Pixel.avd");
        let mut st = SidebarState::default();
        st.picks.insert(Pick::Item("vhdx_compact", ubuntu));
        assert_eq!(st.category_state(&l[0]), (false, true), "um disco de dois");
        st.set_category(&l[0], true);
        assert!(st.picks.contains(&Pick::Item("vhdx_compact", docker)));
        assert_eq!(st.category_state(&l[0]), (true, false));
        // AVD: a pasta é o único item (o `.ini` vai junto); marcá-la completa a categoria.
        st.picks.insert(Pick::Item("android_avds", avd));
        assert_eq!(st.category_state(&l[2]), (true, false));
        st.set_category(&l[2], false);
        assert!(
            !st.picks
                .iter()
                .any(|p| matches!(p, Pick::Item("android_avds", _)))
        );
        // Docker: categoria inteira.
        st.set_category(&l[1], true);
        assert!(st.picks.contains(&Pick::Whole("docker_prune")));
        assert_eq!(st.category_state(&l[1]), (true, false));
    }

    #[test]
    fn detail_texts_for_virt_actions() {
        let t = virt_tree();
        let l = virt_list(&t);
        assert_eq!(
            action_note(&l[0]),
            Some(
                "Compactar libera o espaço vazio de dentro do disco; quanto, só se sabe no fim. Desliga o WSL e o Docker enquanto roda."
            )
        );
        assert_eq!(
            action_note(&l[1]),
            Some(
                "Remove imagens sem contêiner, contêineres parados e cache de build. Volumes não são tocados."
            )
        );
        assert_eq!(action_note(&l[2]), Some("Vão para a Lixeira."));
    }

    #[test]
    fn program_size_label_and_button() {
        use crate::insights::ProgramSource;
        let mut p = Program {
            name: "A".into(),
            publisher: String::new(),
            size: Some(3 * 1024 * 1024),
            installed: None,
            uninstall: "u".into(),
            location: None,
            source: ProgramSource::Registry,
        };
        assert_eq!(size_label(&p, false), "3,0 MB");
        assert_eq!(remove_label(&p), "Desinstalar");
        p.size = None;
        assert_eq!(size_label(&p, false), "tamanho desconhecido");
        p.source = ProgramSource::Store {
            full_name: "x".into(),
        };
        assert_eq!(remove_label(&p), "Remover");
        assert_eq!(
            size_label(&p, false),
            "tamanho desconhecido (precisa de admin)"
        );
        assert_eq!(size_label(&p, true), "tamanho desconhecido");
        // Dica do tamanho desconhecido.
        assert_eq!(
            unknown_size_hint(&p, false),
            "O Windows não informou a pasta deste app"
        );
        p.location = Some(std::path::PathBuf::from(r"C:\Program Files\WindowsApps\x"));
        assert!(unknown_size_hint(&p, false).contains("WindowsApps"));
        p.source = ProgramSource::Registry;
        assert!(unknown_size_hint(&p, false).contains("registro"));
    }

    #[test]
    fn store_apps_get_their_own_section_in_order() {
        use crate::insights::ProgramSource;
        let p = |name: &str, size: Option<u64>, store: bool| Program {
            name: name.into(),
            publisher: String::new(),
            size,
            installed: None,
            uninstall: String::new(),
            location: None,
            source: if store {
                ProgramSource::Store {
                    full_name: format!("{name}_1"),
                }
            } else {
                ProgramSource::Registry
            },
        };
        // Já na ordem de `programs_insight` (tamanho desc, desconhecidos no fim).
        let list = vec![
            p("Grande", Some(90), false),
            p("Spotify", Some(50), true),
            p("Pequeno", Some(5), false),
            p("Bloco", None, true),
        ];
        let (store, registry) = split_store(&list);
        let names = |v: &[&Program]| v.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&store), ["Spotify", "Bloco"]);
        assert_eq!(names(&registry), ["Grande", "Pequeno"]);
        assert_eq!(store_section_title(2), "Apps da Microsoft Store (2)");
    }

    #[test]
    fn retain_valid_forgets_gone_items() {
        let t = tree();
        let l = list(&t);
        let mut st = SidebarState::default();
        st.picks.insert(Pick::Whole("temp"));
        st.picks.insert(Pick::Whole("sumiu"));
        st.picks.insert(Pick::Item("dev_artifacts", NodeId(999)));
        st.retain_valid(&l);
        assert_eq!(st.picks, HashSet::from([Pick::Whole("temp")]));
    }
}
