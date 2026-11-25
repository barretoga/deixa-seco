//! Sugestões e marcações do painel, sem desenho: junta a análise com os programas, poda depois
//! de uma limpeza e transforma o que está marcado em operações de `cleanup`.

use crate::cleanup::Op;
use crate::format::format_bytes;
use crate::insights::programs::programs_insight;
use crate::insights::{Action, Insight, Program, ProgramSource, Risk};
use crate::tree::{NodeId, Tree};
use std::collections::HashSet;
use std::path::Path;

/// Algo marcado no painel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pick {
    /// Categoria inteira (apagar o conteúdo, esvaziar a Lixeira, limpar o Docker).
    Whole(&'static str),
    /// Um item de uma categoria: vai para a Lixeira, ou um disco a compactar.
    Item(&'static str, NodeId),
}

/// Sugestões da análise mais os programas instalados (que chegam depois, de outra thread).
#[derive(Default)]
pub struct Suggestions {
    base: Vec<Insight>,
    /// `None` enquanto a lista de programas é lida do registro.
    programs: Option<Vec<Program>>,
    /// `base` + programas, na ordem de exibição.
    list: Vec<Insight>,
    /// Programas já tirados (nome, origem): uma lista que chega depois (a dos apps da Store já
    /// medidos) não os traz de volta.
    gone: Vec<(String, ProgramSource)>,
}

impl Suggestions {
    pub fn new(base: Vec<Insight>) -> Suggestions {
        let mut s = Suggestions {
            base,
            ..Default::default()
        };
        s.list = s.base.clone();
        s
    }

    pub fn list(&self) -> &[Insight] {
        &self.list
    }

    pub fn loading_programs(&self) -> bool {
        self.programs.is_none()
    }

    /// Novas sugestões da análise (recalculadas numa thread).
    pub fn set_base(&mut self, base: Vec<Insight>, tree: &Tree) {
        self.base = base;
        self.merge(tree);
    }

    /// `programs_insight` roda aqui, na thread da UI: é uma busca por programa (`find`), não
    /// uma passada na árvore toda.
    pub fn set_programs(&mut self, mut programs: Vec<Program>, tree: &Tree) {
        programs.retain(|p| !self.is_gone(p));
        self.programs = Some(programs);
        self.merge(tree);
    }

    fn is_gone(&self, p: &Program) -> bool {
        self.gone
            .iter()
            .any(|(name, source)| *name == p.name && *source == p.source)
    }

    /// Tira um programa desinstalado da lista (pelo nome e pela origem: dois pacotes da Store
    /// com o mesmo nome são programas diferentes).
    pub fn remove_program(&mut self, gone: &Program, tree: &Tree) {
        self.gone.push((gone.name.clone(), gone.source.clone()));
        if let Some(p) = &mut self.programs {
            p.retain(|p| !(p.name == gone.name && p.source == gone.source));
        }
        self.merge(tree);
    }

    /// Depois de `Tree::remove`: tira nós mortos, refaz os tamanhos pela árvore e some com
    /// sugestões que esvaziaram. Vale até a análise recalculada chegar.
    pub fn prune(&mut self, tree: &Tree) {
        self.base.retain_mut(|ins| {
            // Sem nós (Lixeira, Docker): não depende da árvore; fica até o recálculo, mesmo a
            // Informativa de 0 B do Docker parado (senão some e volta).
            if ins.nodes.is_empty() {
                return true;
            }
            ins.nodes.retain(|&n| tree.node(n).alive);
            ins.size = ins.nodes.iter().map(|&n| tree.node(n).size).sum();
            ins.size > 0
        });
        self.merge(tree);
    }

    fn merge(&mut self, tree: &Tree) {
        let mut list = self.base.clone();
        if let Some(p) = self
            .programs
            .as_deref()
            .and_then(|p| programs_insight(p, tree))
        {
            list.push(p);
        }
        list.sort_by(|a, b| {
            a.risk
                .cmp(&b.risk)
                .then(b.size.cmp(&a.size))
                .then(a.id.cmp(b.id))
        });
        self.list = list;
    }

    /// Nós de todas as sugestões (`MapState::explainable`).
    pub fn explainable(&self) -> HashSet<NodeId> {
        explainable_set(&self.list)
    }

    /// Nós marcados no mapa (`marked_set`).
    pub fn marked(&self) -> HashSet<NodeId> {
        marked_set(&self.list)
    }
}

pub fn explainable_set(list: &[Insight]) -> HashSet<NodeId> {
    list.iter().flat_map(|i| i.nodes.iter().copied()).collect()
}

/// Nós que ganham marca no mapa ("em sugestão"/"contém sugestão"): os de todas as sugestões
/// menos "Programas instalados", que marcaria pastas de jogos e programas por todo o mapa.
/// Esses continuam explicáveis ("Por que isso está aqui?").
pub fn marked_set(list: &[Insight]) -> HashSet<NodeId> {
    list.iter()
        .filter(|i| i.id != "programs")
        .flat_map(|i| i.nodes.iter().copied())
        .collect()
}

/// Soma do que dá para liberar por aqui (sem as Informativas).
/// Programas ficam de fora: são um inventário do computador (nem sempre dentro da pasta
/// analisada), não algo a apagar inteiro. Discos a compactar também: o tamanho deles é o do
/// arquivo inteiro, e compactar só devolve o espaço vazio de dentro (quanto, só no fim).
pub fn headline_total(list: &[Insight]) -> u64 {
    list.iter()
        .filter(|i| {
            i.risk != Risk::Informativo && i.programs.is_empty() && i.action != Action::CompactVhdx
        })
        .map(|i| i.size)
        .sum()
}

/// "tamanho atual X" de um disco a compactar.
pub fn disk_size_text(size: u64) -> String {
    format!("tamanho atual {}", format_bytes(size))
}

/// Tamanho na linha da categoria: o de um disco a compactar não é o que sai.
pub fn size_text(ins: &Insight) -> String {
    match ins.action {
        Action::CompactVhdx => disk_size_text(ins.size),
        _ => format_bytes(ins.size),
    }
}

fn is_compact(op: &Op) -> bool {
    matches!(op, Op::CompactVhdx { .. })
}

/// Quanto as operações liberam com certeza (compactar fica de fora: só se sabe no fim).
pub fn freeable(ops: &[(Op, u64)]) -> u64 {
    ops.iter()
        .filter(|(op, _)| !is_compact(op))
        .map(|(_, s)| s)
        .sum()
}

/// Texto do botão "Limpar selecionados" com o que está marcado.
pub fn clean_label(ops: &[(Op, u64)]) -> String {
    let compact = ops.iter().any(|(op, _)| is_compact(op));
    let others = ops.iter().any(|(op, _)| !is_compact(op));
    match (others, compact) {
        (false, false) => "Limpar selecionados".to_string(),
        (true, false) => format!("Limpar selecionados ({})", format_bytes(freeable(ops))),
        (true, true) => format!(
            "Limpar selecionados ({} + compactar)",
            format_bytes(freeable(ops))
        ),
        (false, true) => "Limpar selecionados (compactar)".to_string(),
    }
}

/// A sugestão que contém o nó (ele ou um ancestral está nos nós dela). Com várias, a do nó
/// mais fundo (a mais específica); "Programas instalados" só se nenhuma outra contém o nó.
pub fn insight_for(list: &[Insight], tree: &Tree, id: NodeId) -> Option<&'static str> {
    let chain = tree.ancestors(id); // da raiz até o nó
    list.iter()
        .filter_map(|i| {
            let depth = chain.iter().rposition(|c| i.nodes.contains(c))?;
            Some(((i.id != "programs", depth), i.id))
        })
        // `max_by_key` fica com o último dos iguais; `rev` mantém o primeiro da lista.
        .rev()
        .max_by_key(|(key, _)| *key)
        .map(|(_, id)| id)
}

/// O item pode ser marcado (categoria com ação e, se precisa de admin, o app é admin).
pub(super) fn can_pick(ins: &Insight, elevated: bool) -> bool {
    !(ins.needs_admin && !elevated)
}

/// Categoria com checkbox: Seguro com ação, Revisar que vai para a Lixeira, discos a
/// compactar e o lixo do Docker.
pub(super) fn checkable(ins: &Insight) -> bool {
    match ins.action {
        Action::DeleteContents
        | Action::EmptyRecycleBin
        | Action::Recycle
        | Action::DockerPrune => true,
        Action::CompactVhdx => !ins.disks.is_empty(),
        Action::Uninstall { .. } | Action::None => false,
    }
}

/// Itens marcáveis um a um (`Pick::Item`): os nós de uma categoria Lixeira (sem os
/// acompanhantes, que saem junto com o item deles) ou os discos a compactar. Vazio nas
/// categorias marcadas inteiras.
pub(super) fn items(ins: &Insight) -> Vec<NodeId> {
    match ins.action {
        Action::Recycle => ins
            .nodes
            .iter()
            .copied()
            .filter(|n| !ins.companions.iter().any(|(_, c)| c == n))
            .collect(),
        Action::CompactVhdx => ins.disks.iter().map(|d| d.node).collect(),
        _ => Vec::new(),
    }
}

/// Acompanhantes do item (saem junto com ele).
fn companions_of(ins: &Insight, item: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    ins.companions
        .iter()
        .filter(move |(a, _)| *a == item)
        .map(|(_, c)| *c)
}

/// Tamanho do item na lista, com os acompanhantes.
pub(super) fn item_size(ins: &Insight, tree: &Tree, item: NodeId) -> u64 {
    tree.node(item).size
        + companions_of(ins, item)
            .filter(|&c| tree.node(c).alive)
            .map(|c| tree.node(c).size)
            .sum::<u64>()
}

/// Aviso quando a seleção tem o prune e a compactação juntos.
pub const DOCKER_CONFLICT_NOTE: &str = "Limpar o Docker precisa dele aberto e compactar precisa \
     dele fechado: vou limpar o Docker agora; feche o Docker Desktop e compacte depois.";

/// O prune do Docker precisa do Docker Desktop aberto e a compactação recusa rodar com ele
/// aberto: os dois nunca dão certo na mesma limpeza. Com os dois marcados, fica só o prune e
/// a compactação sai do plano (`true`: avisar com `DOCKER_CONFLICT_NOTE`).
pub fn resolve_docker_conflict(ops: Vec<(Op, u64)>) -> (Vec<(Op, u64)>, bool) {
    let prune = ops.iter().any(|(op, _)| *op == Op::DockerPrune);
    let compact = ops.iter().any(|(op, _)| is_compact(op));
    if !(prune && compact) {
        return (ops, false);
    }
    (
        ops.into_iter().filter(|(op, _)| !is_compact(op)).collect(),
        true,
    )
}

/// Operações do que está marcado, com o tamanho de cada uma (entrada de `cleanup::plan`).
pub fn selected_ops(
    list: &[Insight],
    picks: &HashSet<Pick>,
    tree: &Tree,
    elevated: bool,
) -> Vec<(Op, u64)> {
    let mut out = Vec::new();
    for ins in list.iter().filter(|i| can_pick(i, elevated)) {
        let alive = ins.nodes.iter().copied().filter(|&n| tree.node(n).alive);
        match ins.action {
            Action::DeleteContents if picks.contains(&Pick::Whole(ins.id)) => {
                out.extend(alive.map(|n| (Op::DeleteContents(tree.path(n)), tree.node(n).size)));
            }
            Action::EmptyRecycleBin if picks.contains(&Pick::Whole(ins.id)) => {
                out.push((Op::EmptyRecycleBin, ins.size));
            }
            Action::Recycle => {
                let picked = items(ins)
                    .into_iter()
                    .filter(|&n| tree.node(n).alive && picks.contains(&Pick::Item(ins.id, n)));
                for n in picked {
                    out.push((Op::Recycle(tree.path(n)), tree.node(n).size));
                    out.extend(
                        companions_of(ins, n)
                            .filter(|&c| tree.node(c).alive)
                            .map(|c| (Op::Recycle(tree.path(c)), tree.node(c).size)),
                    );
                }
            }
            Action::CompactVhdx => out.extend(
                ins.disks
                    .iter()
                    .filter(|d| {
                        tree.node(d.node).alive && picks.contains(&Pick::Item(ins.id, d.node))
                    })
                    .map(|d| {
                        (
                            Op::CompactVhdx {
                                path: d.path.clone(),
                                distro: d.distro.clone(),
                            },
                            tree.node(d.node).size,
                        )
                    }),
            ),
            Action::DockerPrune if picks.contains(&Pick::Whole(ins.id)) => {
                out.push((Op::DockerPrune, ins.size));
            }
            _ => {}
        }
    }
    order_and_dedup(out)
}

fn op_path(op: &Op) -> Option<&Path> {
    match op {
        Op::DeletePermanent(p) | Op::DeleteContents(p) | Op::Recycle(p) => Some(p),
        // `CompactVhdx` não remove o arquivo: fica fora da deduplicação por caminho.
        Op::EmptyRecycleBin
        | Op::Uninstall { .. }
        | Op::RemoveStorePackage { .. }
        | Op::CompactVhdx { .. }
        | Op::DockerPrune => None,
    }
}

/// Tira operações sobre caminhos iguais ou dentro de outro caminho marcado (o item já sai junto
/// com a pasta: nada de operar duas vezes nem somar os bytes duas vezes), põe
/// `EmptyRecycleBin` primeiro (o que for para a Lixeira nesta mesma limpeza fica lá, nunca é
/// apagado de vez sem ter sido dito) e `CompactVhdx` por último (desliga o WSL e o Docker:
/// melhor depois do que é só apagar). O prune e a compactação nunca rodam juntos (ver
/// `resolve_docker_conflict`).
fn order_and_dedup(ops: Vec<(Op, u64)>) -> Vec<(Op, u64)> {
    // `Path` ordena por componentes: o que está dentro de X vem logo depois de X.
    let mut by_path: Vec<(usize, &Path)> = ops
        .iter()
        .enumerate()
        .filter_map(|(i, (op, _))| op_path(op).map(|p| (i, p)))
        .collect();
    by_path.sort_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(&b.0)));
    let mut drop = vec![false; ops.len()];
    let mut kept: Option<&Path> = None;
    for (i, p) in by_path {
        match kept {
            Some(k) if p.starts_with(k) => drop[i] = true,
            _ => kept = Some(p),
        }
    }
    let (mut first, rest): (Vec<_>, Vec<_>) = ops
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !drop[*i])
        .map(|(_, o)| o)
        .partition(|(op, _)| *op == Op::EmptyRecycleBin);
    let (compact, middle): (Vec<_>, Vec<_>) = rest.into_iter().partition(|(op, _)| is_compact(op));
    first.extend(middle);
    first.extend(compact);
    first
}

/// Caminho do item relativo à pasta analisada (o começo comum não diz nada na lista); o
/// caminho inteiro se não estiver dentro dela.
pub fn relative_label(path: &std::path::Path, root: &std::path::Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => path.display().to_string(),
    }
}

/// Divide a dica em trechos de texto e de comando (entre crases), sem as crases.
pub fn hint_segments(s: &str) -> Vec<(&str, bool)> {
    s.split('`')
        .enumerate()
        .filter(|(_, t)| !t.is_empty())
        .map(|(i, t)| (t, i % 2 == 1))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::insights::VirtDisk;
    use std::path::{Path, PathBuf};

    pub(crate) fn ins(
        id: &'static str,
        risk: Risk,
        action: Action,
        nodes: Vec<NodeId>,
        size: u64,
    ) -> Insight {
        Insight {
            id,
            title: id.into(),
            explanation: String::new(),
            risk,
            size,
            nodes,
            action,
            needs_admin: false,
            hint: None,
            programs: Vec::new(),
            disks: Vec::new(),
            companions: Vec::new(),
        }
    }

    /// C:\ { Temp\ {a 10, b 20}, proj\node_modules\ {x 100}, Down\ {old 50} }
    pub(crate) fn tree() -> Tree {
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let temp = t.add(r, "Temp", true, 0, None);
        t.add(temp, "a", false, 10, None);
        t.add(temp, "b", false, 20, None);
        let p = t.add(r, "proj", true, 0, None);
        let nm = t.add(p, "node_modules", true, 0, None);
        t.add(nm, "x", false, 100, None);
        let d = t.add(r, "Down", true, 0, None);
        t.add(d, "old", false, 50, None);
        t.finalize();
        t
    }

    pub(crate) fn id(t: &Tree, p: &str) -> NodeId {
        t.find(Path::new(p)).unwrap()
    }

    pub(crate) fn list(t: &Tree) -> Vec<Insight> {
        vec![
            ins(
                "temp",
                Risk::Seguro,
                Action::DeleteContents,
                vec![id(t, r"C:\Temp")],
                30,
            ),
            ins(
                "recycle_bin",
                Risk::Seguro,
                Action::EmptyRecycleBin,
                vec![],
                7,
            ),
            ins(
                "dev_artifacts",
                Risk::Revisar,
                Action::Recycle,
                vec![id(t, r"C:\proj\node_modules"), id(t, r"C:\Down\old")],
                150,
            ),
            ins("system_files", Risk::Informativo, Action::None, vec![], 999),
        ]
    }

    #[test]
    fn nothing_picked_means_no_ops() {
        let t = tree();
        assert!(selected_ops(&list(&t), &HashSet::new(), &t, false).is_empty());
    }

    #[test]
    fn picks_become_ops_with_sizes() {
        let t = tree();
        let nm = id(&t, r"C:\proj\node_modules");
        let picks = HashSet::from([
            Pick::Whole("temp"),
            Pick::Whole("recycle_bin"),
            Pick::Item("dev_artifacts", nm),
            Pick::Whole("system_files"),
        ]);
        let ops = selected_ops(&list(&t), &picks, &t, false);
        assert_eq!(
            ops,
            vec![
                (Op::EmptyRecycleBin, 7),
                (Op::DeleteContents(PathBuf::from(r"C:\Temp")), 30),
                (Op::Recycle(PathBuf::from(r"C:\proj\node_modules")), 100),
            ],
            "esvaziar a Lixeira vem primeiro: o que vai para ela agora fica lá"
        );
    }

    #[test]
    fn nested_picks_are_dropped() {
        let t = tree();
        let temp_a = id(&t, r"C:\Temp\a");
        let proj = id(&t, r"C:\proj");
        let nm = id(&t, r"C:\proj\node_modules");
        let mut l = list(&t);
        // Uma categoria Revisar com itens dentro de outros itens marcados.
        l.push(ins(
            "outros",
            Risk::Revisar,
            Action::Recycle,
            vec![temp_a, proj, nm],
            0,
        ));
        let picks = HashSet::from([
            Pick::Whole("temp"),
            Pick::Item("outros", temp_a),
            Pick::Item("outros", proj),
            Pick::Item("outros", nm),
            Pick::Item("dev_artifacts", nm),
        ]);
        let ops = selected_ops(&l, &picks, &t, false);
        assert_eq!(
            ops,
            vec![
                (Op::DeleteContents(PathBuf::from(r"C:\Temp")), 30),
                (Op::Recycle(PathBuf::from(r"C:\proj")), 100),
            ],
            r"Temp\a está no conteúdo de Temp; node_modules está dentro de proj"
        );
    }

    #[test]
    fn needs_admin_is_skipped_when_not_elevated() {
        let t = tree();
        let mut l = list(&t);
        l[0].needs_admin = true;
        let picks = HashSet::from([Pick::Whole("temp")]);
        assert!(selected_ops(&l, &picks, &t, false).is_empty());
        assert_eq!(selected_ops(&l, &picks, &t, true).len(), 1);
    }

    #[test]
    fn dead_nodes_are_not_offered() {
        let mut t = tree();
        let l = list(&t);
        let nm = id(&t, r"C:\proj\node_modules");
        t.remove(nm);
        let picks = HashSet::from([Pick::Item("dev_artifacts", nm)]);
        assert!(selected_ops(&l, &picks, &t, false).is_empty());
    }

    #[test]
    fn explain_finds_insight_by_ancestor() {
        let t = tree();
        let l = list(&t);
        assert_eq!(insight_for(&l, &t, id(&t, r"C:\Temp\a")), Some("temp"));
        assert_eq!(
            insight_for(&l, &t, id(&t, r"C:\proj\node_modules\x")),
            Some("dev_artifacts")
        );
        assert_eq!(insight_for(&l, &t, id(&t, r"C:\proj")), None);
        let set = explainable_set(&l);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn headline_excludes_informative() {
        let t = tree();
        let mut l = list(&t);
        assert_eq!(headline_total(&l), 30 + 7 + 150);
        let mut progs = ins("programs", Risk::Revisar, Action::None, vec![], 5000);
        progs.programs = vec![Program {
            name: "A".into(),
            publisher: String::new(),
            size: Some(5000),
            installed: None,
            uninstall: "u".into(),
            location: None,
            source: crate::insights::ProgramSource::Registry,
        }];
        l.push(progs);
        assert_eq!(headline_total(&l), 30 + 7 + 150, "programas não somam");
    }

    /// C:\ { Docker\docker_data.vhdx 40, WSL\ext4.vhdx 60, avd\ {Pixel.avd\ {x 30}, Pixel.ini 1} }
    pub(crate) fn virt_tree() -> Tree {
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let d = t.add(r, "Docker", true, 0, None);
        t.add(d, "docker_data.vhdx", false, 40, None);
        let w = t.add(r, "WSL", true, 0, None);
        t.add(w, "ext4.vhdx", false, 60, None);
        let a = t.add(r, "avd", true, 0, None);
        let px = t.add(a, "Pixel.avd", true, 0, None);
        t.add(px, "x", false, 30, None);
        t.add(a, "Pixel.ini", false, 1, None);
        t.finalize();
        t
    }

    pub(crate) fn virt_list(t: &Tree) -> Vec<Insight> {
        let ubuntu = id(t, r"C:\WSL\ext4.vhdx");
        let docker = id(t, r"C:\Docker\docker_data.vhdx");
        let mut compact = ins(
            "vhdx_compact",
            Risk::Seguro,
            Action::CompactVhdx,
            vec![ubuntu, docker],
            100,
        );
        compact.needs_admin = true;
        compact.disks = vec![
            VirtDisk {
                node: ubuntu,
                path: t.path(ubuntu),
                label: "Ubuntu".into(),
                distro: Some("Ubuntu".into()),
            },
            VirtDisk {
                node: docker,
                path: t.path(docker),
                label: "Docker".into(),
                distro: None,
            },
        ];
        let avd = id(t, r"C:\avd\Pixel.avd");
        let ini = id(t, r"C:\avd\Pixel.ini");
        let mut avds = ins(
            "android_avds",
            Risk::Revisar,
            Action::Recycle,
            vec![avd, ini],
            31,
        );
        avds.companions = vec![(avd, ini)];
        vec![
            compact,
            ins(
                "docker_prune",
                Risk::Revisar,
                Action::DockerPrune,
                vec![],
                70,
            ),
            avds,
        ]
    }

    #[test]
    fn disk_picks_become_compact_ops_last() {
        let t = virt_tree();
        let l = virt_list(&t);
        let ubuntu = id(&t, r"C:\WSL\ext4.vhdx");
        let avd = id(&t, r"C:\avd\Pixel.avd");
        let picks = HashSet::from([
            Pick::Item("vhdx_compact", ubuntu),
            Pick::Whole("docker_prune"),
            Pick::Item("android_avds", avd),
        ]);
        assert!(
            selected_ops(&l, &picks, &t, false)
                .iter()
                .all(|(op, _)| !matches!(op, Op::CompactVhdx { .. })),
            "compactar precisa de admin"
        );
        let ops = selected_ops(&l, &picks, &t, true);
        assert_eq!(
            ops,
            vec![
                (Op::DockerPrune, 70),
                (Op::Recycle(PathBuf::from(r"C:\avd\Pixel.avd")), 30),
                (Op::Recycle(PathBuf::from(r"C:\avd\Pixel.ini")), 1),
                (
                    Op::CompactVhdx {
                        path: PathBuf::from(r"C:\WSL\ext4.vhdx"),
                        distro: Some("Ubuntu".into()),
                    },
                    60
                ),
            ],
            "compactar vai por último"
        );
        // Prune e compactação juntos não dão certo: fica só o prune, com o aviso.
        let (kept, conflict) = resolve_docker_conflict(ops.clone());
        assert!(conflict);
        assert_eq!(kept, ops[..3].to_vec());
        assert_eq!(
            DOCKER_CONFLICT_NOTE,
            "Limpar o Docker precisa dele aberto e compactar precisa dele fechado: vou limpar \
             o Docker agora; feche o Docker Desktop e compacte depois."
        );
        let (kept, conflict) = resolve_docker_conflict(ops[1..].to_vec());
        assert!(!conflict, "sem o prune, a compactação fica");
        assert_eq!(kept, ops[1..].to_vec());
        // Compactar não entra no total: só se sabe quanto libera no fim.
        assert_eq!(freeable(&ops), 70 + 30 + 1);
        assert_eq!(clean_label(&ops), "Limpar selecionados (101 B + compactar)");
        let only = vec![ops[3].clone()];
        assert_eq!(clean_label(&only), "Limpar selecionados (compactar)");
        assert_eq!(clean_label(&[]), "Limpar selecionados");
        assert_eq!(clean_label(&ops[..1]), "Limpar selecionados (70 B)");
    }

    #[test]
    fn docker_disk_has_no_distro() {
        let t = virt_tree();
        let l = virt_list(&t);
        let docker = id(&t, r"C:\Docker\docker_data.vhdx");
        let picks = HashSet::from([Pick::Item("vhdx_compact", docker)]);
        assert_eq!(
            selected_ops(&l, &picks, &t, true),
            vec![(
                Op::CompactVhdx {
                    path: PathBuf::from(r"C:\Docker\docker_data.vhdx"),
                    distro: None,
                },
                40
            )]
        );
    }

    #[test]
    fn avd_and_its_ini_are_one_item() {
        let t = virt_tree();
        let l = virt_list(&t);
        let avd = id(&t, r"C:\avd\Pixel.avd");
        let ubuntu = id(&t, r"C:\WSL\ext4.vhdx");
        let docker = id(&t, r"C:\Docker\docker_data.vhdx");
        assert_eq!(items(&l[2]), vec![avd], "o .ini não aparece sozinho");
        assert_eq!(item_size(&l[2], &t, avd), 31, "pasta + .ini");
        assert_eq!(items(&l[0]), vec![ubuntu, docker], "um item por disco");
        assert!(items(&l[1]).is_empty());
        assert!(checkable(&l[0]) && checkable(&l[1]) && checkable(&l[2]));
    }

    #[test]
    fn headline_skips_compaction_and_rows_say_current_size() {
        let t = virt_tree();
        let l = virt_list(&t);
        assert_eq!(
            headline_total(&l),
            70 + 31,
            "o disco inteiro não é liberável"
        );
        assert_eq!(size_text(&l[0]), "tamanho atual 100 B");
        assert_eq!(size_text(&l[1]), "70 B");
        assert_eq!(disk_size_text(60), "tamanho atual 60 B");
    }

    #[test]
    fn explain_prefers_deepest_and_skips_programs() {
        let t = tree();
        let mut l = list(&t);
        let proj = id(&t, r"C:\proj");
        let nm = id(&t, r"C:\proj\node_modules");
        let x = id(&t, r"C:\proj\node_modules\x");
        // "programs" cobre C:\proj e vem antes na lista; outra cobre C:\proj inteiro também.
        l.insert(
            0,
            ins("programs", Risk::Revisar, Action::None, vec![proj], 100),
        );
        assert_eq!(
            insight_for(&l, &t, x),
            Some("dev_artifacts"),
            "a mais funda"
        );
        l.push(ins(
            "outra",
            Risk::Revisar,
            Action::Recycle,
            vec![proj],
            100,
        ));
        assert_eq!(insight_for(&l, &t, nm), Some("dev_artifacts"));
        assert_eq!(
            insight_for(&l, &t, proj),
            Some("outra"),
            "programs só se nada mais"
        );
        l.retain(|i| i.id != "outra");
        assert_eq!(insight_for(&l, &t, proj), Some("programs"));
    }

    #[test]
    fn relative_label_strips_scan_root() {
        let root = Path::new(r"D:\teste");
        assert_eq!(
            relative_label(Path::new(r"D:\teste\Downloads\São Paulo.jpg"), root),
            r"Downloads\São Paulo.jpg"
        );
        assert_eq!(relative_label(Path::new(r"C:\x"), root), r"C:\x");
        assert_eq!(relative_label(root, root), r"D:\teste");
    }

    #[test]
    fn hint_segments_strip_backticks() {
        assert_eq!(
            hint_segments("rode `powercfg /h off` num terminal"),
            vec![
                ("rode ", false),
                ("powercfg /h off", true),
                (" num terminal", false)
            ]
        );
        assert_eq!(hint_segments("sem comando"), vec![("sem comando", false)]);
        assert_eq!(
            hint_segments("`a` e `b`"),
            vec![("a", true), (" e ", false), ("b", true)]
        );
    }

    #[test]
    fn prune_drops_dead_and_resizes() {
        let mut t = tree();
        let mut s = Suggestions::new(list(&t));
        let a = id(&t, r"C:\Temp\a");
        let nm = id(&t, r"C:\proj\node_modules");
        let old = id(&t, r"C:\Down\old");
        t.remove(a);
        t.remove(nm);
        s.prune(&t);
        let temp = s.list().iter().find(|i| i.id == "temp").unwrap();
        assert_eq!(temp.size, 20, "a pasta fica, tamanho refeito");
        let dev = s.list().iter().find(|i| i.id == "dev_artifacts").unwrap();
        assert_eq!(dev.nodes, vec![old]);
        assert_eq!(dev.size, 50);
        assert!(
            s.list().iter().any(|i| i.id == "recycle_bin"),
            "sem nós: fica"
        );
        let mut s2 = Suggestions::new(vec![ins(
            "docker_prune",
            Risk::Informativo,
            Action::None,
            vec![],
            0,
        )]);
        s2.prune(&t);
        assert_eq!(s2.list().len(), 1, "o Docker parado (0 B) não pisca");
        t.remove(old);
        s.prune(&t);
        assert!(
            !s.list().iter().any(|i| i.id == "dev_artifacts"),
            "vazia some"
        );
    }

    #[test]
    fn programs_merge_and_removal() {
        let t = tree();
        let mut s = Suggestions::new(list(&t));
        assert!(s.loading_programs());
        let prog = |name: &str, size| Program {
            name: name.into(),
            publisher: String::new(),
            size: Some(size),
            installed: None,
            uninstall: "u.exe".into(),
            location: None,
            source: crate::insights::ProgramSource::Registry,
        };
        s.set_programs(vec![prog("A", 1000), prog("B", 5)], &t);
        assert!(!s.loading_programs());
        let ids: Vec<&str> = s.list().iter().map(|i| i.id).collect();
        assert_eq!(
            ids,
            vec![
                "temp",
                "recycle_bin",
                "programs",
                "dev_artifacts",
                "system_files"
            ],
            "programas entram na ordem (Revisar, por tamanho)"
        );
        s.remove_program(&prog("A", 1000), &t);
        let p = s.list().iter().find(|i| i.id == "programs").unwrap();
        assert_eq!(p.programs.len(), 1);
        s.remove_program(&prog("B", 5), &t);
        assert!(!s.list().iter().any(|i| i.id == "programs"));
    }

    #[test]
    fn removed_program_stays_out_when_sized_list_arrives() {
        use crate::insights::ProgramSource;
        let t = tree();
        let mut s = Suggestions::new(Vec::new());
        let p = |name: &str, size| Program {
            name: name.into(),
            publisher: String::new(),
            size,
            installed: None,
            uninstall: String::new(),
            location: None,
            source: ProgramSource::Store {
                full_name: format!("{name}_1"),
            },
        };
        s.set_programs(vec![p("A", None), p("B", None)], &t);
        s.remove_program(&p("A", None), &t);
        // A lista medida chega depois (segunda mensagem da thread).
        s.set_programs(vec![p("A", Some(9)), p("B", Some(5))], &t);
        let progs = &s
            .list()
            .iter()
            .find(|i| i.id == "programs")
            .unwrap()
            .programs;
        assert_eq!(progs.len(), 1, "A foi removido e não volta");
        assert_eq!(progs[0].name, "B");
        assert_eq!(progs[0].size, Some(5));
    }

    #[test]
    fn programs_nodes_are_explainable_but_not_marked() {
        let t = tree();
        let mut l = list(&t);
        let down = id(&t, r"C:\Down");
        l.push(ins("programs", Risk::Revisar, Action::None, vec![down], 50));
        assert!(explainable_set(&l).contains(&down));
        let marked = marked_set(&l);
        assert!(!marked.contains(&down), "programas não poluem o mapa");
        assert!(marked.contains(&id(&t, r"C:\Temp")));
        assert_eq!(marked.len(), explainable_set(&l).len() - 1);
    }

    #[test]
    fn removing_a_store_app_keeps_same_named_entries() {
        use crate::insights::ProgramSource;
        let t = tree();
        let mut s = Suggestions::new(Vec::new());
        let p = |full: &str| Program {
            name: "Bloco".into(),
            publisher: String::new(),
            size: Some(1),
            installed: None,
            uninstall: String::new(),
            location: None,
            source: ProgramSource::Store {
                full_name: full.into(),
            },
        };
        s.set_programs(vec![p("Bloco_1"), p("Bloco_2")], &t);
        s.remove_program(&p("Bloco_1"), &t);
        let progs = &s
            .list()
            .iter()
            .find(|i| i.id == "programs")
            .unwrap()
            .programs;
        assert_eq!(progs.len(), 1);
        assert_eq!(progs[0], p("Bloco_2"));
    }
}
