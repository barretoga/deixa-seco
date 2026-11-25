//! In-memory arena file tree. Nodes live in a `Vec` and are addressed by `NodeId`.
//! Children always have a larger id than their parent, which `finalize` relies on.

use crate::kind::{ALL_KINDS, FileKind, KindHist, dominant_in, kind_of};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

/// Máximo de nós de um `preview` (fica leve de copiar e de desenhar a cada segundo).
pub const PREVIEW_MAX_NODES: usize = 5_000;

#[derive(Clone)]
pub struct Node {
    pub name: Box<str>,
    pub parent: Option<NodeId>,
    pub is_dir: bool,
    /// Allocated size; for directories, aggregated after `finalize`.
    pub size: u64,
    pub files: u64,
    pub mtime: Option<SystemTime>,
    pub alive: bool,
    pub no_access: bool,
}

/// `Clone` existe para a UI compartilhar a árvore com threads (`Arc::make_mut`).
#[derive(Clone)]
pub struct Tree {
    root_path: PathBuf,
    nodes: Vec<Node>,
    children: Vec<Vec<NodeId>>,
}

/// Lowercases and strips common Latin accents, so "São" folds to "sao".
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        let f = match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
            'ç' | 'ć' | 'č' => 'c',
            'ď' | 'đ' => 'd',
            'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => 'e',
            'í' | 'ì' | 'î' | 'ï' | 'ī' | 'į' => 'i',
            'ł' => 'l',
            'ñ' | 'ń' | 'ň' => 'n',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => 'o',
            'ŕ' | 'ř' => 'r',
            'ś' | 'š' | 'ş' => 's',
            'ť' | 'ţ' => 't',
            'ú' | 'ù' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => 'u',
            'ý' | 'ÿ' => 'y',
            'ź' | 'ż' | 'ž' => 'z',
            other => other,
        };
        out.push(f);
    }
    out
}

/// Windows paths are case-insensitive but accent-sensitive, so `find` only lowercases.
fn lower_component(c: Component<'_>) -> String {
    c.as_os_str().to_string_lossy().to_lowercase()
}

impl Tree {
    pub fn new(root_path: PathBuf) -> Tree {
        let root = Node {
            name: root_path.to_string_lossy().into_owned().into_boxed_str(),
            parent: None,
            is_dir: true,
            size: 0,
            files: 0,
            mtime: None,
            alive: true,
            no_access: false,
        };
        Tree {
            root_path,
            nodes: vec![root],
            children: vec![Vec::new()],
        }
    }

    pub fn root(&self) -> NodeId {
        NodeId(0)
    }

    /// Total de nós (inclui removidos e a raiz).
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        false // a raiz sempre existe
    }

    /// Libera capacidade sobrando dos vetores; chamar depois de `finalize`.
    pub fn shrink(&mut self) {
        self.nodes.shrink_to_fit();
        self.children.shrink_to_fit();
        for k in &mut self.children {
            k.shrink_to_fit();
        }
    }

    pub fn add(
        &mut self,
        parent: NodeId,
        name: &str,
        is_dir: bool,
        size: u64,
        mtime: Option<SystemTime>,
    ) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            name: name.into(),
            parent: Some(parent),
            is_dir,
            size,
            files: u64::from(!is_dir),
            mtime,
            alive: true,
            no_access: false,
        });
        self.children.push(Vec::new());
        self.children[parent.0 as usize].push(id);
        id
    }

    /// Aggregates size/files into directories and sorts children by size descending.
    /// Call once, after all nodes have been added.
    pub fn finalize(&mut self) {
        for i in (1..self.nodes.len()).rev() {
            let (size, files) = (self.nodes[i].size, self.nodes[i].files);
            if let Some(p) = self.nodes[i].parent {
                let parent = &mut self.nodes[p.0 as usize];
                parent.size += size;
                parent.files += files;
            }
        }
        let nodes = &self.nodes;
        for kids in &mut self.children {
            kids.sort_by(|a, b| {
                let (na, nb) = (&nodes[a.0 as usize], &nodes[b.0 as usize]);
                nb.size.cmp(&na.size).then_with(|| na.name.cmp(&nb.name))
            });
        }
    }

    pub fn set_no_access(&mut self, id: NodeId) {
        self.nodes[id.0 as usize].no_access = true;
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    /// Live children only, in sorted order.
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.children[id.0 as usize]
    }

    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut names = Vec::new();
        let mut cur = id;
        while let Some(p) = self.node(cur).parent {
            names.push(&*self.node(cur).name);
            cur = p;
        }
        let mut path = self.root_path.clone();
        for n in names.into_iter().rev() {
            path.push(n);
        }
        path
    }

    /// Root first, includes `id` itself.
    pub fn ancestors(&self, id: NodeId) -> Vec<NodeId> {
        let mut v = vec![id];
        let mut cur = id;
        while let Some(p) = self.node(cur).parent {
            v.push(p);
            cur = p;
        }
        v.reverse();
        v
    }

    /// Case-insensitive (but accent-sensitive) lookup of a live node by full path.
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let mut comps = path.components();
        for rc in self.root_path.components() {
            if lower_component(rc) != lower_component(comps.next()?) {
                return None;
            }
        }
        let mut cur = self.root();
        for c in comps {
            match c {
                Component::CurDir => continue,
                Component::Normal(_) => {}
                _ => return None,
            }
            let want = lower_component(c);
            cur = *self
                .children(cur)
                .iter()
                .find(|&&k| self.node(k).name.to_lowercase() == want)?;
        }
        Some(cur)
    }

    /// Live nodes whose name contains `query` (case/accent-insensitive), largest first.
    /// Usado como referência nos testes (a busca da UI usa `ui::search::SearchIndex`).
    pub fn search(&self, query: &str, limit: usize) -> Vec<NodeId> {
        let q = fold(query);
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<NodeId> = (1..self.nodes.len())
            .filter(|&i| self.nodes[i].alive && fold(&self.nodes[i].name).contains(&q))
            .map(|i| NodeId(i as u32))
            .collect();
        hits.sort_by_key(|id| std::cmp::Reverse(self.node(*id).size));
        hits.truncate(limit);
        hits
    }

    /// Marks the subtree dead and subtracts it from its ancestors (re-sorting their children).
    /// Returns the bytes freed. The root cannot be removed (returns 0).
    pub fn remove(&mut self, id: NodeId) -> u64 {
        self.remove_many(&[id])
    }

    /// `remove` for many nodes at once: sizes are subtracted per node, but each affected
    /// parent's child list is filtered once and each affected ancestor re-sorted once, so a
    /// flat folder with tens of thousands of removed files stays linear. Dead ids, repeats,
    /// the root and nodes inside another removed node are fine. Returns the bytes freed.
    pub fn remove_many(&mut self, ids: &[NodeId]) -> u64 {
        let mut total = 0;
        let mut parents: HashSet<NodeId> = HashSet::new();
        for &id in ids {
            let Some(parent) = self.node(id).parent else {
                continue;
            };
            if !self.node(id).alive {
                continue;
            }
            let (bytes, files) = (self.node(id).size, self.node(id).files);
            let mut stack = vec![id];
            while let Some(n) = stack.pop() {
                self.nodes[n.0 as usize].alive = false;
                stack.extend(self.children[n.0 as usize].iter().copied());
            }
            let mut cur = Some(parent);
            while let Some(a) = cur {
                let node = &mut self.nodes[a.0 as usize];
                node.size = node.size.saturating_sub(bytes);
                node.files = node.files.saturating_sub(files);
                cur = node.parent;
            }
            parents.insert(parent);
            total += bytes;
        }
        let nodes = &self.nodes;
        for &p in &parents {
            self.children[p.0 as usize].retain(|&k| nodes[k.0 as usize].alive);
        }
        // Each live ancestor of an affected parent, once.
        let mut affected: HashSet<NodeId> = HashSet::new();
        for &p in &parents {
            let mut cur = Some(p);
            while let Some(a) = cur {
                if !affected.insert(a) {
                    break;
                }
                cur = self.nodes[a.0 as usize].parent;
            }
        }
        for a in affected {
            if self.nodes[a.0 as usize].alive {
                self.sort_children(a);
            }
        }
        total
    }

    /// Replaces the live subtree at `id` with a fresh scan `sub` of the same folder (its root
    /// becomes the new node, keeping the old name). Ancestors get the size difference and the
    /// parent's children are re-sorted. Returns the new node; `None` for the root or a dead node.
    pub fn replace_subtree(&mut self, id: NodeId, sub: &Tree) -> Option<NodeId> {
        let parent = self.node(id).parent?;
        if !self.node(id).alive {
            return None;
        }
        let name = self.node(id).name.clone();
        self.remove(id);
        let new_root = self.push_copy(parent, name, sub.node(sub.root()));
        let mut stack = vec![(sub.root(), new_root)];
        while let Some((from, to)) = stack.pop() {
            for &k in sub.children(from) {
                let n = sub.node(k);
                let copied = self.push_copy(to, n.name.clone(), n);
                stack.push((k, copied));
            }
        }
        let (bytes, files) = (self.node(new_root).size, self.node(new_root).files);
        let mut cur = Some(parent);
        while let Some(a) = cur {
            let node = &mut self.nodes[a.0 as usize];
            node.size += bytes;
            node.files += files;
            cur = node.parent;
        }
        self.sort_chain(parent);
        Some(new_root)
    }

    /// Troca o tamanho de um arquivo vivo (o disco virtual depois de compactar): os ancestrais
    /// recebem a diferença e os filhos são reordenados ao longo do caminho. Devolve o tamanho
    /// antigo; `None` para pasta ou nó morto.
    pub fn set_file_size(&mut self, id: NodeId, size: u64) -> Option<u64> {
        let n = self.node(id);
        if n.is_dir || !n.alive {
            return None;
        }
        let (old, parent) = (n.size, n.parent);
        if old == size {
            return Some(old);
        }
        self.nodes[id.0 as usize].size = size;
        let mut cur = parent;
        while let Some(a) = cur {
            let node = &mut self.nodes[a.0 as usize];
            node.size = node.size.saturating_sub(old).saturating_add(size);
            cur = node.parent;
        }
        if let Some(p) = parent {
            self.sort_chain(p);
        }
        Some(old)
    }

    /// Re-sorts the children of `from` and of each ancestor (sizes changed along the chain;
    /// the treemap relies on children sorted by size).
    fn sort_chain(&mut self, from: NodeId) {
        let mut cur = Some(from);
        while let Some(a) = cur {
            self.sort_children(a);
            cur = self.nodes[a.0 as usize].parent;
        }
    }

    /// Children by size descending, then name (same order as `finalize`).
    fn sort_children(&mut self, id: NodeId) {
        let nodes = &self.nodes;
        self.children[id.0 as usize].sort_by(|x, y| {
            let (nx, ny) = (&nodes[x.0 as usize], &nodes[y.0 as usize]);
            ny.size.cmp(&nx.size).then_with(|| nx.name.cmp(&ny.name))
        });
    }

    /// Appends a copy of `n` (already aggregated) as the last child of `parent`.
    fn push_copy(&mut self, parent: NodeId, name: Box<str>, n: &Node) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            name,
            parent: Some(parent),
            alive: true,
            ..n.clone()
        });
        self.children.push(Vec::new());
        self.children[parent.0 as usize].push(id);
        id
    }

    /// Cópia pequena para o mapa parcial durante a varredura. Funciona numa árvore ainda não
    /// finalizada (e numa finalizada): só os nós até `max_depth` abaixo da raiz, com o tamanho
    /// e a contagem de arquivos de toda a subárvore real; filhos ordenados por tamanho. Com
    /// mais de `PREVIEW_MAX_NODES` candidatos, ficam os maiores (o pai de um nó copiado sempre
    /// é copiado, porque nunca é menor que o filho). Nós mortos ficam de fora.
    pub fn preview(&self, max_depth: u8) -> Tree {
        self.preview_capped(max_depth, PREVIEW_MAX_NODES).0
    }

    /// `preview` mais o tipo dominante de cada nó dele (índice = `NodeId` do preview), contado
    /// na árvore inteira: uma pasta cortada no limite de profundidade continua com a cor do
    /// que tem dentro.
    pub fn preview_with_kinds(&self, max_depth: u8) -> (Tree, Vec<FileKind>) {
        let (out, mut map) = self.preview_capped(max_depth, PREVIEW_MAX_NODES);
        // Cada nó fora do preview passa a apontar para o ancestral copiado mais próximo
        // (o pai vem antes do filho, então já está resolvido).
        for i in 1..self.nodes.len() {
            if map[i] == u32::MAX
                && let Some(p) = self.nodes[i].parent
            {
                map[i] = map[p.0 as usize];
            }
        }
        let mut hists: Vec<KindHist> = vec![[0; ALL_KINDS.len()]; out.nodes.len()];
        for (i, node) in self.nodes.iter().enumerate() {
            if node.alive && !node.is_dir && map[i] != u32::MAX {
                hists[map[i] as usize][kind_of(&node.name, false) as usize] += node.size;
            }
        }
        drop(map);
        for j in (1..out.nodes.len()).rev() {
            if let Some(p) = out.nodes[j].parent {
                let h = hists[j];
                for (a, b) in hists[p.0 as usize].iter_mut().zip(h) {
                    *a += b;
                }
            }
        }
        let kinds = out
            .nodes
            .iter()
            .zip(&hists)
            .map(|(n, h)| {
                if n.is_dir {
                    dominant_in(h)
                } else {
                    kind_of(&n.name, false)
                }
            })
            .collect();
        (out, kinds)
    }

    /// Preview e, para cada nó da árvore original, o id dele no preview (`u32::MAX` se ficou
    /// de fora).
    fn preview_capped(&self, max_depth: u8, cap: usize) -> (Tree, Vec<u32>) {
        const NONE: u32 = u32::MAX;
        let n = self.nodes.len();
        let cap = cap.max(1);
        // Só arquivos contam o próprio tamanho: pastas valem 0 antes do `finalize` e o total
        // depois dele, então somar apenas arquivos serve para os dois casos.
        let mut size = vec![0u64; n];
        let mut files = vec![0u64; n];
        for (i, node) in self.nodes.iter().enumerate() {
            if node.alive && !node.is_dir {
                size[i] = node.size;
                files[i] = 1;
            }
        }
        // Filho tem id maior que o pai: uma passada reversa agrega tudo.
        for i in (1..n).rev() {
            if let Some(p) = self.nodes[i].parent {
                size[p.0 as usize] += size[i];
                files[p.0 as usize] += files[i];
            }
        }
        // Profundidade numa passada direta (o pai já foi visto).
        let mut depth = vec![0u8; n];
        for i in 1..n {
            if let Some(p) = self.nodes[i].parent {
                depth[i] = depth[p.0 as usize].saturating_add(1);
            }
        }
        let candidate = |i: usize| self.nodes[i].alive && depth[i] <= max_depth;

        // Limite: fica quem é maior que o tamanho do (cap-1)-ésimo candidato; empates entram
        // na ordem dos ids até completar.
        let mut threshold = 0u64;
        let mut ties = 0usize;
        let mut sizes: Vec<u64> = (1..n).filter(|&i| candidate(i)).map(|i| size[i]).collect();
        let capped = sizes.len() > cap - 1;
        if capped {
            if cap == 1 {
                threshold = u64::MAX;
                ties = 0;
            } else {
                let (_, &mut t, _) = sizes.select_nth_unstable_by(cap - 2, |a, b| b.cmp(a));
                threshold = t;
                let above = sizes.iter().filter(|&&s| s > t).count();
                ties = (cap - 1).saturating_sub(above);
            }
        }
        drop(sizes);

        let mut map = vec![NONE; n];
        let mut nodes: Vec<Node> = Vec::new();
        let mut children: Vec<Vec<NodeId>> = Vec::new();
        for i in 0..n {
            let src = &self.nodes[i];
            let parent = match src.parent {
                None => None,
                Some(p) => {
                    let mp = map[p.0 as usize];
                    if mp == NONE || !candidate(i) {
                        continue;
                    }
                    if capped {
                        if size[i] < threshold {
                            continue;
                        }
                        if size[i] == threshold {
                            if ties == 0 {
                                continue;
                            }
                            ties -= 1;
                        }
                    }
                    Some(NodeId(mp))
                }
            };
            let id = nodes.len() as u32;
            map[i] = id;
            nodes.push(Node {
                name: src.name.clone(),
                parent,
                is_dir: src.is_dir,
                size: size[i],
                files: files[i],
                mtime: src.mtime,
                alive: true,
                no_access: src.no_access,
            });
            children.push(Vec::new());
            if let Some(p) = parent {
                children[p.0 as usize].push(NodeId(id));
            }
        }
        let mut out = Tree {
            root_path: self.root_path.clone(),
            nodes,
            children,
        };
        for i in 0..out.nodes.len() {
            out.sort_children(NodeId(i as u32));
        }
        (out, map)
    }

    /// `id` itself if alive, otherwise the closest live ancestor.
    pub fn nearest_alive(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        while !self.node(cur).alive {
            match self.node(cur).parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tree {
        // C:\ { a\ {x.bin 100, y.bin 50}, São Paulo\ {foto.jpg 30} }
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let a = t.add(r, "a", true, 0, None);
        t.add(a, "x.bin", false, 100, None);
        t.add(a, "y.bin", false, 50, None);
        let s = t.add(r, "São Paulo", true, 0, None);
        t.add(s, "foto.jpg", false, 30, None);
        t.finalize();
        t
    }

    #[test]
    fn set_file_size_adjusts_ancestors_and_order() {
        let mut t = sample();
        let a = t.find(Path::new(r"C:\a")).unwrap();
        let x = t.find(Path::new(r"C:\a\x.bin")).unwrap();
        let y = t.find(Path::new(r"C:\a\y.bin")).unwrap();
        let s = t.find(Path::new(r"C:\São Paulo")).unwrap();
        let foto = t.find(Path::new(r"C:\São Paulo\foto.jpg")).unwrap();
        assert_eq!(t.set_file_size(x, 10), Some(100));
        assert_eq!(t.node(x).size, 10);
        assert_eq!(t.node(a).size, 60);
        assert_eq!(t.node(t.root()).size, 90);
        assert_eq!(t.node(t.root()).files, 3, "contagem de arquivos não muda");
        assert_eq!(t.children(a), &[y, x], "y (50) passa x (10)");
        assert_eq!(
            t.children(t.root()),
            &[a, s],
            "a (60) ainda maior que São Paulo (30)"
        );
        assert_eq!(t.set_file_size(x, 5), Some(10));
        // Crescer também: os ancestrais sobem e a ordem muda.
        assert_eq!(t.set_file_size(y, 200), Some(50));
        assert_eq!(t.node(a).size, 205);
        assert_eq!(t.node(t.root()).size, 235);
        assert_eq!(t.children(a), &[y, x]);
        assert_eq!(t.set_file_size(foto, 500), Some(30));
        assert_eq!(t.node(s).size, 500);
        assert_eq!(t.node(t.root()).size, 705);
        assert_eq!(
            t.children(t.root()),
            &[s, a],
            "São Paulo (500) passa a (205)"
        );
        assert_eq!(t.set_file_size(y, 50), Some(200));
        assert_eq!(t.set_file_size(foto, 30), Some(500));
        assert_eq!(t.set_file_size(a, 1), None, "pasta não");
        t.remove(y);
        assert_eq!(t.set_file_size(y, 1), None, "morto não");
        assert_eq!(t.node(t.root()).size, 35);
    }

    #[test]
    fn aggregates() {
        let t = sample();
        assert_eq!(t.node(t.root()).size, 180);
        assert_eq!(t.node(t.root()).files, 3);
    }

    #[test]
    fn children_sorted_desc() {
        let t = sample();
        let c = t.children(t.root());
        assert_eq!(&*t.node(c[0]).name, "a");
    }

    #[test]
    fn path_and_find() {
        let t = sample();
        let id = t.find(Path::new("c:\\são paulo\\FOTO.jpg")).unwrap();
        assert_eq!(t.path(id), PathBuf::from("C:\\São Paulo\\foto.jpg"));
    }

    #[test]
    fn search_ignores_accents() {
        let t = sample();
        assert_eq!(t.search("sao", 10).len(), 1);
    }

    #[test]
    fn remove_updates_ancestors() {
        let mut t = sample();
        let a = t.find(Path::new("C:\\a")).unwrap();
        assert_eq!(t.remove(a), 150);
        assert_eq!(t.node(t.root()).size, 30);
        assert_eq!(t.children(t.root()).len(), 1);
        let x = t.find(Path::new("C:\\a\\x.bin"));
        assert!(x.is_none());
    }

    #[test]
    fn nearest_alive_after_remove() {
        let mut t = sample();
        let x = t.find(Path::new("C:\\a\\x.bin")).unwrap();
        let a = t.find(Path::new("C:\\a")).unwrap();
        t.remove(a);
        assert_eq!(t.nearest_alive(x), t.root());
    }

    #[test]
    fn find_is_case_insensitive_but_accent_sensitive() {
        let t = sample();
        assert!(t.find(Path::new("c:\\SÃO PAULO\\foto.jpg")).is_some());
        assert!(t.find(Path::new("C:\\Sao Paulo")).is_none());
        assert_eq!(t.find(Path::new("C:\\")), Some(t.root()));
        assert!(t.find(Path::new("D:\\a")).is_none());
        assert!(t.find(Path::new("C:\\nada")).is_none());
    }

    #[test]
    fn search_orders_by_size_and_limits() {
        let t = sample();
        let hits = t.search("X", 10);
        assert_eq!(&*t.node(hits[0]).name, "x.bin");
        assert_eq!(t.search(".", 1).len(), 1);
        assert!(t.search("", 10).is_empty());
    }

    #[test]
    fn remove_root_is_noop() {
        let mut t = sample();
        assert_eq!(t.remove(t.root()), 0);
        assert_eq!(t.node(t.root()).size, 180);
        assert!(t.node(t.root()).alive);
    }

    #[test]
    fn remove_twice_returns_zero() {
        let mut t = sample();
        let a = t.find(Path::new("C:\\a")).unwrap();
        t.remove(a);
        assert_eq!(t.remove(a), 0);
        assert_eq!(t.node(t.root()).size, 30);
    }

    #[test]
    fn ancestors_root_first() {
        let t = sample();
        let x = t.find(Path::new("C:\\a\\x.bin")).unwrap();
        let anc = t.ancestors(x);
        assert_eq!(anc.len(), 3);
        assert_eq!(anc[0], t.root());
        assert_eq!(anc[2], x);
    }

    #[test]
    fn find_distinguishes_accented_siblings() {
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let a = t.add(r, "resume", true, 0, None);
        let b = t.add(r, "résumé", true, 0, None);
        t.finalize();
        assert_eq!(t.find(Path::new(r"C:\RESUME")), Some(a));
        assert_eq!(t.find(Path::new(r"C:\RÉSUMÉ")), Some(b));
    }

    #[test]
    fn replace_subtree_swaps_contents_and_sizes() {
        let mut t = sample();
        let a = t.find(Path::new(r"C:\a")).unwrap();
        // Nova varredura de C:\a: sobrou só z.bin (20) dentro de sub\.
        let mut sub = Tree::new(r"C:\a".into());
        let r = sub.root();
        let d = sub.add(r, "sub", true, 0, None);
        sub.add(d, "z.bin", false, 20, None);
        sub.set_no_access(d);
        sub.finalize();
        let new = t.replace_subtree(a, &sub).unwrap();
        assert!(!t.node(a).alive, "o nó antigo morre");
        assert_eq!(t.find(Path::new(r"C:\a")), Some(new));
        assert_eq!(t.node(new).size, 20);
        assert_eq!(t.node(new).files, 1);
        assert_eq!(t.node(t.root()).size, 50, "30 (São Paulo) + 20");
        assert_eq!(t.node(t.root()).files, 2);
        let z = t.find(Path::new(r"C:\a\sub\z.bin")).unwrap();
        assert_eq!(t.path(z), PathBuf::from(r"C:\a\sub\z.bin"));
        assert!(t.node(t.find(Path::new(r"C:\a\sub")).unwrap()).no_access);
        assert!(t.find(Path::new(r"C:\a\x.bin")).is_none());
        let kids = t.children(t.root());
        assert_eq!(
            &*t.node(kids[0]).name,
            "São Paulo",
            "filhos do pai reordenados por tamanho"
        );
        assert!(t.replace_subtree(t.root(), &sub).is_none(), "raiz não");
        assert!(t.replace_subtree(a, &sub).is_none(), "nó morto não");
    }

    #[test]
    fn remove_keeps_children_sorted_up_the_chain() {
        // C:\ { big\ {sub\ {f 100}, g 10}, mid\ {h 50} }: tirar `sub` deixa big (10) < mid (50).
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let big = t.add(r, "big", true, 0, None);
        let sub = t.add(big, "sub", true, 0, None);
        t.add(sub, "f", false, 100, None);
        t.add(big, "g", false, 10, None);
        let mid = t.add(r, "mid", true, 0, None);
        t.add(mid, "h", false, 50, None);
        t.finalize();
        assert_eq!(t.children(r), &[big, mid]);
        let f = t.find(Path::new(r"C:\big\sub\f")).unwrap();
        t.remove(f);
        assert_eq!(t.children(r), &[mid, big], "raiz reordenada");
        let g = t.find(Path::new(r"C:\big\g")).unwrap();
        assert_eq!(
            t.children(big),
            &[g, sub],
            "pai reordenado (sub vazia vai para o fim)"
        );
    }

    #[test]
    fn remove_many_handles_nested_and_dead_ids() {
        // C:\ { a\ {x 100, y 50}, São Paulo\ {foto 30} }
        let mut t = sample();
        let a = t.find(Path::new(r"C:\a")).unwrap();
        let x = t.find(Path::new(r"C:\a\x.bin")).unwrap();
        let y = t.find(Path::new(r"C:\a\y.bin")).unwrap();
        // x (dentro de a) antes de a; a de novo; raiz ignorada.
        assert_eq!(t.remove_many(&[x, a, a, t.root()]), 150);
        assert_eq!(t.node(t.root()).size, 30);
        assert_eq!(t.node(t.root()).files, 1);
        assert!(!t.node(y).alive);
        assert_eq!(t.children(t.root()).len(), 1);
        assert_eq!(t.remove_many(&[x]), 0, "já morto");
    }

    #[test]
    fn remove_many_flat_folder_with_20k_files() {
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let temp = t.add(r, "Temp", true, 0, None);
        let other = t.add(r, "outro", true, 0, None);
        t.add(other, "z", false, 50_000, None);
        let ids: Vec<NodeId> = (0..20_000)
            .map(|i| t.add(temp, &format!("f{i}.tmp"), false, 10, None))
            .collect();
        t.finalize();
        assert_eq!(t.children(r)[0], temp, "Temp (200 KB) maior que outro");
        let start = std::time::Instant::now();
        assert_eq!(t.remove_many(&ids[..19_999]), 199_990);
        let took = start.elapsed();
        assert_eq!(t.children(temp).len(), 1);
        assert_eq!(t.node(temp).size, 10);
        assert_eq!(t.children(r)[0], other, "raiz reordenada");
        assert!(took.as_secs() < 5, "remoção em lote linear: {took:?}");
    }

    #[test]
    fn tree_is_clone() {
        let t = sample();
        let c = t.clone();
        assert_eq!(c.node(c.root()).size, 180);
    }

    /// Árvore NÃO finalizada de 5 níveis:
    /// C:\ { h.bin 5, a\ { g.bin 10, b\ { x.bin 20, c\ { d\ { f.bin 1000 } } } } }
    fn deep_unfinalized() -> Tree {
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        t.add(r, "h.bin", false, 5, None);
        let a = t.add(r, "a", true, 0, None);
        t.add(a, "g.bin", false, 10, None);
        let b = t.add(a, "b", true, 0, None);
        t.add(b, "x.bin", false, 20, None);
        let c = t.add(b, "c", true, 0, None);
        let d = t.add(c, "d", true, 0, None);
        t.add(d, "f.bin", false, 1000, None);
        t
    }

    fn depth_of(t: &Tree, id: NodeId) -> usize {
        t.ancestors(id).len() - 1
    }

    #[test]
    fn preview_aggregates_full_subtree() {
        let src = deep_unfinalized();
        let p = src.preview(2);
        assert_eq!(p.node(p.root()).size, 1035, "raiz com o total");
        assert_eq!(p.node(p.root()).files, 4);
        let a = p.find(Path::new(r"C:\a")).unwrap();
        assert_eq!(p.node(a).size, 1030);
        let b = p.find(Path::new(r"C:\a\b")).unwrap();
        assert_eq!(p.node(b).size, 1020, "nível 2 com a subárvore inteira");
        assert_eq!(p.node(b).files, 2);
        assert!(p.node(b).is_dir);
        assert!(p.children(b).is_empty(), "nada do nível 3");
        assert_eq!(p.len(), 5, "raiz, h.bin, a, g.bin, b");
        assert!((0..p.len()).all(|i| depth_of(&p, NodeId(i as u32)) <= 2));
        assert_eq!(p.path(b), PathBuf::from(r"C:\a\b"));
        // A original não muda (continua sem agregar).
        assert_eq!(src.node(src.root()).size, 0);
    }

    #[test]
    fn preview_is_sorted_desc() {
        let p = deep_unfinalized().preview(3);
        let names = |id: NodeId| -> Vec<String> {
            p.children(id)
                .iter()
                .map(|&k| p.node(k).name.to_string())
                .collect()
        };
        assert_eq!(names(p.root()), ["a", "h.bin"]);
        let a = p.find(Path::new(r"C:\a")).unwrap();
        assert_eq!(names(a), ["b", "g.bin"]);
        let b = p.find(Path::new(r"C:\a\b")).unwrap();
        assert_eq!(names(b), ["c", "x.bin"]);
    }

    #[test]
    fn preview_caps_node_count_keeping_largest() {
        // 3 pastas com 4.000 arquivos cada; `big` tem os maiores.
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        for (dir, size) in [("small", 1u64), ("big", 100), ("mid", 10)] {
            let d = t.add(r, dir, true, 0, None);
            for i in 0..4_000 {
                t.add(d, &format!("f{i}"), false, size, None);
            }
        }
        let p = t.preview(3);
        assert!(p.len() <= PREVIEW_MAX_NODES, "{} nós", p.len());
        assert!(
            p.len() >= PREVIEW_MAX_NODES - 10,
            "usa o limite: {} nós",
            p.len()
        );
        assert_eq!(
            p.node(p.root()).size,
            4_000 * 111,
            "tamanhos continuam completos"
        );
        let big = p.find(Path::new(r"C:\big")).unwrap();
        assert_eq!(p.children(big).len(), 4_000, "os maiores ficam");
        let small = p.find(Path::new(r"C:\small")).unwrap();
        assert_eq!(p.node(small).size, 4_000, "pasta cortada mantém o tamanho");
        // Todo nó copiado tem o pai copiado (a árvore continua ligada).
        for i in 1..p.len() {
            let id = NodeId(i as u32);
            assert_eq!(p.ancestors(id)[0], p.root());
        }
    }

    #[test]
    fn preview_kinds_come_from_the_whole_tree() {
        // C:\ { fotos\ { 2024\ { viagem\ { a.jpg 900 } }, x.txt 10 }, b.mp4 50 }: com o corte
        // no nível 2, `2024` não tem filhos no preview, mas a cor dela é Imagem.
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let f = t.add(r, "fotos", true, 0, None);
        let y = t.add(f, "2024", true, 0, None);
        let v = t.add(y, "viagem", true, 0, None);
        t.add(v, "a.jpg", false, 900, None);
        t.add(f, "x.txt", false, 10, None);
        t.add(r, "b.mp4", false, 50, None);
        let (p, kinds) = t.preview_with_kinds(2);
        assert_eq!(kinds.len(), p.len());
        let k = |path: &str| kinds[p.find(Path::new(path)).unwrap().0 as usize];
        assert_eq!(k(r"C:\fotos\2024"), FileKind::Image);
        assert_eq!(k(r"C:\fotos"), FileKind::Image);
        assert_eq!(k(r"C:\fotos\x.txt"), FileKind::Document);
        assert_eq!(k(r"C:\b.mp4"), FileKind::Video);
        assert_eq!(kinds[0], FileKind::Image, "raiz");
        assert_eq!(p.len(), t.preview(2).len(), "mesma árvore que `preview`");
    }

    #[test]
    fn preview_of_finalized_tree_does_not_double_count() {
        let t = sample();
        let p = t.preview(3);
        assert_eq!(p.node(p.root()).size, 180);
        assert_eq!(p.node(p.root()).files, 3);
    }

    #[test]
    fn set_no_access_marks_node() {
        let mut t = sample();
        let a = t.find(Path::new(r"C:\a")).unwrap();
        assert!(!t.node(a).no_access);
        t.set_no_access(a);
        assert!(t.node(a).no_access);
    }
}
