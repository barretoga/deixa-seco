//! Histórico de navegação (voltar/avançar) e barra de caminho clicável.

use crate::tree::{NodeId, Tree};
use eframe::egui;

/// Histórico de pastas visitadas. `history[pos]` é a pasta atual.
pub struct Nav {
    history: Vec<NodeId>,
    pos: usize,
}

impl Nav {
    pub fn new(root: NodeId) -> Nav {
        Nav {
            history: vec![root],
            pos: 0,
        }
    }

    pub fn current(&self) -> NodeId {
        self.history[self.pos]
    }

    /// Vai para `id` descartando o "futuro". Ir para a pasta atual não muda nada.
    pub fn go(&mut self, id: NodeId) {
        if id == self.current() {
            return;
        }
        self.history.truncate(self.pos + 1);
        self.history.push(id);
        self.pos += 1;
    }

    pub fn back(&mut self) -> bool {
        if self.can_back() {
            self.pos -= 1;
            true
        } else {
            false
        }
    }

    pub fn forward(&mut self) -> bool {
        if self.can_forward() {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub fn can_back(&self) -> bool {
        self.pos > 0
    }

    pub fn can_forward(&self) -> bool {
        self.pos + 1 < self.history.len()
    }

    /// Troca entradas removidas da árvore pelo ancestral vivo mais próximo e junta
    /// duplicatas consecutivas que isso cria, mantendo a posição na entrada equivalente.
    pub fn fix_after_remove(&mut self, tree: &Tree) {
        let mut out: Vec<NodeId> = Vec::with_capacity(self.history.len());
        let mut pos = 0;
        for (i, &id) in self.history.iter().enumerate() {
            let id = tree.nearest_alive(id);
            if out.last() != Some(&id) {
                out.push(id);
            }
            if i == self.pos {
                pos = out.len() - 1;
            }
        }
        self.history = out;
        self.pos = pos;
    }
}

/// Barra de caminho: um botão por ancestral da pasta atual (`C: › Users › usuario`).
/// Devolve o segmento clicado.
pub fn path_bar(ui: &mut egui::Ui, tree: &Tree, nav: &Nav) -> Option<NodeId> {
    let mut clicked = None;
    let chain = tree.ancestors(nav.current());
    let last = chain.len() - 1;
    for (i, &id) in chain.iter().enumerate() {
        if i > 0 {
            ui.label(egui::RichText::new("›").weak());
        }
        let text = segment_label(tree, id);
        let text = if i == last {
            egui::RichText::new(text).strong()
        } else {
            egui::RichText::new(text)
        };
        let resp = ui
            .add(egui::Button::new(text).frame(false))
            .on_hover_text(tree.path(id).display().to_string());
        if resp.clicked() && i != last {
            clicked = Some(id);
        }
    }
    clicked
}

/// Nome de um segmento; a raiz (`C:\`) aparece sem a barra final.
pub fn segment_label(tree: &Tree, id: NodeId) -> String {
    let name = &*tree.node(id).name;
    if id == tree.root() {
        let trimmed = name.trim_end_matches(['\\', '/']);
        if trimmed.is_empty() { name } else { trimmed }.to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sample() -> Tree {
        // C:\ { a\ { b\ { f 10 } }, c\ { g 5 } }
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let a = t.add(r, "a", true, 0, None);
        let b = t.add(a, "b", true, 0, None);
        t.add(b, "f", false, 10, None);
        let c = t.add(r, "c", true, 0, None);
        t.add(c, "g", false, 5, None);
        t.finalize();
        t
    }

    fn id(t: &Tree, p: &str) -> NodeId {
        t.find(Path::new(p)).unwrap()
    }

    #[test]
    fn new_starts_at_root_without_history() {
        let t = sample();
        let mut nav = Nav::new(t.root());
        assert_eq!(nav.current(), t.root());
        assert!(!nav.can_back() && !nav.can_forward());
        assert!(!nav.back());
        assert!(!nav.forward());
        assert_eq!(nav.current(), t.root());
    }

    #[test]
    fn go_then_back_returns() {
        let t = sample();
        let (a, b) = (id(&t, r"C:\a"), id(&t, r"C:\a\b"));
        let mut nav = Nav::new(t.root());
        nav.go(a);
        nav.go(b);
        assert_eq!(nav.current(), b);
        assert!(nav.back());
        assert_eq!(nav.current(), a);
        assert!(nav.back());
        assert_eq!(nav.current(), t.root());
        assert!(!nav.back());
        assert!(nav.forward());
        assert_eq!(nav.current(), a);
        assert!(nav.forward());
        assert_eq!(nav.current(), b);
        assert!(!nav.forward());
    }

    #[test]
    fn go_after_back_discards_future() {
        let t = sample();
        let (a, b, c) = (id(&t, r"C:\a"), id(&t, r"C:\a\b"), id(&t, r"C:\c"));
        let mut nav = Nav::new(t.root());
        nav.go(a);
        nav.go(b);
        nav.back(); // em a
        nav.go(c);
        assert_eq!(nav.current(), c);
        assert!(!nav.can_forward());
        assert!(!nav.forward());
        assert!(nav.back());
        assert_eq!(nav.current(), a);
        assert!(nav.back());
        assert_eq!(nav.current(), t.root());
    }

    #[test]
    fn go_to_current_is_noop() {
        let t = sample();
        let a = id(&t, r"C:\a");
        let mut nav = Nav::new(t.root());
        nav.go(a);
        nav.go(a);
        assert!(nav.back());
        assert_eq!(nav.current(), t.root());
        assert!(!nav.back());
    }

    #[test]
    fn fix_after_remove_replaces_dead_entries() {
        let mut t = sample();
        let (a, b, c) = (id(&t, r"C:\a"), id(&t, r"C:\a\b"), id(&t, r"C:\c"));
        let mut nav = Nav::new(t.root());
        nav.go(c);
        nav.go(a);
        nav.go(b);
        t.remove(a);
        nav.fix_after_remove(&t);
        // a e b morreram: viram a raiz, o ancestral vivo mais próximo.
        assert_eq!(nav.current(), t.root());
        assert!(t.node(nav.current()).alive);
        assert!(nav.back());
        assert_eq!(nav.current(), c);
        assert!(nav.back());
        assert_eq!(nav.current(), t.root());
        assert!(!nav.back());
    }

    #[test]
    fn fix_after_remove_keeps_live_position_and_future() {
        let mut t = sample();
        let (a, b, c) = (id(&t, r"C:\a"), id(&t, r"C:\a\b"), id(&t, r"C:\c"));
        let mut nav = Nav::new(t.root());
        nav.go(a);
        nav.go(b);
        nav.back();
        nav.back(); // na raiz, futuro: a, b
        t.remove(c);
        nav.fix_after_remove(&t);
        assert_eq!(nav.current(), t.root());
        assert!(nav.forward());
        assert_eq!(nav.current(), a);
        t.remove(b);
        nav.fix_after_remove(&t);
        assert_eq!(nav.current(), a);
        assert!(
            !nav.forward(),
            "b morto virou a: duplicata consecutiva some"
        );
    }

    #[test]
    fn segment_label_trims_root_separator() {
        let t = sample();
        assert_eq!(segment_label(&t, t.root()), "C:");
        assert_eq!(segment_label(&t, id(&t, r"C:\a\b")), "b");
        let t = Tree::new(r"D:\Fotos de São Paulo".into());
        assert_eq!(segment_label(&t, t.root()), r"D:\Fotos de São Paulo");
    }
}
