//! Layout squarified (Bruls, Huizing e van Wijk) e aninhado do treemap.
//! Módulo puro (sem egui): a UI converte `Rect` para o tipo do egui.

use crate::tree::{NodeId, Tree};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileKind {
    Node(NodeId),
    Others {
        parent: NodeId,
        count: u32,
        size: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tile {
    pub kind: TileKind,
    pub rect: Rect,
    pub depth: u8,
    pub has_header: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct LayoutOpts {
    pub max_depth: u8,
    pub min_side_px: f32,
    pub header_px: f32,
    pub padding_px: f32,
}

impl Default for LayoutOpts {
    fn default() -> Self {
        LayoutOpts {
            max_depth: 3,
            min_side_px: 6.0,
            header_px: 18.0,
            padding_px: 2.0,
        }
    }
}

/// Squarified treemap. `sizes` em ordem decrescente; a saída segue a mesma ordem.
pub fn squarify(sizes: &[u64], area: Rect) -> Vec<Rect> {
    let mut out = Vec::with_capacity(sizes.len());
    squarify_into(sizes, area, &mut out);
    out
}

/// Pior razão de aspecto de uma fileira de área total `s` (min `lo`, max `hi`) no lado `l`.
fn worst(s: f64, hi: f64, lo: f64, l: f64) -> f64 {
    let l2 = l * l;
    (l2 * hi / (s * s)).max(s * s / (l2 * lo))
}

fn squarify_into(sizes: &[u64], area: Rect, out: &mut Vec<Rect>) {
    let n = sizes.len();
    let base = out.len();
    let total: f64 = sizes.iter().map(|&s| s as f64).sum();
    let (mut x, mut y) = (f64::from(area.x), f64::from(area.y));
    let (mut w, mut h) = (f64::from(area.w), f64::from(area.h));
    let empty = Rect {
        x: if area.x.is_finite() { area.x } else { 0.0 },
        y: if area.y.is_finite() { area.y } else { 0.0 },
        w: 0.0,
        h: 0.0,
    };
    if !(total > 0.0 && w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()) {
        out.extend(std::iter::repeat_n(empty, n));
        return;
    }
    let scale = w * h / total;
    let mut i = 0;
    while i < n && sizes[i] > 0 {
        let vertical = w >= h; // fileira vira uma coluna à esquerda
        let side = if vertical { h } else { w };
        // Estende a fileira enquanto a pior razão de aspecto não piora.
        let first = sizes[i] as f64 * scale;
        let (mut sum, mut hi, mut lo) = (first, first, first);
        let mut cur = worst(sum, hi, lo, side);
        let mut j = i + 1;
        while j < n && sizes[j] > 0 {
            let a = sizes[j] as f64 * scale;
            let (ns, nh, nl) = (sum + a, hi.max(a), lo.min(a));
            let nw = worst(ns, nh, nl, side);
            if nw > cur {
                break;
            }
            (sum, hi, lo, cur) = (ns, nh, nl, nw);
            j += 1;
        }
        let last_row = j >= n || sizes[j] == 0;
        let thickness = if last_row {
            if vertical { w } else { h }
        } else {
            sum / side
        };
        let mut pos = if vertical { y } else { x };
        let end = pos + side;
        for (k, &sz) in sizes.iter().enumerate().take(j).skip(i) {
            let len = if k + 1 == j {
                end - pos
            } else {
                sz as f64 * scale / thickness
            };
            let r = if vertical {
                Rect {
                    x: x as f32,
                    y: pos as f32,
                    w: thickness as f32,
                    h: len as f32,
                }
            } else {
                Rect {
                    x: pos as f32,
                    y: y as f32,
                    w: len as f32,
                    h: thickness as f32,
                }
            };
            out.push(r);
            pos += len;
        }
        if vertical {
            x += thickness;
            w -= thickness;
        } else {
            y += thickness;
            h -= thickness;
        }
        i = j;
        if w <= 0.0 || h <= 0.0 {
            break;
        }
    }
    // Itens de tamanho zero (cauda) ou sem espaço restante viram retângulos vazios.
    while out.len() < base + n {
        out.push(empty);
    }
}

/// Lado menor do retângulo.
fn min_side(r: &Rect) -> f32 {
    r.w.min(r.h)
}

struct Ctx<'a> {
    tree: &'a Tree,
    opts: &'a LayoutOpts,
    tiles: Vec<Tile>,
}

impl Ctx<'_> {
    /// Posiciona os filhos de `parent` dentro de `area` (profundidade `depth`).
    fn place_children(&mut self, parent: NodeId, area: Rect, depth: u8) {
        let kids: Vec<NodeId> = self
            .tree
            .children(parent)
            .iter()
            .copied()
            .take_while(|&k| self.tree.node(k).size > 0)
            .collect();
        if kids.is_empty() {
            return;
        }
        let mut sizes: Vec<u64> = kids.iter().map(|&k| self.tree.node(k).size).collect();
        let mut rects = squarify(&sizes, area);

        // Filhos cujo retângulo é pequeno demais viram um único tile `Others` no fim.
        let cut = rects
            .iter()
            .position(|r| min_side(r) < self.opts.min_side_px)
            .unwrap_or(kids.len());
        let mut others = None;
        if cut < kids.len() {
            let size: u64 = sizes[cut..].iter().sum();
            let count = (kids.len() - cut) as u32;
            sizes.truncate(cut);
            sizes.push(size);
            rects = squarify(&sizes, area);
            others = Some((count, size));
        }

        let shown = if others.is_some() { cut } else { kids.len() };
        for (&kid, &rect) in kids[..shown].iter().zip(&rects) {
            let node = self.tree.node(kid);
            let header = node.is_dir
                && depth < self.opts.max_depth
                && rect.h >= self.opts.header_px + self.opts.min_side_px
                && rect.w >= self.opts.min_side_px;
            self.tiles.push(Tile {
                kind: TileKind::Node(kid),
                rect,
                depth,
                has_header: header,
            });
            if header {
                let pad = self.opts.padding_px;
                let inner = Rect {
                    x: rect.x + pad,
                    y: rect.y + self.opts.header_px + pad,
                    w: rect.w - 2.0 * pad,
                    h: rect.h - self.opts.header_px - 2.0 * pad,
                };
                if inner.w >= self.opts.min_side_px && inner.h >= self.opts.min_side_px {
                    self.place_children(kid, inner, depth + 1);
                }
            }
        }
        if let Some((count, size)) = others {
            self.tiles.push(Tile {
                kind: TileKind::Others {
                    parent,
                    count,
                    size,
                },
                rect: rects[shown],
                depth,
                has_header: false,
            });
        }
    }
}

/// Layout aninhado dos descendentes de `root` dentro de `area`. A própria raiz não vira tile;
/// seus filhos têm `depth` 1. Pais vêm antes dos filhos.
pub fn layout(tree: &Tree, root: NodeId, area: Rect, opts: &LayoutOpts) -> Vec<Tile> {
    let mut ctx = Ctx {
        tree,
        opts,
        tiles: Vec::new(),
    };
    let finite = [
        area.x,
        area.y,
        area.w,
        area.h,
        opts.min_side_px,
        opts.header_px,
        opts.padding_px,
    ]
    .iter()
    .all(|v| v.is_finite());
    if finite && area.w >= opts.min_side_px && area.h >= opts.min_side_px && opts.max_depth >= 1 {
        ctx.place_children(root, area, 1);
    }
    ctx.tiles
}
#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 100.0,
        h: 60.0,
    };

    #[test]
    fn squarify_preserves_area() {
        let r = squarify(&[6, 6, 4, 3, 2, 2, 1], AREA);
        assert_eq!(r.len(), 7);
        let sum: f32 = r.iter().map(|r| r.w * r.h).sum();
        assert!((sum - 6000.0).abs() < 0.5, "sum = {sum}");
        for r in &r {
            assert!(r.x >= -0.001 && r.y >= -0.001, "{r:?}");
            assert!(r.x + r.w <= 100.001 && r.y + r.h <= 60.001, "{r:?}");
        }
    }

    #[test]
    fn squarify_aspect_ok() {
        for r in squarify(&[6, 6, 4, 3, 2, 2, 1], AREA) {
            let ratio = (r.w / r.h).max(r.h / r.w);
            assert!(ratio <= 3.0, "{r:?} ratio {ratio}");
        }
    }

    #[test]
    fn squarify_proportional_and_edge_cases() {
        let r = squarify(
            &[3, 1],
            Rect {
                x: 10.0,
                y: 20.0,
                w: 40.0,
                h: 10.0,
            },
        );
        assert!((r[0].w * r[0].h - 300.0).abs() < 0.01);
        assert!((r[1].w * r[1].h - 100.0).abs() < 0.01);
        assert!(squarify(&[], AREA).is_empty());
        let z = squarify(&[5, 0, 0], AREA);
        assert_eq!(z.len(), 3);
        assert!((z[0].w * z[0].h - 6000.0).abs() < 0.5);
        assert_eq!(z[1].w * z[1].h, 0.0);
        let all0 = squarify(&[0, 0], AREA);
        assert_eq!(all0.len(), 2);
    }

    #[test]
    fn squarify_degenerate_areas() {
        let bad = [
            (f32::NAN, 10.0),
            (10.0, f32::NAN),
            (f32::INFINITY, 10.0),
            (10.0, f32::INFINITY),
            (-5.0, 10.0),
            (10.0, -5.0),
            (0.0, 10.0),
            (10.0, 0.0),
        ];
        for (w, h) in bad {
            let r = squarify(
                &[3, 2, 1],
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w,
                    h,
                },
            );
            assert!(
                r.iter()
                    .all(|r| r.w == 0.0 && r.h == 0.0 && !r.x.is_nan() && !r.y.is_nan())
            );
        }
        let r = squarify(
            &[3],
            Rect {
                x: f32::NAN,
                y: 0.0,
                w: 10.0,
                h: 10.0,
            },
        );
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn squarify_single_fills_area() {
        let a = Rect {
            x: 5.0,
            y: 7.0,
            w: 30.0,
            h: 20.0,
        };
        let r = squarify(&[42], a);
        assert_eq!(r.len(), 1);
        assert!((r[0].x - 5.0).abs() < 1e-4 && (r[0].y - 7.0).abs() < 1e-4);
        assert!((r[0].w - 30.0).abs() < 1e-4 && (r[0].h - 20.0).abs() < 1e-4);
    }

    #[test]
    fn layout_degenerate_inputs_are_empty() {
        let t = chain_tree();
        let o = LayoutOpts::default();
        let tiny = Rect {
            x: 0.0,
            y: 0.0,
            w: 3.0,
            h: 500.0,
        };
        assert!(layout(&t, t.root(), tiny, &o).is_empty());
        let nan = Rect {
            x: 0.0,
            y: 0.0,
            w: f32::NAN,
            h: 100.0,
        };
        assert!(layout(&t, t.root(), nan, &o).is_empty());
        let ok = Rect {
            x: 0.0,
            y: 0.0,
            w: 500.0,
            h: 500.0,
        };
        let bad = LayoutOpts {
            header_px: f32::NAN,
            ..o
        };
        assert!(layout(&t, t.root(), ok, &bad).is_empty());
        let bad = LayoutOpts {
            padding_px: f32::INFINITY,
            ..o
        };
        assert!(layout(&t, t.root(), ok, &bad).is_empty());
    }

    #[test]
    fn layout_children_inside_parent() {
        let t = chain_tree();
        let area = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 800.0,
        };
        let opts = LayoutOpts {
            max_depth: 9,
            ..LayoutOpts::default()
        };
        let tiles = layout(&t, t.root(), area, &opts);
        let mut last: Vec<Rect> = Vec::new(); // último tile visto por profundidade
        let mut checked = 0;
        for tile in &tiles {
            let d = tile.depth as usize;
            last.truncate(d - 1);
            let parent = if d == 1 { area } else { *last.last().unwrap() };
            let (r, e) = (tile.rect, 0.01);
            assert!(
                r.x >= parent.x - e && r.y >= parent.y - e,
                "{r:?} in {parent:?}"
            );
            assert!(r.x + r.w <= parent.x + parent.w + e, "{r:?} in {parent:?}");
            assert!(r.y + r.h <= parent.y + parent.h + e, "{r:?} in {parent:?}");
            if d > 1 {
                checked += 1;
            }
            last.push(r);
        }
        assert!(checked > 0);
    }

    fn chain_tree() -> Tree {
        // r { d1 { d2 { d3 { d4 { d5 { f } g } g } g } g } g }
        let mut t = Tree::new("C:\\".into());
        let mut cur = t.root();
        for i in 1..=5 {
            let d = t.add(cur, &format!("d{i}"), true, 0, None);
            t.add(cur, &format!("g{i}"), false, 1000, None);
            cur = d;
        }
        t.add(cur, "f", false, 5000, None);
        t.finalize();
        t
    }

    #[test]
    fn layout_respects_depth() {
        let t = chain_tree();
        let area = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 1000.0,
        };
        let opts = LayoutOpts {
            max_depth: 2,
            ..LayoutOpts::default()
        };
        let tiles = layout(&t, t.root(), area, &opts);
        assert!(tiles.iter().all(|t| t.depth <= 2));
        assert!(tiles.iter().any(|t| t.depth == 2));
        assert!(tiles.iter().any(|t| t.depth == 1));
        // pais antes dos filhos: o primeiro tile é de profundidade 1
        assert_eq!(tiles[0].depth, 1);
        let deep = layout(
            &t,
            t.root(),
            area,
            &LayoutOpts {
                max_depth: 9,
                ..opts
            },
        );
        assert!(deep.iter().any(|t| t.depth == 6));
    }

    #[test]
    fn layout_groups_tiny() {
        let mut t = Tree::new("C:\\".into());
        let d = t.add(t.root(), "d", true, 0, None);
        t.add(d, "big.bin", false, 1 << 30, None);
        for i in 0..1000 {
            t.add(d, &format!("s{i}"), false, 1024, None);
        }
        t.finalize();
        let area = Rect {
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        let tiles = layout(&t, t.root(), area, &LayoutOpts::default());
        let others: Vec<_> = tiles
            .iter()
            .filter(|t| matches!(t.kind, TileKind::Others { .. }))
            .collect();
        assert_eq!(others.len(), 1);
        match others[0].kind {
            TileKind::Others {
                parent,
                count,
                size,
            } => {
                assert_eq!(parent, d);
                assert_eq!(count, 1000);
                assert_eq!(size, 1000 * 1024);
            }
            _ => unreachable!(),
        }
        // Others é o último tile dentro do pai
        assert!(matches!(
            tiles.last().unwrap().kind,
            TileKind::Others { .. }
        ));
        assert!(
            tiles
                .iter()
                .any(|t| t.kind == TileKind::Node(d) && t.has_header)
        );
    }

    #[test]
    fn layout_skips_zero_sizes_and_small_dirs_get_no_header() {
        let mut t = Tree::new("C:\\".into());
        let d = t.add(t.root(), "empty", true, 0, None);
        t.add(t.root(), "z", false, 0, None);
        t.add(t.root(), "f", false, 100, None);
        let _ = d;
        t.finalize();
        let area = Rect {
            x: 0.0,
            y: 0.0,
            w: 200.0,
            h: 20.0,
        };
        let tiles = layout(&t, t.root(), area, &LayoutOpts::default());
        assert_eq!(tiles.len(), 1);
        assert!(!tiles[0].has_header);

        let mut t = Tree::new("C:\\".into());
        let d = t.add(t.root(), "d", true, 0, None);
        t.add(d, "a", false, 10, None);
        t.finalize();
        let tiles = layout(
            &t,
            t.root(),
            Rect {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 20.0,
            },
            &LayoutOpts::default(),
        );
        assert_eq!(
            tiles.len(),
            1,
            "h=20 < header+min_side: desenhada como folha"
        );
        assert!(!tiles[0].has_header);
    }

    #[test]
    #[ignore = "perf: cargo test --release -- --ignored"]
    fn layout_perf_100k() {
        let mut t = Tree::new("C:\\".into());
        for i in 0..100 {
            let d = t.add(t.root(), &format!("d{i}"), true, 0, None);
            for j in 0..100 {
                let e = t.add(d, &format!("e{j}"), true, 0, None);
                for k in 0..10 {
                    t.add(e, &format!("f{k}"), false, 1000 + (i * j * k) as u64, None);
                }
            }
        }
        t.finalize();
        let area = Rect {
            x: 0.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        };
        let opts = LayoutOpts {
            max_depth: 3,
            min_side_px: 0.5,
            ..LayoutOpts::default()
        };
        let start = std::time::Instant::now();
        let tiles = layout(&t, t.root(), area, &opts);
        eprintln!("{} tiles in {:?}", tiles.len(), start.elapsed());
    }
}
