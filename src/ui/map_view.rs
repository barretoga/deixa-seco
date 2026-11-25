//! Mapa (treemap): desenha os tiles com o `Painter` (um retângulo por tile, sem um widget
//! por tile), hover com tooltip, clique seleciona, duplo clique entra na pasta com zoom.

use super::nav::Nav;
use super::search::{SearchIndex, SearchState};
use crate::format::{format_bytes, format_count, format_date};
use crate::kind::{FileKind, KindHist, dominant_in, kind_of};
use crate::tree::{NodeId, Tree};
use crate::treemap::{self, LayoutOpts, Tile, TileKind};
use eframe::egui::{self, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use eframe::epaint::text::{LayoutJob, TextWrapping};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Duração da animação de zoom ao entrar numa pasta.
pub const ZOOM_SECS: f64 = 0.15;

/// Duração do destaque pulsante de um resultado de busca.
pub const PULSE_SECS: f64 = 1.0;

/// O que o mapa pede para o resto da UI fazer. As ações de shell (Explorer, copiar,
/// terminal, propriedades) o próprio mapa executa; estas dependem de outras telas.
#[derive(Debug, PartialEq, Eq)]
pub enum MapAction {
    None,
    /// "Mandar para a Lixeira…": abre o diálogo de confirmação.
    Recycle(NodeId),
    /// "Por que isso está aqui?": abre o painel de sugestões na sugestão do nó.
    Explain(NodeId),
}

/// Estado da tela do mapa.
pub struct MapState {
    /// Compartilhada com a thread que recalcula as sugestões; mude só por `tree_mut`.
    pub tree: Arc<Tree>,
    pub nav: Nav,
    pub selected: Option<NodeId>,
    pub opts: LayoutOpts,
    /// Nós destacados (sugestão clicada no painel). Nó sem tile próprio destaca o ancestral
    /// desenhado.
    pub highlight: HashSet<NodeId>,
    /// Nós das sugestões: eles e tudo dentro deles mostram "Por que isso está aqui?"
    /// (`is_explainable`); o tile de cada um ganha uma marca no canto. Mude por
    /// `set_explainable` (refaz as marcas).
    explainable: HashSet<NodeId>,
    /// Nós de sugestão cujo tile ganha marca (ou o bloco desenhado mais próximo, "contém
    /// sugestão"). Subconjunto de `explainable`: "Programas instalados" explica, mas não marca.
    marked: HashSet<NodeId>,
    /// Busca (Ctrl+F).
    pub search: SearchState,
    /// Nomes dobrados para a busca em outra thread (montado junto com o mapa).
    search_index: Arc<SearchIndex>,
    /// Tile destacado com um pulso, e quando começou (segundos do relógio do egui).
    pulse: Option<(NodeId, f64)>,
    /// Aviso curto de uma ação de shell que falhou (mostrado no rodapé).
    pub notice: Option<String>,
    /// Alvo do menu de contexto aberto.
    menu_target: Option<NodeId>,
    /// Tipo que mais ocupa espaço em cada nó (`dominant_kinds`), indexado por `NodeId`.
    kinds: Vec<FileKind>,
    /// Sobe a cada mudança da árvore (ou das cores); faz parte da chave do cache de layout.
    tree_rev: u64,
    /// Sobe a cada troca de `explainable`; faz parte da chave do cache (marcas dos tiles).
    explain_rev: u64,
    cache: Option<LayoutCache>,
    zoom: Option<Zoom>,
}

#[derive(Clone, Copy, PartialEq)]
struct CacheKey {
    root: NodeId,
    w: f32,
    h: f32,
    depth: u8,
    rev: u64,
    explain_rev: u64,
}

/// Como um tile é desenhado.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Style {
    File,
    /// Pasta com faixa de título e filhos dentro.
    HeaderDir,
    /// Pasta no limite de profundidade (ou pequena demais): bloco cheio na cor do conteúdo.
    CappedDir,
    Others,
}

/// Cores de um tile, calculadas junto com o layout. Índice 0 = tema claro, 1 = escuro.
#[derive(Clone, Copy)]
struct TileLook {
    style: Style,
    fill: [Color32; 2],
    /// HeaderDir: corpo; CappedDir: aba de pasta no topo; demais: igual a `fill`.
    accent: [Color32; 2],
    text: [Color32; 2],
    border: [Color32; 2],
    /// O nó do tile é de uma sugestão: marca no canto (calculada junto com o layout).
    marked: bool,
    /// Alguma sugestão dentro deste bloco não foi desenhada (profundidade ou agrupada em
    /// "Others"): este é o bloco desenhado mais próximo dela.
    contains_marked: bool,
}

struct LayoutCache {
    key: CacheKey,
    tiles: Vec<Tile>,
    looks: Vec<TileLook>,
}

/// Zoom em andamento: o retângulo (em coordenadas da área) do tile clicado no layout
/// antigo, e o layout antigo, que fica escurecido por baixo enquanto o novo cresce.
struct Zoom {
    from: treemap::Rect,
    start: f64,
    prev: Option<LayoutCache>,
}

impl MapState {
    pub fn new(tree: Tree) -> MapState {
        MapState {
            nav: Nav::new(tree.root()),
            kinds: dominant_kinds(&tree),
            search_index: Arc::new(SearchIndex::build(&tree)),
            tree: Arc::new(tree),
            selected: None,
            opts: LayoutOpts::default(),
            highlight: HashSet::new(),
            explainable: HashSet::new(),
            marked: HashSet::new(),
            search: SearchState::default(),
            pulse: None,
            notice: None,
            menu_target: None,
            tree_rev: 0,
            explain_rev: 0,
            cache: None,
            zoom: None,
        }
    }

    /// Árvore para mudar (`remove`, `replace_subtree`). Se uma thread ainda tem a árvore,
    /// copia antes (a thread fica com a versão antiga).
    pub fn tree_mut(&mut self) -> &mut Tree {
        Arc::make_mut(&mut self.tree)
    }

    /// O nó ou um ancestral dele faz parte de uma sugestão.
    pub fn is_explainable(&self, id: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(n) = cur {
            if self.explainable.contains(&n) {
                return true;
            }
            cur = self.tree.node(n).parent;
        }
        false
    }

    /// Nós das sugestões (ver `explainable`).
    pub fn explainable(&self) -> &HashSet<NodeId> {
        &self.explainable
    }

    /// Troca os nós das sugestões e refaz as marcas dos tiles (todos marcados).
    pub fn set_explainable(&mut self, set: HashSet<NodeId>) {
        let marked = set.clone();
        self.set_explainable_marked(set, marked);
    }

    /// Troca os nós das sugestões (`explain`) e os que ganham marca no mapa (`marked`).
    pub fn set_explainable_marked(&mut self, explain: HashSet<NodeId>, marked: HashSet<NodeId>) {
        if explain != self.explainable || marked != self.marked {
            self.explainable = explain;
            self.marked = marked;
            self.explain_rev += 1;
        }
    }

    /// Troca o índice da busca (refeito numa thread depois de `replace_subtree`).
    pub fn set_search_index(&mut self, index: SearchIndex) {
        self.search_index = Arc::new(index);
    }

    /// Chamar depois de `Tree::remove`/`replace_subtree`: refaz o layout, corrige o histórico e
    /// descarta seleção e destaques que morreram. As cores por tipo não são refeitas aqui (é
    /// uma passada na árvore toda): o recálculo em outra thread as manda (`set_kinds`). Até
    /// lá valem as antigas; ids mortos não são desenhados e ids novos caem no padrão.
    pub fn after_remove(&mut self) {
        self.tree_rev += 1;
        self.zoom = None;
        self.nav.fix_after_remove(&self.tree);
        let tree = &self.tree;
        self.selected = self.selected.filter(|&id| tree.node(id).alive);
        self.highlight.retain(|&id| tree.node(id).alive);
        self.explainable.retain(|&id| tree.node(id).alive);
        self.marked.retain(|&id| tree.node(id).alive);
        self.pulse = self.pulse.filter(|&(id, _)| tree.node(id).alive);
        self.menu_target = self.menu_target.filter(|&id| tree.node(id).alive);
        self.keep_selection_inside();
    }

    /// Navega sem animação (barra de caminho, busca).
    pub fn go(&mut self, id: NodeId) {
        self.zoom = None;
        self.nav.go(id);
        self.keep_selection_inside();
    }

    pub fn back(&mut self) -> bool {
        self.zoom = None;
        let moved = self.nav.back();
        self.keep_selection_inside();
        moved
    }

    pub fn forward(&mut self) -> bool {
        self.zoom = None;
        let moved = self.nav.forward();
        self.keep_selection_inside();
        moved
    }

    /// Índice de nomes da busca (para a thread de busca).
    pub fn search_index(&self) -> &Arc<SearchIndex> {
        &self.search_index
    }

    /// Mostra um resultado de busca: vai para a pasta dele, seleciona e pulsa o tile.
    pub fn reveal(&mut self, id: NodeId, now: f64) {
        if !self.tree.node(id).alive {
            return;
        }
        let target = self.tree.node(id).parent.unwrap_or(id);
        self.go(target);
        self.selected = Some(id);
        self.pulse = Some((id, now));
    }

    /// Troca as cores por tipo (`dominant_kinds` da árvore atual, calculado numa thread) e
    /// refaz as cores dos tiles.
    pub fn set_kinds(&mut self, kinds: Vec<FileKind>) {
        self.kinds = kinds;
        self.tree_rev += 1;
    }

    /// Tipo que mais ocupa espaço no nó.
    pub fn dominant_kind(&self, id: NodeId) -> FileKind {
        self.kinds
            .get(id.0 as usize)
            .copied()
            .unwrap_or(FileKind::Folder)
    }

    /// A seleção só sobrevive se continua visível dentro da pasta atual.
    fn keep_selection_inside(&mut self) {
        let cur = self.nav.current();
        let tree = &self.tree;
        self.selected = self
            .selected
            .filter(|&s| s != cur && tree.node(s).alive && tree.ancestors(s).contains(&cur));
    }

    /// Refaz o layout só se raiz, tamanho da área, profundidade ou árvore mudaram.
    fn ensure_layout(&mut self, size: Vec2) {
        let key = CacheKey {
            root: self.nav.current(),
            w: size.x,
            h: size.y,
            depth: self.opts.max_depth,
            rev: self.tree_rev,
            explain_rev: self.explain_rev,
        };
        if self.cache.as_ref().is_some_and(|c| c.key == key) {
            return;
        }
        self.cache = Some(build_layout(
            &self.tree,
            &self.kinds,
            key,
            &self.opts,
            &self.marked,
        ));
    }
}

/// Layout e cores dos tiles para `key` (raiz e tamanho da área). `explainable`: nós marcados das
/// sugestões; o tile de cada um ganha `marked`, e o bloco desenhado mais próximo de cada
/// sugestão não desenhada ganha `contains_marked`. Custo O(sugestões × profundidade + tiles).
fn build_layout(
    tree: &Tree,
    kinds: &[FileKind],
    key: CacheKey,
    opts: &LayoutOpts,
    explainable: &HashSet<NodeId>,
) -> LayoutCache {
    let area = treemap::Rect {
        x: 0.0,
        y: 0.0,
        w: key.w,
        h: key.h,
    };
    let tiles = treemap::layout(tree, key.root, area, opts);
    let mut contains = vec![false; tiles.len()];
    if !explainable.is_empty() {
        let mut node_tile: HashMap<NodeId, usize> = HashMap::new();
        let mut others_tile: HashMap<NodeId, usize> = HashMap::new();
        for (i, t) in tiles.iter().enumerate() {
            match t.kind {
                TileKind::Node(id) => {
                    node_tile.insert(id, i);
                }
                TileKind::Others { parent, .. } => {
                    others_tile.insert(parent, i);
                }
            }
        }
        for &e in explainable {
            if node_tile.contains_key(&e) {
                continue;
            }
            let mut cur = e;
            while let Some(parent) = tree.node(cur).parent {
                if let Some(&i) = others_tile.get(&parent).or_else(|| node_tile.get(&parent)) {
                    contains[i] = true;
                    break;
                }
                cur = parent;
            }
        }
    }
    let looks = tiles
        .iter()
        .zip(contains)
        .map(|(t, c)| {
            let m = matches!(t.kind, TileKind::Node(id) if explainable.contains(&id));
            tile_look(tree, kinds, t, m, c)
        })
        .collect();
    LayoutCache { key, tiles, looks }
}

/// Mapa parcial da tela de varredura: só leitura (sem clique, menu ou navegação), com
/// hover e tooltip. A árvore e as cores vêm do `Tree::preview_with_kinds` mais recente (as
/// cores contadas na árvore inteira, feitas na thread da varredura).
pub struct PreviewMap {
    tree: Tree,
    kinds: Vec<FileKind>,
    opts: LayoutOpts,
    cache: Option<LayoutCache>,
}

impl PreviewMap {
    /// `kinds`: tipo dominante por `NodeId` de `tree` (`Tree::preview_with_kinds`).
    pub fn new(tree: Tree, kinds: Vec<FileKind>) -> PreviewMap {
        PreviewMap {
            kinds,
            tree,
            opts: LayoutOpts {
                max_depth: crate::scan::PREVIEW_DEPTH,
                ..LayoutOpts::default()
            },
            cache: None,
        }
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Refaz o layout só se a área mudou de tamanho (preview novo = `PreviewMap` novo).
    fn ensure_layout(&mut self, size: Vec2) {
        let key = CacheKey {
            root: self.tree.root(),
            w: size.x,
            h: size.y,
            depth: self.opts.max_depth,
            rev: 0,
            explain_rev: 0,
        };
        if self.cache.as_ref().is_some_and(|c| c.key == key) {
            return;
        }
        self.cache = Some(build_layout(
            &self.tree,
            &self.kinds,
            key,
            &self.opts,
            &HashSet::new(),
        ));
    }
}

/// Desenha o mapa parcial com hover e tooltip (caminho e tamanho). Nada de clique.
pub fn preview_view(ui: &mut egui::Ui, map: &mut PreviewMap) {
    let (area, resp) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
    map.ensure_layout(area.size());
    let dark = ui.visuals().dark_mode;
    let origin = area.min.to_vec2();
    let painter = ui.painter_at(area);
    painter.rect_filled(area, 0.0, ui.visuals().extreme_bg_color);
    let cache = map.cache.as_ref().expect("layout calculado acima");
    let to_screen = |r: treemap::Rect| to_egui(r).translate(origin);
    paint_tiles(
        &painter,
        cache,
        &map.tree,
        dark,
        map.opts.header_px,
        &to_screen,
        true,
    );
    let hovered = resp
        .hover_pos()
        .and_then(|p| hit_test(&cache.tiles, p.x - area.min.x, p.y - area.min.y));
    if let Some(i) = hovered {
        painter.rect_stroke(
            to_screen(cache.tiles[i].rect),
            0.0,
            Stroke::new(1.5, hover_color(dark)),
            StrokeKind::Inside,
        );
        let (path, detail) = tooltip_text(&map.tree, &map.kinds, cache.tiles[i].kind);
        resp.on_hover_ui_at_pointer(|ui| {
            ui.label(egui::RichText::new(path).strong());
            ui.label(detail);
        });
    }
}

fn hover_color(dark: bool) -> Color32 {
    if dark {
        Color32::from_white_alpha(200)
    } else {
        Color32::from_black_alpha(170)
    }
}

/// Tooltip de um tile: caminho completo e, abaixo, tamanho e o que mais ocupa espaço.
fn tooltip_text(tree: &Tree, kinds: &[FileKind], kind: TileKind) -> (String, String) {
    match kind {
        TileKind::Node(id) => {
            let n = tree.node(id);
            let mut detail = format_bytes(n.size);
            if n.is_dir {
                detail.push_str(&format!(" · {} arquivos", format_count(n.files)));
                let k = kinds
                    .get(id.0 as usize)
                    .copied()
                    .unwrap_or(FileKind::Folder);
                if k != FileKind::Folder {
                    detail.push_str(&format!(" · mais espaço: {}", kind_label(k)));
                }
            }
            (tree.path(id).display().to_string(), detail)
        }
        TileKind::Others {
            parent,
            count,
            size,
        } => (
            format!("{} em {}", others_label(count), tree.path(parent).display()),
            format!("{} · pequenos demais para mostrar", format_bytes(size)),
        ),
    }
}

fn tile_look(
    tree: &Tree,
    kinds: &[FileKind],
    tile: &Tile,
    marked: bool,
    contains_marked: bool,
) -> TileLook {
    let d = tile.depth;
    let (style, fill, accent): (Style, [Color32; 2], [Color32; 2]) = match tile.kind {
        TileKind::Others { .. } => {
            let f = [others_fill(false), others_fill(true)];
            (Style::Others, f, f)
        }
        TileKind::Node(id) => {
            let kind = kinds.get(id.0 as usize).copied().unwrap_or(FileKind::Other);
            if !tree.node(id).is_dir {
                let f = [tile_fill(kind, d, false), tile_fill(kind, d, true)];
                (Style::File, f, f)
            } else if tile.has_header {
                let f = [folder_band(kind, d, false), folder_band(kind, d, true)];
                (
                    Style::HeaderDir,
                    f,
                    [body_fill(f[0], false), body_fill(f[1], true)],
                )
            } else {
                let f = [capped_fill(kind, d, false), capped_fill(kind, d, true)];
                let tab = [folder_band(kind, d, false), folder_band(kind, d, true)];
                (Style::CappedDir, f, tab)
            }
        }
    };
    TileLook {
        style,
        fill,
        accent,
        text: [text_color_for(fill[0]), text_color_for(fill[1])],
        border: fill.map(|c| c.lerp_to_gamma(Color32::BLACK, 0.4)),
        marked,
        contains_marked,
    }
}

// ---------- helpers puros (testados) ----------

/// Índice do tile mais profundo que contém o ponto (coordenadas da área).
/// Pais vêm antes dos filhos no `Vec`, então basta percorrer de trás para frente.
pub fn hit_test(tiles: &[Tile], x: f32, y: f32) -> Option<usize> {
    tiles.iter().rposition(|t| {
        let r = t.rect;
        x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
    })
}

/// Tile que representa `id` na tela: o dele, ou o do ancestral mais próximo que foi
/// desenhado; se `id` (ou o ancestral) foi agrupado, o tile `Others` da pasta dele.
pub fn visible_tile(tree: &Tree, tiles: &[Tile], id: NodeId) -> Option<usize> {
    let mut cur = id;
    loop {
        if let Some(i) = tiles.iter().position(|t| t.kind == TileKind::Node(cur)) {
            return Some(i);
        }
        let parent = tree.node(cur).parent?;
        if let Some(i) = tiles
            .iter()
            .position(|t| matches!(t.kind, TileKind::Others { parent: p, .. } if p == parent))
        {
            return Some(i);
        }
        cur = parent;
    }
}

/// Tiles a destacar: o de cada nó de `highlight`, ou o do ancestral mais próximo que foi
/// desenhado (abaixo de `current`). Sem repetição, na ordem dos tiles.
pub fn highlight_tiles(
    tree: &Tree,
    tiles: &[Tile],
    highlight: &HashSet<NodeId>,
    current: NodeId,
) -> Vec<usize> {
    let index: HashMap<NodeId, usize> = tiles
        .iter()
        .enumerate()
        .filter_map(|(i, t)| match t.kind {
            TileKind::Node(id) => Some((id, i)),
            TileKind::Others { .. } => None,
        })
        .collect();
    let mut out: Vec<usize> = Vec::new();
    for &h in highlight {
        let mut cur = h;
        while cur != current {
            if let Some(&i) = index.get(&cur) {
                out.push(i);
                break;
            }
            match tree.node(cur).parent {
                Some(p) => cur = p,
                None => break,
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Pasta em que um duplo clique no tile `idx` entra: a própria pasta, ou a pasta que
/// contém o arquivo (se não for a pasta atual). `Others` não navega.
pub fn nav_target(tree: &Tree, tiles: &[Tile], idx: usize, current: NodeId) -> Option<NodeId> {
    match tiles.get(idx)?.kind {
        TileKind::Others { .. } => None,
        TileKind::Node(id) if tree.node(id).is_dir => Some(id),
        TileKind::Node(id) => tree.node(id).parent.filter(|&p| p != current),
    }
}

/// Curva de desaceleração (cúbica) para `t` em 0..=1; fora disso é limitado.
pub fn ease_out(t: f64) -> f32 {
    let t = t.clamp(0.0, 1.0);
    (1.0 - (1.0 - t).powi(3)) as f32
}

pub fn lerp_rect(a: treemap::Rect, b: treemap::Rect, t: f32) -> treemap::Rect {
    let l = |p: f32, q: f32| p + (q - p) * t;
    treemap::Rect {
        x: l(a.x, b.x),
        y: l(a.y, b.y),
        w: l(a.w, b.w),
        h: l(a.h, b.h),
    }
}

pub use crate::kind::ALL_KINDS;

/// Nome do tipo na legenda.
pub fn kind_label(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Folder => "Pasta",
        FileKind::Video => "Vídeo",
        FileKind::Image => "Imagem",
        FileKind::Audio => "Áudio",
        FileKind::Archive => "Compactado",
        FileKind::Code => "Código",
        FileKind::Executable => "Executável",
        FileKind::Document => "Documento",
        FileKind::Cache => "Cache",
        FileKind::GameData => "Jogos e dados",
        FileKind::Other => "Outros",
    }
}

/// Tipo de arquivo que mais ocupa bytes em cada nó, indexado por `NodeId`. Arquivo: o
/// próprio tipo. Pasta: o tipo com mais bytes no conteúdo todo; sem bytes, `Folder`.
/// Um passe de trás para frente (filhos têm id maior que o pai). Cada pasta viva tem um
/// histograma num `Vec` denso (índice da pasta por `NodeId`, sem `HashMap`). Custa uma
/// passada na árvore toda: chame fora da thread da UI.
pub fn dominant_kinds(tree: &Tree) -> Vec<FileKind> {
    type Hist = KindHist;
    const NO_DIR: u32 = u32::MAX;
    let n = tree.len();
    // Índice denso de cada pasta viva (`NO_DIR` para arquivos e nós mortos).
    let mut dir_slot = vec![NO_DIR; n];
    let mut dirs = 0u32;
    for (i, slot) in dir_slot.iter_mut().enumerate() {
        let node = tree.node(NodeId(i as u32));
        if node.alive && node.is_dir {
            *slot = dirs;
            dirs += 1;
        }
    }
    let mut hists: Vec<Hist> = vec![[0; ALL_KINDS.len()]; dirs as usize];
    let argmax = dominant_in;
    // Histograma do pai (sempre uma pasta viva numa árvore bem formada).
    let parent_slot = |parent: Option<NodeId>| {
        parent
            .map(|p| dir_slot[p.0 as usize])
            .filter(|&s| s != NO_DIR)
    };
    let mut out = vec![FileKind::Folder; n];
    for i in (0..n).rev() {
        let node = tree.node(NodeId(i as u32));
        if !node.alive {
            continue;
        }
        if node.is_dir {
            let h = hists[dir_slot[i] as usize];
            out[i] = argmax(&h);
            if let Some(p) = parent_slot(node.parent) {
                for (a, b) in hists[p as usize].iter_mut().zip(h) {
                    *a += b;
                }
            }
        } else {
            let k = kind_of(&node.name, false);
            out[i] = k;
            if let Some(p) = parent_slot(node.parent) {
                hists[p as usize][k as usize] += node.size;
            }
        }
    }
    out
}

/// Cor base (tema claro) de cada tipo de arquivo.
fn base_color(kind: FileKind) -> Color32 {
    match kind {
        FileKind::Folder => Color32::from_rgb(96, 120, 152),
        FileKind::Video => Color32::from_rgb(220, 92, 80),
        FileKind::Image => Color32::from_rgb(72, 168, 104),
        FileKind::Audio => Color32::from_rgb(152, 104, 200),
        FileKind::Archive => Color32::from_rgb(232, 156, 52),
        FileKind::Code => Color32::from_rgb(52, 168, 180),
        FileKind::Executable => Color32::from_rgb(208, 82, 160),
        FileKind::Document => Color32::from_rgb(184, 178, 66),
        FileKind::Cache => Color32::from_rgb(140, 100, 70),
        FileKind::GameData => Color32::from_rgb(66, 108, 224),
        FileKind::Other => Color32::from_rgb(186, 168, 132),
    }
}

const NEAR_BLACK: Color32 = Color32::from_rgb(18, 18, 22);
const NEAR_WHITE: Color32 = Color32::from_rgb(250, 250, 252);

/// Cor de preenchimento de um tile: tipo + tema + profundidade. No tema claro os tiles são
/// claros (texto escuro); no escuro, fechados (texto claro). Pastas alternam um tom mais
/// claro nos níveis pares (e clareiam um pouco a cada nível), para a pasta de dentro
/// se destacar da de fora; arquivos escurecem pouco com a profundidade.
pub fn tile_fill(kind: FileKind, depth: u8, dark: bool) -> Color32 {
    let base = base_color(kind);
    let level = f32::from(depth.saturating_sub(1));
    let even = depth.is_multiple_of(2);
    if kind == FileKind::Folder {
        if dark {
            let c = base.lerp_to_gamma(Color32::BLACK, 0.38);
            let t = if even { 0.13 } else { 0.0 } + (level * 0.015).min(0.08);
            c.lerp_to_gamma(Color32::WHITE, t)
        } else {
            let t = if even { 0.46 } else { 0.30 } + (level * 0.03).min(0.15);
            base.lerp_to_gamma(Color32::WHITE, t)
        }
    } else {
        let c = if dark {
            base.lerp_to_gamma(Color32::BLACK, 0.38)
        } else {
            base.lerp_to_gamma(Color32::WHITE, 0.30)
        };
        c.lerp_to_gamma(Color32::BLACK, (level * 0.025).min(0.12))
    }
}

/// Faixa de título de uma pasta: cor de pasta puxada para o tipo que domina o conteúdo.
pub fn folder_band(dominant: FileKind, depth: u8, dark: bool) -> Color32 {
    let folder = tile_fill(FileKind::Folder, depth, dark);
    if dominant == FileKind::Folder {
        return folder;
    }
    folder.lerp_to_gamma(tile_fill(dominant, depth, dark), 0.45)
}

/// Pasta no limite de profundidade: cor do tipo dominante, um pouco puxada para a cor de
/// pasta (fica reconhecível como pasta e não se confunde com um arquivo daquele tipo).
pub fn capped_fill(dominant: FileKind, depth: u8, dark: bool) -> Color32 {
    let folder = tile_fill(FileKind::Folder, depth, dark);
    if dominant == FileKind::Folder {
        return folder;
    }
    tile_fill(dominant, depth, dark).lerp_to_gamma(folder, 0.25)
}

/// Fundo do corpo de uma pasta com faixa de título (mais escuro que a faixa, para o
/// contorno dos filhos aparecer).
pub fn body_fill(fill: Color32, dark: bool) -> Color32 {
    fill.lerp_to_gamma(Color32::BLACK, if dark { 0.32 } else { 0.20 })
}

/// Cor do tile "outros (N itens)".
pub fn others_fill(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(78, 80, 86)
    } else {
        Color32::from_rgb(176, 178, 184)
    }
}

/// Luminância relativa (WCAG) de uma cor sRGB.
pub fn luminance(c: Color32) -> f32 {
    let ch = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
}

/// Razão de contraste WCAG entre duas cores (1 a 21).
pub fn contrast(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Texto escuro ou claro, o que tiver mais contraste com o fundo.
pub fn text_color_for(bg: Color32) -> Color32 {
    if contrast(NEAR_BLACK, bg) >= contrast(NEAR_WHITE, bg) {
        NEAR_BLACK
    } else {
        NEAR_WHITE
    }
}

/// Rodapé: `caminho · tamanho · N arquivos · mod. dd/mm/aaaa`.
pub fn footer_text(tree: &Tree, id: NodeId) -> String {
    let n = tree.node(id);
    let files = if n.files == 1 {
        "1 arquivo".to_string()
    } else {
        format!("{} arquivos", format_count(n.files))
    };
    let mut s = format!(
        "{} · {} · {}",
        tree.path(id).display(),
        format_bytes(n.size),
        files
    );
    if let Some(t) = n.mtime {
        s.push_str(" · mod. ");
        s.push_str(&format_date(t));
    }
    if n.no_access {
        s.push_str(" · sem acesso a parte do conteúdo");
    }
    s
}

/// Texto do tile `Others`.
pub fn others_label(count: u32) -> String {
    if count == 1 {
        "outros (1 item)".to_string()
    } else {
        format!("outros ({} itens)", format_count(u64::from(count)))
    }
}

// ---------- desenho ----------

/// Lado do quadrado de cor da legenda.
const LEGEND_SWATCH: f32 = 14.0;

/// Legenda: um quadrado de cor por tipo, com o nome em pt-BR em texto normal, e a marca
/// dos blocos que aparecem nas sugestões.
pub fn legend(ui: &mut egui::Ui) {
    let dark = ui.visuals().dark_mode;
    let swatch = |ui: &mut egui::Ui, fill: Color32| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(LEGEND_SWATCH), Sense::hover());
        ui.painter().rect_filled(r, 3.0, fill);
        ui.painter().rect_stroke(
            r,
            3.0,
            Stroke::new(1.0, fill.lerp_to_gamma(Color32::BLACK, 0.4)),
            StrokeKind::Inside,
        );
        r
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        for kind in ALL_KINDS {
            swatch(ui, tile_fill(kind, 1, dark));
            ui.label(egui::RichText::new(kind_label(kind)).size(13.0));
            ui.add_space(8.0);
        }
        let fill = others_fill(dark);
        let r = swatch(ui, fill);
        paint_mark(ui.painter(), r, text_color_for(fill));
        ui.label(egui::RichText::new("Em sugestão").size(13.0))
            .on_hover_text("O bloco aparece no painel de sugestões");
        ui.add_space(8.0);
        let r = swatch(ui, fill);
        paint_contains_mark(ui.painter(), r, text_color_for(fill));
        ui.label(egui::RichText::new("Contém sugestão").size(13.0))
            .on_hover_text(
                "Uma sugestão está dentro deste bloco, escondida (muito pequena ou funda)",
            );
    });
}

/// Marca de bloco que aparece nas sugestões: triângulo no canto superior direito, na cor do
/// texto do tile (contraste garantido com o fundo). Diferente do contorno laranja do
/// destaque e do amarelo da seleção.
fn paint_mark(painter: &egui::Painter, r: Rect, color: Color32) {
    let side = (r.width().min(r.height()) * 0.45).min(10.0);
    if side < 5.0 {
        return;
    }
    let corner = r.right_top() + Vec2::new(-1.0, 1.0);
    painter.add(egui::Shape::convex_polygon(
        vec![
            corner,
            corner + Vec2::new(0.0, side),
            corner - Vec2::new(side, 0.0),
        ],
        color,
        Stroke::NONE,
    ));
}

/// Marca de bloco que contém uma sugestão não desenhada: ponto no canto inferior esquerdo
/// (oposto ao triângulo), na cor do texto do tile.
fn paint_contains_mark(painter: &egui::Painter, r: Rect, color: Color32) {
    let radius = (r.width().min(r.height()) * 0.2).min(3.5);
    if radius < 2.0 {
        return;
    }
    let center = r.left_bottom() + Vec2::new(radius + 2.0, -radius - 2.0);
    painter.circle_filled(center, radius, color);
}

fn to_egui(r: treemap::Rect) -> Rect {
    Rect::from_min_size(Pos2::new(r.x, r.y), Vec2::new(r.w, r.h))
}

/// Uma linha de texto truncada com "…" na largura do retângulo; não desenha se não couber.
fn draw_line(painter: &egui::Painter, rect: Rect, text: String, font: FontId, color: Color32) {
    let max_w = rect.width() - 6.0;
    if max_w < 14.0 || rect.height() < font.size + 2.0 {
        return;
    }
    let mut job = LayoutJob::simple_singleline(text, font, color);
    job.wrap = TextWrapping::truncate_at_width(max_w);
    let galley = painter.layout_job(job);
    let y = rect.top() + (rect.height() - galley.size().y) / 2.0;
    painter.galley(Pos2::new(rect.left() + 4.0, y), galley, color);
}

const LEAF_LINE: f32 = 15.0;

/// Desenha um layout. `xform` leva o retângulo do tile para a tela. Os textos só são
/// montados para tiles grandes o bastante para mostrá-los.
fn paint_tiles(
    painter: &egui::Painter,
    cache: &LayoutCache,
    tree: &Tree,
    dark: bool,
    header_px: f32,
    xform: &dyn Fn(treemap::Rect) -> Rect,
    draw_text: bool,
) {
    let t = usize::from(dark);
    let header_font = FontId::proportional(12.5);
    let leaf_font = FontId::proportional(11.5);
    for (tile, look) in cache.tiles.iter().zip(&cache.looks) {
        let r = xform(tile.rect);
        if r.width() < 0.5 || r.height() < 0.5 {
            continue;
        }
        painter.rect_filled(r, 0.0, look.fill[t]);
        match look.style {
            Style::HeaderDir => {
                let band_h = header_px * r.height() / tile.rect.h.max(1.0);
                let band = Rect::from_min_size(r.min, Vec2::new(r.width(), band_h));
                let body = Rect::from_min_max(Pos2::new(r.left(), band.bottom()), r.max);
                painter.rect_filled(body, 0.0, look.accent[t]);
            }
            Style::CappedDir if r.height() >= 12.0 => {
                let tab = Rect::from_min_size(r.min, Vec2::new(r.width(), 3.0));
                painter.rect_filled(tab, 0.0, look.accent[t]);
            }
            _ => {}
        }
        painter.rect_stroke(r, 0.0, Stroke::new(1.0, look.border[t]), StrokeKind::Inside);
        if look.marked {
            paint_mark(painter, r, look.text[t]);
        }
        if look.contains_marked {
            paint_contains_mark(painter, r, look.text[t]);
        }
        if !draw_text || r.width() < 30.0 {
            continue;
        }
        let color = look.text[t];
        let (name, size): (&str, u64) = match tile.kind {
            TileKind::Node(id) => {
                let n = tree.node(id);
                (&n.name, n.size)
            }
            TileKind::Others { size, .. } => ("", size),
        };
        if look.style == Style::HeaderDir {
            let band = Rect::from_min_size(r.min, Vec2::new(r.width(), header_px));
            let text = format!("{name} · {}", format_bytes(size));
            draw_line(painter, band, text, header_font.clone(), color);
        } else if r.height() >= LEAF_LINE {
            let first = match tile.kind {
                TileKind::Others { count, .. } => others_label(count),
                TileKind::Node(_) => name.to_string(),
            };
            let top =
                Rect::from_min_size(r.min + Vec2::new(0.0, 2.0), Vec2::new(r.width(), LEAF_LINE));
            draw_line(painter, top, first, leaf_font.clone(), color);
            if r.height() >= 2.0 * LEAF_LINE + 4.0 {
                let second = top.translate(Vec2::new(0.0, LEAF_LINE - 1.0));
                let dim = color.gamma_multiply(0.8);
                draw_line(painter, second, format_bytes(size), leaf_font.clone(), dim);
            }
        }
    }
}

/// Desenha o mapa e trata hover, clique e duplo clique.
pub fn map_view(ui: &mut egui::Ui, state: &mut MapState) -> MapAction {
    let (area, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click());
    let size = area.size();
    state.ensure_layout(size);
    let dark = ui.visuals().dark_mode;
    let now = ui.input(|i| i.time);
    let origin = area.min.to_vec2();

    // Progresso do zoom: o layout novo é desenhado dentro de um retângulo que cresce
    // do tile clicado até a área toda, por cima do layout antigo escurecido.
    let full = treemap::Rect {
        x: 0.0,
        y: 0.0,
        w: size.x,
        h: size.y,
    };
    let view =
        match &state.zoom {
            Some(z) if now - z.start < ZOOM_SECS && size.x > 0.0 && size.y > 0.0 => Some(
                lerp_rect(z.from, full, ease_out((now - z.start) / ZOOM_SECS)),
            ),
            _ => None,
        };
    if view.is_some() {
        ui.ctx().request_repaint();
    } else {
        state.zoom = None;
    }
    let to_screen = |r: treemap::Rect| -> Rect {
        let r = match view {
            Some(v) => treemap::Rect {
                x: v.x + r.x * v.w / size.x,
                y: v.y + r.y * v.h / size.y,
                w: r.w * v.w / size.x,
                h: r.h * v.h / size.y,
            },
            None => r,
        };
        to_egui(r).translate(origin)
    };

    let painter = ui.painter_at(area);
    let bg = ui.visuals().extreme_bg_color;
    painter.rect_filled(area, 0.0, bg);

    let tree = &state.tree;
    let cache = state.cache.as_ref().expect("layout calculado acima");
    let tiles = &cache.tiles;
    let current = state.nav.current();
    let header_px = state.opts.header_px;

    if let (Some(v), Some(prev)) = (view, state.zoom.as_ref().and_then(|z| z.prev.as_ref())) {
        let identity = |r: treemap::Rect| to_egui(r).translate(origin);
        paint_tiles(&painter, prev, tree, dark, header_px, &identity, false);
        painter.rect_filled(area, 0.0, Color32::from_black_alpha(110));
        painter.rect_filled(to_egui(v).translate(origin), 0.0, bg);
    }

    if tiles.is_empty() && view.is_none() {
        let msg = if tree.node(current).size == 0 {
            "Pasta vazia"
        } else {
            "Área pequena demais para o mapa"
        };
        painter.text(
            area.center(),
            egui::Align2::CENTER_CENTER,
            msg,
            FontId::proportional(16.0),
            ui.visuals().weak_text_color(),
        );
    }
    paint_tiles(
        &painter,
        cache,
        tree,
        dark,
        header_px,
        &to_screen,
        view.is_none(),
    );

    // Destaques (Task 11 / busca), depois a seleção por cima de tudo.
    let rect_of = |id: NodeId| {
        tiles
            .iter()
            .find(|t| t.kind == TileKind::Node(id))
            .map(|t| to_screen(t.rect))
    };
    if !state.highlight.is_empty() {
        let stroke = Stroke::new(2.0, Color32::from_rgb(255, 128, 32));
        for i in highlight_tiles(tree, tiles, &state.highlight, current) {
            painter.rect_stroke(to_screen(tiles[i].rect), 0.0, stroke, StrokeKind::Inside);
        }
    }

    // Hover e cliques só fora da animação (o layout desenhado ainda está se mexendo).
    let hovered = match (view, resp.hover_pos()) {
        (None, Some(p)) => hit_test(tiles, p.x - area.min.x, p.y - area.min.y),
        _ => None,
    };
    if let Some(i) = hovered {
        painter.rect_stroke(
            to_screen(tiles[i].rect),
            0.0,
            Stroke::new(1.5, hover_color(dark)),
            StrokeKind::Inside,
        );
    }
    if let Some(r) = state.selected.and_then(rect_of) {
        painter.rect_stroke(
            r,
            0.0,
            Stroke::new(4.0, Color32::from_black_alpha(170)),
            StrokeKind::Inside,
        );
        painter.rect_stroke(
            r.shrink(1.0),
            0.0,
            Stroke::new(2.0, Color32::from_rgb(255, 214, 40)),
            StrokeKind::Inside,
        );
    }

    if let Some((id, start)) = state.pulse {
        let t = now - start;
        if t < PULSE_SECS
            && let Some(r) = visible_tile(&state.tree, tiles, id).map(|i| to_screen(tiles[i].rect))
        {
            // Três pulsos em 1 s; o brilho cai no fim.
            let wave = (t * std::f64::consts::TAU * 3.0).sin() * 0.5 + 0.5;
            let fade = 1.0 - t / PULSE_SECS;
            let alpha = (255.0 * (0.25 + 0.75 * wave) * (0.4 + 0.6 * fade)) as u8;
            let color = Color32::from_rgba_unmultiplied(255, 64, 32, alpha);
            painter.rect_stroke(
                r.shrink(2.0),
                0.0,
                Stroke::new(5.0, color),
                StrokeKind::Inside,
            );
            ui.ctx().request_repaint();
        } else {
            state.pulse = None;
        }
    }

    let pointer_hit = || {
        resp.interact_pointer_pos()
            .and_then(|p| hit_test(tiles, p.x - area.min.x, p.y - area.min.y))
    };
    let mut select: Option<Option<NodeId>> = None;
    let mut enter: Option<(NodeId, treemap::Rect)> = None;
    if view.is_none() {
        if resp.clicked() {
            select = Some(pointer_hit().and_then(|i| match tiles[i].kind {
                TileKind::Node(id) => Some(id),
                TileKind::Others { .. } => None,
            }));
        }
        if resp.double_clicked()
            && let Some(i) = pointer_hit()
            && let Some(target) = nav_target(tree, tiles, i, current)
        {
            let from = tiles
                .iter()
                .find(|t| t.kind == TileKind::Node(target))
                .map_or(full, |t| t.rect);
            enter = Some((target, from));
        }
    }

    // Botão direito: seleciona o tile e abre o menu (`Others` não é um caminho real).
    if view.is_none() && resp.secondary_clicked() {
        let hit = pointer_hit().and_then(|i| match tiles[i].kind {
            TileKind::Node(id) => Some(id),
            TileKind::Others { .. } => None,
        });
        state.menu_target = hit;
        if hit.is_some() {
            select = Some(hit);
        }
    }
    let mut command = None;
    if let Some(target) = state.menu_target {
        resp.context_menu(|ui| {
            command = context_menu(ui, tree, target, state.is_explainable(target));
        });
    }

    // Tooltip: caminho completo, tamanho e o que mais ocupa espaço.
    let tooltip = hovered.map(|i| tooltip_text(tree, &state.kinds, tiles[i].kind));
    if let Some((path, detail)) = tooltip {
        resp.on_hover_ui_at_pointer(|ui| {
            ui.label(egui::RichText::new(path).strong());
            ui.label(detail);
        });
    }

    if let Some(sel) = select {
        state.selected = sel;
        state.notice = None;
    }
    let mut action = MapAction::None;
    if let (Some(cmd), Some(target)) = (command, state.menu_target) {
        state.notice = None;
        match cmd {
            MenuCommand::Recycle => action = MapAction::Recycle(target),
            MenuCommand::Explain => action = MapAction::Explain(target),
            shell_cmd => state.notice = run_shell(ui.ctx(), &state.tree, target, shell_cmd),
        }
    }
    if let Some((target, from)) = enter {
        state.nav.go(target);
        state.keep_selection_inside();
        let prev = state.cache.take();
        state.zoom = Some(Zoom {
            from,
            start: now,
            prev,
        });
        ui.ctx().request_repaint();
    }
    action
}

/// Item escolhido no menu de contexto.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuCommand {
    Explorer,
    CopyPath,
    Terminal,
    Properties,
    Recycle,
    Explain,
}

/// Conteúdo do menu de contexto de um tile, na ordem do brief.
fn context_menu(
    ui: &mut egui::Ui,
    tree: &Tree,
    id: NodeId,
    explainable: bool,
) -> Option<MenuCommand> {
    let mut chosen = None;
    let mut item = |ui: &mut egui::Ui, label: &str, cmd: MenuCommand| {
        if ui.button(label).clicked() {
            chosen = Some(cmd);
            ui.close();
        }
    };
    ui.label(egui::RichText::new(&*tree.node(id).name).weak());
    ui.separator();
    item(ui, "Abrir no Explorer", MenuCommand::Explorer);
    item(ui, "Copiar caminho", MenuCommand::CopyPath);
    item(ui, "Abrir terminal aqui", MenuCommand::Terminal);
    item(ui, "Propriedades", MenuCommand::Properties);
    ui.separator();
    item(ui, "Mandar para a Lixeira…", MenuCommand::Recycle);
    if explainable {
        item(ui, "Por que isso está aqui?", MenuCommand::Explain);
    }
    chosen
}

/// Executa uma ação de shell. Devolve a mensagem de erro (pt-BR) se falhou.
fn run_shell(ctx: &egui::Context, tree: &Tree, id: NodeId, cmd: MenuCommand) -> Option<String> {
    let path = tree.path(id);
    let is_dir = tree.node(id).is_dir;
    match cmd {
        MenuCommand::Explorer => crate::shell::open_in_explorer(&path)
            .err()
            .map(|_| "Não consegui abrir o Explorer".to_string()),
        MenuCommand::CopyPath => {
            ctx.copy_text(path.display().to_string());
            None
        }
        MenuCommand::Terminal => {
            crate::shell::open_terminal(&crate::shell::terminal_dir(&path, is_dir))
                .err()
                .map(|_| "Não consegui abrir o terminal".to_string())
        }
        MenuCommand::Properties => (!crate::shell::show_properties(&path))
            .then(|| "Não consegui abrir as propriedades".to_string()),
        MenuCommand::Recycle | MenuCommand::Explain => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    fn rect(x: f32, y: f32, w: f32, h: f32) -> treemap::Rect {
        treemap::Rect { x, y, w, h }
    }

    fn sample() -> Tree {
        // C:\ { São Paulo\ { foto.jpg 30 }, a.bin 10 }
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let mtime = Some(UNIX_EPOCH + Duration::from_secs(1_795_435_200));
        let s = t.add(r, "São Paulo", true, 0, mtime);
        t.add(s, "foto.jpg", false, 30, mtime);
        t.add(r, "a.bin", false, 10, None);
        t.finalize();
        t
    }

    fn tiles_for(t: &Tree) -> Vec<Tile> {
        let s = t.find(Path::new(r"C:\São Paulo")).unwrap();
        let f = t.find(Path::new(r"C:\São Paulo\foto.jpg")).unwrap();
        let a = t.find(Path::new(r"C:\a.bin")).unwrap();
        let tile = |kind, rect, depth| Tile {
            kind,
            rect,
            depth,
            has_header: false,
        };
        vec![
            tile(TileKind::Node(s), rect(0.0, 0.0, 60.0, 100.0), 1),
            tile(TileKind::Node(f), rect(2.0, 20.0, 56.0, 78.0), 2),
            tile(TileKind::Node(a), rect(60.0, 0.0, 30.0, 100.0), 1),
            tile(
                TileKind::Others {
                    parent: t.root(),
                    count: 3,
                    size: 1,
                },
                rect(90.0, 0.0, 10.0, 100.0),
                1,
            ),
        ]
    }

    #[test]
    fn hit_test_finds_deepest() {
        let t = sample();
        let tiles = tiles_for(&t);
        assert_eq!(hit_test(&tiles, 10.0, 50.0), Some(1)); // foto dentro de São Paulo
        assert_eq!(hit_test(&tiles, 10.0, 5.0), Some(0)); // header de São Paulo
        assert_eq!(hit_test(&tiles, 70.0, 5.0), Some(2));
        assert_eq!(hit_test(&tiles, 95.0, 5.0), Some(3));
        assert_eq!(hit_test(&tiles, 100.0, 5.0), None); // borda direita é exclusiva
        assert_eq!(hit_test(&tiles, -1.0, 5.0), None);
        assert_eq!(hit_test(&[], 1.0, 1.0), None);
    }

    #[test]
    fn visible_tile_falls_back_to_ancestor_or_others() {
        let t = sample();
        let tiles = tiles_for(&t);
        let foto = t.find(Path::new(r"C:\São Paulo\foto.jpg")).unwrap();
        assert_eq!(visible_tile(&t, &tiles, foto), Some(1), "o próprio tile");
        // Sem o tile da foto (profundidade): sobe para a pasta.
        let capped: Vec<Tile> = tiles_for(&t)
            .into_iter()
            .filter(|x| x.kind != TileKind::Node(foto))
            .collect();
        assert_eq!(visible_tile(&t, &capped, foto), Some(0));
        // Sem o tile de a.bin: cai no "outros" da raiz.
        let a = t.find(Path::new(r"C:\a.bin")).unwrap();
        let no_a: Vec<Tile> = tiles_for(&t)
            .into_iter()
            .filter(|x| x.kind != TileKind::Node(a))
            .collect();
        let others = no_a
            .iter()
            .position(|x| matches!(x.kind, TileKind::Others { .. }));
        assert_eq!(visible_tile(&t, &no_a, a), others);
    }

    #[test]
    fn nav_target_rules() {
        let t = sample();
        let tiles = tiles_for(&t);
        let s = t.find(Path::new(r"C:\São Paulo")).unwrap();
        assert_eq!(
            nav_target(&t, &tiles, 0, t.root()),
            Some(s),
            "pasta entra nela"
        );
        assert_eq!(
            nav_target(&t, &tiles, 1, t.root()),
            Some(s),
            "arquivo entra na pasta dele"
        );
        assert_eq!(
            nav_target(&t, &tiles, 1, s),
            None,
            "já está na pasta do arquivo"
        );
        assert_eq!(nav_target(&t, &tiles, 2, t.root()), None, "arquivo na raiz");
        assert_eq!(
            nav_target(&t, &tiles, 3, t.root()),
            None,
            "outros não navega"
        );
        assert_eq!(nav_target(&t, &tiles, 99, t.root()), None);
    }

    #[test]
    fn zoom_interpolation() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert_eq!(ease_out(-3.0), 0.0);
        assert_eq!(ease_out(7.0), 1.0);
        assert!(ease_out(0.5) > 0.5, "desacelera: passa da metade cedo");
        let mut last = 0.0;
        for i in 1..=20 {
            let v = ease_out(f64::from(i) / 20.0);
            assert!(v >= last);
            last = v;
        }
        let a = rect(10.0, 20.0, 30.0, 40.0);
        let b = rect(0.0, 0.0, 100.0, 80.0);
        assert_eq!(lerp_rect(a, b, 0.0), a);
        assert_eq!(lerp_rect(a, b, 1.0), b);
        assert_eq!(lerp_rect(a, b, 0.5), rect(5.0, 10.0, 65.0, 60.0));
    }

    const KINDS: [FileKind; 11] = ALL_KINDS;

    #[test]
    fn text_is_legible_on_every_tile() {
        for dark in [false, true] {
            for kind in KINDS {
                for depth in 1..=6 {
                    for (what, bg) in [
                        ("arquivo", tile_fill(kind, depth, dark)),
                        ("faixa", folder_band(kind, depth, dark)),
                        ("pasta no limite", capped_fill(kind, depth, dark)),
                    ] {
                        let ratio = contrast(text_color_for(bg), bg);
                        assert!(
                            ratio >= 4.5,
                            "{what} {kind:?} d{depth} dark={dark}: {ratio}"
                        );
                        // O corpo da pasta não tem texto; só precisa se distinguir da faixa.
                        assert!(contrast(body_fill(bg, dark), bg) > 1.2);
                    }
                }
            }
            let o = others_fill(dark);
            assert!(contrast(text_color_for(o), o) >= 4.5);
        }
    }

    fn dist(a: Color32, b: Color32) -> f32 {
        let d = |x: u8, y: u8| (f32::from(x) - f32::from(y)).powi(2);
        (d(a.r(), b.r()) + d(a.g(), b.g()) + d(a.b(), b.b())).sqrt()
    }

    #[test]
    fn kind_colors_are_far_apart() {
        for dark in [false, true] {
            for depth in [1, 3] {
                for (i, &a) in KINDS.iter().enumerate() {
                    for &b in &KINDS[i + 1..] {
                        let d = dist(tile_fill(a, depth, dark), tile_fill(b, depth, dark));
                        assert!(d >= 30.0, "{a:?} x {b:?} d{depth} dark={dark}: {d}");
                    }
                    if a != FileKind::Folder {
                        let o = dist(tile_fill(a, depth, dark), others_fill(dark));
                        assert!(o >= 30.0, "{a:?} x outros dark={dark}: {o}");
                        let f = tile_fill(FileKind::Folder, depth, dark);
                        let c = capped_fill(a, depth, dark);
                        assert!(dist(c, f) >= 25.0, "pasta no limite de {a:?} mostra o tipo");
                    }
                }
            }
        }
    }

    #[test]
    fn all_kinds_index_matches_enum() {
        for (i, k) in ALL_KINDS.iter().enumerate() {
            assert_eq!(*k as usize, i);
            assert!(!kind_label(*k).is_empty());
        }
        assert_eq!(kind_label(FileKind::Archive), "Compactado");
        assert_eq!(kind_label(FileKind::Other), "Outros");
        assert_eq!(kind_label(FileKind::GameData), "Jogos e dados");
        assert_eq!(
            ALL_KINDS.len(),
            FileKind::Other as usize + 1,
            "todos os tipos"
        );
    }

    #[test]
    fn text_color_picks_dark_on_light() {
        assert_eq!(text_color_for(Color32::WHITE), NEAR_BLACK);
        assert_eq!(text_color_for(Color32::BLACK), NEAR_WHITE);
        assert_eq!(text_color_for(Color32::from_rgb(250, 230, 120)), NEAR_BLACK);
        assert_eq!(text_color_for(Color32::from_rgb(30, 40, 90)), NEAR_WHITE);
    }

    #[test]
    fn colors_distinguish_kinds_and_folder_depth() {
        for dark in [false, true] {
            let all: HashSet<Color32> = KINDS.iter().map(|&k| tile_fill(k, 1, dark)).collect();
            assert_eq!(all.len(), KINDS.len(), "uma cor por tipo");
            for d in 1..6 {
                assert_ne!(
                    tile_fill(FileKind::Folder, d, dark),
                    tile_fill(FileKind::Folder, d + 1, dark),
                    "pasta aninhada destaca da de fora"
                );
            }
            assert_ne!(
                tile_fill(FileKind::Video, 1, false),
                tile_fill(FileKind::Video, 1, true),
                "respeita o tema"
            );
        }
    }

    #[test]
    fn footer_formats_pt_br() {
        let t = sample();
        let s = t.find(Path::new(r"C:\São Paulo")).unwrap();
        assert_eq!(
            footer_text(&t, s),
            r"C:\São Paulo · 30 B · 1 arquivo · mod. 23/11/2026"
        );
        assert_eq!(footer_text(&t, t.root()), r"C:\ · 40 B · 2 arquivos");
        let mut t = t;
        t.set_no_access(s);
        assert!(footer_text(&t, s).ends_with("sem acesso a parte do conteúdo"));
    }

    #[test]
    fn others_label_pt_br() {
        assert_eq!(others_label(1), "outros (1 item)");
        assert_eq!(others_label(1234), "outros (1.234 itens)");
    }

    #[test]
    fn layout_is_cached_until_key_changes() {
        let mut m = MapState::new(sample());
        let size = Vec2::new(400.0, 300.0);
        m.ensure_layout(size);
        let ptr = m.cache.as_ref().unwrap().tiles.as_ptr();
        m.ensure_layout(size);
        assert_eq!(
            m.cache.as_ref().unwrap().tiles.as_ptr(),
            ptr,
            "mesma chave: reaproveita"
        );
        let key = m.cache.as_ref().unwrap().key;
        m.opts.max_depth = 5;
        m.ensure_layout(size);
        assert!(m.cache.as_ref().unwrap().key != key, "profundidade refaz");
        let key = m.cache.as_ref().unwrap().key;
        m.after_remove();
        m.ensure_layout(size);
        assert!(m.cache.as_ref().unwrap().key != key, "árvore refaz");
        let key = m.cache.as_ref().unwrap().key;
        let s = m.tree.find(Path::new(r"C:\São Paulo")).unwrap();
        m.go(s);
        m.ensure_layout(size);
        assert!(m.cache.as_ref().unwrap().key != key, "raiz refaz");
        let key = m.cache.as_ref().unwrap().key;
        m.ensure_layout(Vec2::new(401.0, 300.0));
        assert!(m.cache.as_ref().unwrap().key != key, "tamanho refaz");
        assert!(m.back());
        assert_eq!(m.nav.current(), m.tree.root());
    }

    /// Mesma forma de `sample`, sem `finalize` (como a árvore no meio da varredura).
    fn partial() -> Tree {
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let s = t.add(r, "São Paulo", true, 0, None);
        t.add(s, "foto.jpg", false, 30, None);
        t.add(r, "a.bin", false, 10, None);
        t
    }

    #[test]
    fn preview_layout_redone_only_for_new_size() {
        let (t, k) = partial().preview_with_kinds(3);
        let mut p = PreviewMap::new(t, k);
        let size = Vec2::new(400.0, 300.0);
        p.ensure_layout(size);
        let ptr = p.cache.as_ref().unwrap().tiles.as_ptr();
        assert!(!p.cache.as_ref().unwrap().tiles.is_empty());
        p.ensure_layout(size);
        assert_eq!(
            p.cache.as_ref().unwrap().tiles.as_ptr(),
            ptr,
            "mesmo tamanho: reaproveita"
        );
        p.ensure_layout(Vec2::new(401.0, 300.0));
        assert_eq!(p.cache.as_ref().unwrap().key.w, 401.0, "tamanho refaz");
        assert_eq!(p.opts.max_depth, crate::scan::PREVIEW_DEPTH);
    }

    #[test]
    fn preview_colors_and_tooltip_by_kind() {
        let (t, k) = partial().preview_with_kinds(3);
        let p = PreviewMap::new(t, k);
        let t = p.tree();
        let s = t.find(Path::new(r"C:\São Paulo")).unwrap();
        assert_eq!(
            p.kinds[s.0 as usize],
            FileKind::Image,
            "cores do próprio preview"
        );
        let (path, detail) = tooltip_text(t, &p.kinds, TileKind::Node(s));
        assert_eq!(path, r"C:\São Paulo");
        assert!(detail.starts_with("30 B"), "{detail}");
        assert!(detail.contains("mais espaço: Imagem"), "{detail}");
    }

    #[test]
    fn dominant_kind_by_bytes() {
        // C:\ { fotos\ { a.jpg 300, b.mp4 100 }, mix\ { v.mp4 50, sub\ { x.zip 80, y.zip 1 } },
        //       vazia\, r.txt 5 }
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let fotos = t.add(r, "fotos", true, 0, None);
        let a = t.add(fotos, "a.JPG", false, 300, None);
        t.add(fotos, "b.mp4", false, 100, None);
        let mix = t.add(r, "mix", true, 0, None);
        t.add(mix, "v.mp4", false, 50, None);
        let sub = t.add(mix, "sub", true, 0, None);
        t.add(sub, "x.zip", false, 80, None);
        t.add(sub, "y.zip", false, 1, None);
        let vazia = t.add(r, "vazia", true, 0, None);
        let txt = t.add(r, "r.txt", false, 5, None);
        t.finalize();
        let k = dominant_kinds(&t);
        assert_eq!(k.len(), t.len());
        let at = |id: NodeId| k[id.0 as usize];
        assert_eq!(at(a), FileKind::Image);
        assert_eq!(at(txt), FileKind::Document);
        assert_eq!(at(fotos), FileKind::Image);
        assert_eq!(at(sub), FileKind::Archive);
        assert_eq!(
            at(mix),
            FileKind::Archive,
            "81 de zip > 50 de vídeo, somando subpastas"
        );
        assert_eq!(at(vazia), FileKind::Folder);
        // raiz: Image 300 > Video 150 > Archive 81
        assert_eq!(at(t.root()), FileKind::Image);
        // depois de remover, recalcular reflete a árvore viva
        t.remove(fotos);
        let k = dominant_kinds(&t);
        assert_eq!(k[t.root().0 as usize], FileKind::Archive);
    }

    #[test]
    #[ignore = "perf: cargo test --release -- --ignored dominant"]
    fn dominant_kinds_perf_1_5m() {
        let mut t = Tree::new("C:\\".into());
        let exts = ["mp4", "jpg", "rs", "dll", "zip", "txt", "bin", "log"];
        for i in 0..1500 {
            let d = t.add(t.root(), &format!("d{i}"), true, 0, None);
            for j in 0..1000 {
                let name = format!("arquivo_{j}.{}", exts[(i + j) % exts.len()]);
                t.add(d, &name, false, 1000 + j as u64, None);
            }
        }
        t.finalize();
        let start = std::time::Instant::now();
        let k = dominant_kinds(&t);
        eprintln!("{} nós em {:?}", k.len(), start.elapsed());
    }

    #[test]
    fn after_remove_cleans_state() {
        let mut m = MapState::new(sample());
        let s = m.tree.find(Path::new(r"C:\São Paulo")).unwrap();
        let f = m.tree.find(Path::new(r"C:\São Paulo\foto.jpg")).unwrap();
        let a = m.tree.find(Path::new(r"C:\a.bin")).unwrap();
        assert_eq!(m.dominant_kind(m.tree.root()), FileKind::Image);
        m.go(s);
        m.selected = Some(f);
        m.highlight.extend([f, a]);
        m.ensure_layout(Vec2::new(300.0, 200.0));
        let rev = m.cache.as_ref().unwrap().key.rev;
        m.tree_mut().remove(s);
        m.after_remove();
        assert_eq!(
            m.nav.current(),
            m.tree.root(),
            "pasta aberta morreu: volta à raiz"
        );
        assert_eq!(m.selected, None, "seleção morta some");
        assert_eq!(m.highlight, HashSet::from([a]), "só destaques vivos ficam");
        assert_eq!(
            m.dominant_kind(m.tree.root()),
            FileKind::Image,
            "cores não são refeitas na thread da UI: valem as antigas até o recálculo"
        );
        m.ensure_layout(Vec2::new(300.0, 200.0));
        let rev2 = m.cache.as_ref().unwrap().key.rev;
        assert!(rev2 > rev, "layout refeito");
        // O recálculo manda as cores novas: troca e refaz as cores dos tiles.
        let kinds = dominant_kinds(&m.tree);
        m.set_kinds(kinds);
        assert_eq!(m.dominant_kind(m.tree.root()), FileKind::GameData);
        m.ensure_layout(Vec2::new(300.0, 200.0));
        assert!(m.cache.as_ref().unwrap().key.rev > rev2, "cores refeitas");
    }

    #[test]
    fn stale_kinds_are_harmless() {
        // Ids novos (replace_subtree) além do vetor antigo caem no padrão, sem pânico.
        let mut m = MapState::new(sample());
        m.set_kinds(Vec::new());
        assert_eq!(m.dominant_kind(m.tree.root()), FileKind::Folder);
        m.ensure_layout(Vec2::new(300.0, 200.0));
        assert!(!m.cache.as_ref().unwrap().tiles.is_empty());
    }

    #[test]
    fn selection_kept_only_inside_new_root() {
        let mut m = MapState::new(sample());
        let s = m.tree.find(Path::new(r"C:\São Paulo")).unwrap();
        let f = m.tree.find(Path::new(r"C:\São Paulo\foto.jpg")).unwrap();
        let a = m.tree.find(Path::new(r"C:\a.bin")).unwrap();
        m.selected = Some(f);
        m.go(s);
        assert_eq!(m.selected, Some(f), "continua visível dentro de São Paulo");
        m.back();
        assert_eq!(m.selected, Some(f), "a raiz contém tudo");
        m.selected = Some(a);
        m.forward();
        assert_eq!(m.selected, None, "a.bin não está em São Paulo");
        m.selected = Some(s);
        m.back();
        m.go(s);
        assert_eq!(
            m.selected, None,
            "a própria pasta aberta não fica selecionada"
        );
    }

    #[test]
    fn explainable_but_unmarked_nodes_get_no_mark() {
        let mut m = MapState::new(sample());
        let s = m.tree.find(Path::new(r"C:\São Paulo")).unwrap();
        let size = Vec2::new(400.0, 300.0);
        m.set_explainable_marked(HashSet::from([s]), HashSet::new());
        m.ensure_layout(size);
        assert!(
            m.is_explainable(s),
            "continua com \"Por que isso está aqui?\""
        );
        let c = m.cache.as_ref().unwrap();
        assert!(
            c.looks.iter().all(|l| !l.marked && !l.contains_marked),
            "sem marca no mapa"
        );
        let ptr = c.tiles.as_ptr();
        m.set_explainable_marked(HashSet::from([s]), HashSet::from([s]));
        m.ensure_layout(size);
        assert_ne!(
            m.cache.as_ref().unwrap().tiles.as_ptr(),
            ptr,
            "mudar só as marcas refaz o cache"
        );
        assert!(m.cache.as_ref().unwrap().looks.iter().any(|l| l.marked));
    }

    #[test]
    fn suggestion_tiles_are_marked_once_per_layout() {
        let mut m = MapState::new(sample());
        let s = m.tree.find(Path::new(r"C:\São Paulo")).unwrap();
        let size = Vec2::new(400.0, 300.0);
        let marked = |m: &MapState| -> Vec<TileKind> {
            let c = m.cache.as_ref().unwrap();
            c.tiles
                .iter()
                .zip(&c.looks)
                .filter(|(_, l)| l.marked)
                .map(|(t, _)| t.kind)
                .collect()
        };
        m.ensure_layout(size);
        assert!(marked(&m).is_empty(), "sem sugestões, sem marca");
        let ptr = m.cache.as_ref().unwrap().tiles.as_ptr();
        m.set_explainable(HashSet::from([s]));
        m.ensure_layout(size);
        assert_eq!(
            marked(&m),
            vec![TileKind::Node(s)],
            "só o tile do nó da sugestão (não os filhos)"
        );
        assert_ne!(
            m.cache.as_ref().unwrap().tiles.as_ptr(),
            ptr,
            "trocar as sugestões refaz o cache"
        );
        let ptr = m.cache.as_ref().unwrap().tiles.as_ptr();
        m.set_explainable(HashSet::from([s]));
        m.ensure_layout(size);
        assert_eq!(
            m.cache.as_ref().unwrap().tiles.as_ptr(),
            ptr,
            "mesmas sugestões: reaproveita"
        );
    }

    /// C:\ { a\b\c\d\e\deep.bin 100, a\other.bin 50, z.bin 10 } e `build_layout` com a
    /// profundidade dada; devolve (árvore, layout).
    fn deep_layout(depth: u8, marks: &[&str]) -> (Tree, LayoutCache) {
        let mut t = Tree::new("C:\\".into());
        let mut cur = t.root();
        let mut a = cur;
        for n in ["a", "b", "c", "d", "e"] {
            cur = t.add(cur, n, true, 0, None);
            if n == "a" {
                a = cur;
            }
        }
        t.add(cur, "deep.bin", false, 100, None);
        t.add(a, "other.bin", false, 50, None);
        t.add(t.root(), "z.bin", false, 10, None);
        t.finalize();
        let set: HashSet<NodeId> = marks
            .iter()
            .map(|p| t.find(Path::new(p)).unwrap())
            .collect();
        let key = CacheKey {
            root: t.root(),
            w: 800.0,
            h: 600.0,
            depth,
            rev: 0,
            explain_rev: 0,
        };
        let opts = LayoutOpts {
            max_depth: depth,
            ..LayoutOpts::default()
        };
        let c = build_layout(&t, &[], key, &opts, &set);
        (t, c)
    }

    fn flagged(c: &LayoutCache, f: impl Fn(&TileLook) -> bool) -> Vec<TileKind> {
        c.tiles
            .iter()
            .zip(&c.looks)
            .filter(|(_, l)| f(l))
            .map(|(t, _)| t.kind)
            .collect()
    }

    const DEEP: &str = r"C:\a\b\c\d\e\deep.bin";
    const OTHER: &str = r"C:\a\other.bin";

    #[test]
    fn ancestor_of_hidden_suggestion_is_marked() {
        let (t, c) = deep_layout(3, &[DEEP]);
        assert!(flagged(&c, |l| l.marked).is_empty(), "não desenhada");
        let marked = flagged(&c, |l| l.contains_marked);
        assert_eq!(marked.len(), 1, "só um bloco: {marked:?}");
        let TileKind::Node(id) = marked[0] else {
            panic!("esperava Node")
        };
        let deepest = c
            .tiles
            .iter()
            .filter(|x| matches!(x.kind, TileKind::Node(_)))
            .max_by_key(|x| x.depth)
            .unwrap();
        assert_eq!(
            deepest.kind,
            TileKind::Node(id),
            "o bloco desenhado mais fundo"
        );
        let deep = t.find(Path::new(DEEP)).unwrap();
        assert!(t.ancestors(deep).contains(&id));
    }

    #[test]
    fn drawn_suggestion_not_double_marked() {
        let (t, c) = deep_layout(3, &[OTHER]);
        let o = t.find(Path::new(OTHER)).unwrap();
        assert_eq!(flagged(&c, |l| l.marked), vec![TileKind::Node(o)]);
        assert!(
            flagged(&c, |l| l.contains_marked).is_empty(),
            "ancestrais não herdam a marca de uma sugestão desenhada"
        );
        // desenhada + escondida: contains_marked só pela escondida
        let (t, c) = deep_layout(3, &[OTHER, DEEP]);
        let o = t.find(Path::new(OTHER)).unwrap();
        assert_eq!(flagged(&c, |l| l.marked), vec![TileKind::Node(o)]);
        let cm = flagged(&c, |l| l.contains_marked);
        assert_eq!(cm.len(), 1);
        assert_ne!(cm[0], TileKind::Node(o));
        let a = t.find(Path::new(r"C:\a")).unwrap();
        assert_ne!(
            cm[0],
            TileKind::Node(a),
            "a pasta rasa não é a mais próxima"
        );
    }

    #[test]
    fn others_tile_containing_suggestion_marked() {
        // Muito pequeno para desenhar: `tiny` vira "Others" de C:\.
        let mut t = Tree::new("C:\\".into());
        t.add(t.root(), "big.bin", false, 1_000_000, None);
        let tiny = t.add(t.root(), "tiny.bin", false, 1, None);
        t.finalize();
        let key = CacheKey {
            root: t.root(),
            w: 800.0,
            h: 600.0,
            depth: 3,
            rev: 0,
            explain_rev: 0,
        };
        let c = build_layout(&t, &[], key, &LayoutOpts::default(), &HashSet::from([tiny]));
        let m = flagged(&c, |l| l.contains_marked);
        assert!(
            matches!(m.as_slice(), [TileKind::Others { parent, .. }] if *parent == t.root()),
            "{m:?}"
        );
        assert!(flagged(&c, |l| l.marked).is_empty());
    }

    #[test]
    fn explainable_includes_descendants() {
        let mut m = MapState::new(sample());
        let s = m.tree.find(Path::new(r"C:\São Paulo")).unwrap();
        let f = m.tree.find(Path::new(r"C:\São Paulo\foto.jpg")).unwrap();
        let a = m.tree.find(Path::new(r"C:\a.bin")).unwrap();
        m.set_explainable(HashSet::from([s]));
        assert!(m.is_explainable(s));
        assert!(m.is_explainable(f), "dentro de um nó de sugestão");
        assert!(!m.is_explainable(a));
        assert!(!m.is_explainable(m.tree.root()), "ancestral não conta");
    }

    #[test]
    fn highlight_falls_back_to_visible_ancestor() {
        let t = sample();
        let s = t.find(Path::new(r"C:\São Paulo")).unwrap();
        let a = t.find(Path::new(r"C:\a.bin")).unwrap();
        // Só os tiles de primeiro nível: foto.jpg não foi desenhado.
        let tiles: Vec<Tile> = tiles_for(&t).into_iter().filter(|t| t.depth == 1).collect();
        let f = t.find(Path::new(r"C:\São Paulo\foto.jpg")).unwrap();
        let got = highlight_tiles(&t, &tiles, &HashSet::from([f]), t.root());
        assert_eq!(got, vec![0], "foto.jpg aparece no tile de São Paulo");
        let got = highlight_tiles(&t, &tiles, &HashSet::from([f, s, a]), t.root());
        assert_eq!(got, vec![0, 1], "sem repetição, na ordem dos tiles");
        let got = highlight_tiles(&t, &tiles, &HashSet::from([t.root()]), t.root());
        assert!(got.is_empty(), "a pasta aberta não pinta a área toda");
        let got = highlight_tiles(&t, &tiles, &HashSet::from([f]), s);
        assert!(got.is_empty(), "fora dos tiles desenhados");
    }

    #[test]
    fn tree_mut_clones_only_when_shared() {
        let mut m = MapState::new(sample());
        let shared = Arc::clone(&m.tree);
        let a = m.tree.find(Path::new(r"C:\a.bin")).unwrap();
        m.tree_mut().remove(a);
        assert!(shared.node(a).alive, "quem tinha a cópia não vê a remoção");
        assert!(!m.tree.node(a).alive);
    }

    #[test]
    fn set_search_index_replaces_index() {
        let mut m = MapState::new(sample());
        let mut t = sample();
        let r = t.root();
        t.add(r, "novo.txt", false, 1, None);
        m.set_search_index(SearchIndex::build(&t));
        assert_eq!(m.search_index().matches("novo").len(), 1);
    }
}
