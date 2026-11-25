//! Busca (Ctrl+F): índice de nomes dobrados, debounce, resultados e a lista na tela.

use crate::format::format_bytes;
use crate::tree::{NodeId, Tree, fold};
use eframe::egui;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Espera depois da última tecla antes de buscar.
pub const DEBOUNCE_SECS: f64 = 0.15;
/// Máximo de resultados na lista.
pub const MAX_RESULTS: usize = 50;
/// Largura máxima (em caracteres) do caminho na lista.
const PATH_CHARS: usize = 60;

/// Nomes dobrados (minúsculas, sem acento) de todos os nós, num único buffer: serve
/// para buscar fora da thread da UI sem emprestar a árvore. `offsets[i]..offsets[i+1]`
/// é o nome do nó `i`. Nós removidos continuam aqui; `top_hits` filtra os vivos.
pub struct SearchIndex {
    text: String,
    offsets: Vec<u32>,
}

impl SearchIndex {
    pub fn build(tree: &Tree) -> SearchIndex {
        let mut text = String::new();
        let mut offsets = Vec::with_capacity(tree.len() + 1);
        for i in 0..tree.len() as u32 {
            offsets.push(text.len() as u32);
            text.push_str(&fold(&tree.node(NodeId(i)).name));
        }
        offsets.push(text.len() as u32);
        SearchIndex { text, offsets }
    }

    /// Nós (menos a raiz) cujo nome dobrado contém `folded_query`, em ordem de id.
    pub fn matches(&self, folded_query: &str) -> Vec<NodeId> {
        if folded_query.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for i in 1..self.offsets.len() - 1 {
            let name = &self.text[self.offsets[i] as usize..self.offsets[i + 1] as usize];
            if name.contains(folded_query) {
                out.push(NodeId(i as u32));
            }
        }
        out
    }
}

/// Os `limit` maiores entre os nós vivos de `ids` (empate: id menor primeiro).
pub fn top_hits(tree: &Tree, ids: Vec<NodeId>, limit: usize) -> Vec<NodeId> {
    let mut hits: Vec<NodeId> = ids.into_iter().filter(|&id| tree.node(id).alive).collect();
    let key = |id: &NodeId| (std::cmp::Reverse(tree.node(*id).size), id.0);
    if hits.len() > limit {
        if limit == 0 {
            return Vec::new();
        }
        hits.select_nth_unstable_by_key(limit - 1, key);
        hits.truncate(limit);
    }
    hits.sort_by_key(key);
    hits
}

/// Já passou o tempo de espera desde a última edição?
pub fn debounce_ready(edited_at: Option<f64>, now: f64) -> bool {
    edited_at.is_some_and(|t| now - t >= DEBOUNCE_SECS)
}

/// Move a seleção da lista sem sair dela.
pub fn step_selection(sel: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let moved = sel as i64 + i64::from(delta);
    moved.clamp(0, len as i64 - 1) as usize
}

/// Encurta no meio com "…", mantendo `max` caracteres no total.
pub fn shorten_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// Estado da busca: campo, debounce, thread de busca e resultados.
#[derive(Default)]
pub struct SearchState {
    pub open: bool,
    pub query: String,
    /// Pede foco no campo no próximo quadro.
    focus: bool,
    edited_at: Option<f64>,
    /// Consulta da última busca disparada.
    last_run: String,
    pending: Option<Receiver<Vec<NodeId>>>,
    pub results: Vec<NodeId>,
    pub selected: usize,
    /// Enter apertado antes dos resultados chegarem: escolhe o primeiro quando chegarem.
    submit: bool,
}

impl SearchState {
    pub fn open(&mut self) {
        self.open = true;
        self.focus = true;
    }

    pub fn close(&mut self) {
        *self = SearchState::default();
    }

    /// O texto mudou.
    pub fn edited(&mut self, now: f64) {
        self.edited_at = Some(now);
    }

    /// Resultados em dia com o texto do campo?
    fn settled(&self) -> bool {
        self.pending.is_none() && self.edited_at.is_none() && self.last_run == self.query
    }

    /// Dispara a busca quando o debounce vence e recolhe o resultado da thread.
    /// Devolve o nó escolhido se o Enter estava esperando resultados.
    pub fn poll(
        &mut self,
        now: f64,
        index: &Arc<SearchIndex>,
        tree: &Tree,
        ctx: &egui::Context,
    ) -> Option<NodeId> {
        if debounce_ready(self.edited_at, now) {
            self.edited_at = None;
            if self.query != self.last_run {
                self.last_run.clone_from(&self.query);
                let q = fold(self.query.trim());
                if q.is_empty() {
                    self.pending = None;
                    self.results.clear();
                    self.selected = 0;
                } else {
                    let (tx, rx) = mpsc::channel();
                    let index = Arc::clone(index);
                    std::thread::spawn(move || {
                        let _ = tx.send(index.matches(&q));
                    });
                    self.pending = Some(rx);
                }
            }
        } else if let Some(t) = self.edited_at {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(
                (DEBOUNCE_SECS - (now - t)).max(0.0) + 0.005,
            ));
        }
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(ids) => {
                    self.pending = None;
                    self.results = top_hits(tree, ids, MAX_RESULTS);
                    self.selected = 0;
                }
                Err(TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(15));
                }
                Err(TryRecvError::Disconnected) => self.pending = None,
            }
        }
        if self.submit && self.settled() {
            self.submit = false;
            return self.results.first().copied();
        }
        None
    }

    /// Enter: escolhe o resultado marcado, ou espera a busca terminar.
    fn submit(&mut self) -> Option<NodeId> {
        if self.settled() {
            self.results.get(self.selected).copied()
        } else {
            self.submit = true;
            None
        }
    }
}

/// Campo de busca (linha extra do painel superior). Devolve o nó escolhido com Enter.
pub fn search_bar(ui: &mut egui::Ui, s: &mut SearchState, now: f64) -> Option<NodeId> {
    let mut chosen = None;
    ui.horizontal(|ui| {
        ui.label("Buscar:");
        let width = (ui.available_width() - 40.0).max(120.0);
        let edit = egui::TextEdit::singleline(&mut s.query)
            .hint_text("nome de arquivo ou pasta (sem acento, tanto faz)")
            .desired_width(width);
        let resp = ui.add(edit);
        if s.focus {
            resp.request_focus();
            s.focus = false;
        }
        if resp.changed() {
            s.edited(now);
        }
        // O campo de uma linha perde o foco no Enter, então ele é lido em `lost_focus`.
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            chosen = s.submit();
            resp.request_focus();
        }
        if resp.has_focus() {
            let (down, up) = ui.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                )
            });
            if down {
                s.selected = step_selection(s.selected, s.results.len(), 1);
            }
            if up {
                s.selected = step_selection(s.selected, s.results.len(), -1);
            }
        }
        if ui.small_button("✕").on_hover_text("Fechar (Esc)").clicked() {
            s.close();
        }
    });
    chosen
}

/// Uma linha da lista: `nome · tamanho · caminho` (pasta onde o item está).
pub fn result_line(tree: &Tree, id: NodeId) -> String {
    let n = tree.node(id);
    let parent = tree.path(id).parent().map(|p| p.display().to_string());
    format!(
        "{} · {} · {}",
        n.name,
        format_bytes(n.size),
        shorten_middle(&parent.unwrap_or_default(), PATH_CHARS)
    )
}

/// Lista de resultados flutuando logo abaixo do painel superior. Devolve o clicado.
pub fn results_list(
    ctx: &egui::Context,
    below: egui::Pos2,
    width: f32,
    tree: &Tree,
    s: &mut SearchState,
) -> Option<NodeId> {
    if s.query.trim().is_empty() {
        return None;
    }
    let mut chosen = None;
    egui::Area::new(egui::Id::unique("busca_resultados"))
        .order(egui::Order::Foreground)
        .fixed_pos(below)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width((width * 0.7).clamp(300.0, 900.0));
                if s.results.is_empty() {
                    let msg = if s.settled() {
                        "Nada encontrado"
                    } else {
                        "Buscando…"
                    };
                    ui.weak(msg);
                    return;
                }
                // Altura explícita: a área lembra o tamanho do quadro "Buscando…".
                let row_h = ui.spacing().interact_size.y + ui.spacing().item_spacing.y;
                let height = (s.results.len() as f32 * row_h).min(360.0);
                egui::ScrollArea::vertical()
                    .min_scrolled_height(height)
                    .max_height(height)
                    .show(ui, |ui| {
                        for (i, &id) in s.results.iter().enumerate() {
                            let text = result_line(tree, id);
                            let row = ui.add(
                                egui::Button::selectable(i == s.selected, text)
                                    .truncate()
                                    .min_size(egui::vec2(ui.available_width(), 0.0)),
                            );
                            if row.clicked() {
                                chosen = Some(id);
                            }
                        }
                    });
            });
        });
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sample() -> Tree {
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let s = t.add(r, "São Paulo", true, 0, None);
        t.add(s, "foto.jpg", false, 30, None);
        t.add(s, "SAO-notas.txt", false, 5, None);
        t.add(r, "sao.bin", false, 100, None);
        t.add(r, "outro.bin", false, 10, None);
        t.finalize();
        t
    }

    #[test]
    fn index_agrees_with_tree_search() {
        let t = sample();
        let idx = SearchIndex::build(&t);
        for q in ["sao", "SÃO", "bin", "foto", "zzz", "."] {
            let hits = top_hits(&t, idx.matches(&fold(q)), MAX_RESULTS);
            assert_eq!(hits, t.search(q, MAX_RESULTS), "consulta {q:?}");
        }
        let hits = top_hits(&t, idx.matches("sao"), 50);
        assert_eq!(
            t.path(hits[0]),
            Path::new("C:\\").join("sao.bin"),
            "maior primeiro"
        );
    }

    #[test]
    fn top_hits_skips_dead_and_limits() {
        let mut t = sample();
        let idx = SearchIndex::build(&t);
        let bin = t.find(Path::new(r"C:\sao.bin")).unwrap();
        t.remove(bin);
        let hits = top_hits(&t, idx.matches("bin"), 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(&*t.node(hits[0]).name, "outro.bin");
    }

    #[test]
    fn empty_query_matches_nothing() {
        let t = sample();
        let idx = SearchIndex::build(&t);
        assert!(idx.matches("").is_empty());
    }

    #[test]
    fn debounce_waits_150ms() {
        assert!(!debounce_ready(None, 10.0));
        assert!(!debounce_ready(Some(10.0), 10.1));
        assert!(debounce_ready(Some(10.0), 10.15));
        assert!(debounce_ready(Some(10.0), 11.0));
    }

    #[test]
    fn selection_steps_clamp() {
        assert_eq!(step_selection(0, 3, 1), 1);
        assert_eq!(step_selection(2, 3, 1), 2);
        assert_eq!(step_selection(0, 3, -1), 0);
        assert_eq!(step_selection(5, 3, 0), 2, "volta para dentro da lista");
        assert_eq!(step_selection(0, 0, 1), 0);
    }

    #[test]
    fn shorten_keeps_both_ends() {
        assert_eq!(shorten_middle("abc", 10), "abc");
        assert_eq!(shorten_middle("abcdefghij", 10), "abcdefghij");
        let s = shorten_middle(r"C:\Users\usuario\São Paulo\fotos\2026\coração.jpg", 20);
        assert_eq!(s.chars().count(), 20);
        assert!(s.starts_with(r"C:\Users"));
        assert!(s.ends_with("ração.jpg"));
        assert!(s.contains('…'));
        assert_eq!(shorten_middle("abcdef", 1), "…");
    }
}
