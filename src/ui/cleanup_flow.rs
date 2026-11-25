//! Estado da tela do mapa que vai além do mapa: sugestões, painel, diálogo e as threads de
//! limpeza, desinstalação, reanálise da pasta do programa e recálculo das sugestões.
//!
//! Fluxo: painel/menu → `plan` → diálogo de confirmação → `execute` numa thread (progresso no
//! diálogo) → `Tree::remove` do que saiu → sugestões podadas na hora e recalculadas numa
//! thread → resumo, com a oferta de esvaziar a Lixeira.

use super::MapState;
use super::confirm::{self, Dialog, JobMsg, Kind, Outcome, Resized};
use super::map_view::dominant_kinds;
use super::search::SearchIndex;
use super::sidebar::{self, SidebarAction, SidebarState};
use super::suggestions::{self, Suggestions};
use crate::cleanup::{CleanupPlan, CleanupReport, Op, bin_max_bytes, plan};
use crate::format::format_bytes;
use crate::insights::known::allowed_system_paths;
use crate::insights::store::{fill_store_sizes, merge_programs, store_apps};
use crate::insights::{Env, Insight, Program, SysFacts, analyze, programs::installed_programs};
use crate::kind::FileKind;
use crate::scan::{ScanEvent, start_scan_with};
use crate::tree::{NodeId, Tree};
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

/// Sugestões da árvore com o ambiente e a Lixeira reais (chame fora da thread da UI).
pub fn analyze_now(tree: &Tree) -> Vec<Insight> {
    analyze(
        tree,
        &Env::from_system(),
        &SysFacts::gather(),
        SystemTime::now(),
    )
}

/// Resultado do recálculo depois de uma mudança na árvore.
pub struct Refresh {
    pub insights: Vec<Insight>,
    /// Índice da busca refeito (só quando entraram nós novos, `replace_subtree`).
    pub index: Option<SearchIndex>,
    /// Cores por tipo (`dominant_kinds`) da árvore recalculada.
    pub kinds: Vec<FileKind>,
}

/// Recalcula as sugestões, as cores por tipo (e o índice da busca, se pedido) numa thread,
/// sobre uma referência da árvore; a UI continua desenhando a mesma árvore.
pub fn start_refresh(tree: Arc<Tree>, rebuild_index: bool) -> Receiver<Refresh> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let insights = analyze_now(&tree);
        let kinds = dominant_kinds(&tree);
        let index = rebuild_index.then(|| SearchIndex::build(&tree));
        drop(tree); // solta a referência antes de avisar: a UI pode mudar a árvore sem copiar
        let _ = tx.send(Refresh {
            insights,
            index,
            kinds,
        });
    });
    rx
}

/// Lê os programas instalados (registro) e os apps da Microsoft Store numa thread, já juntos
/// sem duplicar, e manda a lista logo. Depois mede pela própria pasta os apps da Store que
/// sobraram da junção (só leitura, numa thread só, até `store::SIZE_TOTAL` ao todo; listar
/// `WindowsApps` sem admin é negado) e, se algum ganhou tamanho, manda a lista de novo.
pub fn start_programs() -> Receiver<Vec<Program>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut all = merge_programs(installed_programs(), store_apps());
        if tx.send(all.clone()).is_err() {
            return;
        }
        if fill_store_sizes(&mut all) > 0 {
            let _ = tx.send(all);
        }
    });
    rx
}

/// Limite da Lixeira na unidade da árvore; 0 (tudo "grande demais", ou seja, o aviso de que
/// sai de vez) se a raiz não tem letra de unidade.
fn bin_limit(tree: &Tree) -> u64 {
    let root = tree.path(tree.root());
    let s = root.to_string_lossy();
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(l), Some(':')) if l.is_ascii_alphabetic() => bin_max_bytes(l),
        _ => 0,
    }
}

fn make_plan(ops: Vec<(Op, u64)>, tree: &Tree) -> CleanupPlan {
    plan(
        ops,
        &allowed_system_paths(&Env::from_system()),
        bin_limit(tree),
    )
}

/// Reanálise da pasta de um programa desinstalado.
struct Rescan {
    name: String,
    path: PathBuf,
    rx: Receiver<ScanEvent>,
}

/// Mudança na árvore. Enquanto o recálculo das sugestões tem uma referência da árvore, as
/// mudanças esperam na fila (`pending`): mudar ali copiaria a árvore inteira na thread da UI.
enum Mutation {
    /// Nós que saíram do disco (já resolvidos, só leitura).
    Remove(Vec<NodeId>),
    /// Arquivos com tamanho novo (discos virtuais compactados).
    Resize(Vec<(NodeId, u64)>),
    /// Resultado da reanálise da pasta de um programa (`None`: a reanálise falhou).
    Rescanned {
        name: String,
        path: PathBuf,
        sub: Option<Tree>,
    },
}

/// Desinstalação mandada para segundo plano: o resultado vira aviso no rodapé.
struct Background {
    kind: Kind,
    rx: Receiver<JobMsg>,
}

pub struct MapScreen {
    pub map: MapState,
    pub sugg: Suggestions,
    pub side: SidebarState,
    dialog: Option<Dialog>,
    /// Sobe a cada diálogo novo: cada um é uma janela nova (sem o tamanho do anterior).
    dialog_gen: u64,
    dialog_opened: Instant,
    refresh_rx: Option<Receiver<Refresh>>,
    /// Pedir mais um recálculo quando o atual chegar (`true`: refazer o índice da busca).
    refresh_again: Option<bool>,
    pending: Vec<Mutation>,
    background: Option<Background>,
    programs_rx: Option<Receiver<Vec<Program>>>,
    rescan: Option<Rescan>,
}

impl MapScreen {
    pub fn new(
        map: MapState,
        insights: Vec<Insight>,
        programs_rx: Option<Receiver<Vec<Program>>>,
    ) -> MapScreen {
        let mut s = MapScreen {
            map,
            sugg: Suggestions::new(insights),
            side: SidebarState::default(),
            dialog: None,
            dialog_gen: 0,
            dialog_opened: Instant::now(),
            refresh_rx: None,
            refresh_again: None,
            pending: Vec::new(),
            background: None,
            programs_rx,
            rescan: None,
        };
        s.sync();
        s
    }

    fn open(&mut self, dialog: Dialog) {
        self.dialog_gen += 1;
        self.dialog_opened = Instant::now();
        self.dialog = Some(dialog);
    }

    /// Um diálogo está aberto (atalhos do mapa ficam desligados).
    pub fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// Limpeza, desinstalação (mesmo em segundo plano), reanálise ou recálculo em andamento:
    /// não deixa começar outra ação destrutiva, nem "Nova análise" (que largaria esta tela e o
    /// resultado do trabalho em andamento).
    pub fn busy(&self) -> bool {
        matches!(self.dialog, Some(Dialog::Running { .. }))
            || self.background.is_some()
            || self.rescan.is_some()
            || self.refresh_rx.is_some()
    }

    /// Sugestões mudaram: atualiza o que o mapa sabe delas e esquece marcações velhas.
    fn sync(&mut self) {
        self.map
            .set_explainable_marked(self.sugg.explainable(), self.sugg.marked());
        self.side.retain_valid(self.sugg.list());
    }

    /// Recalcula as sugestões numa thread; se já há um recálculo, pede outro para depois (nunca
    /// larga uma thread que ainda segura a árvore).
    fn request_refresh(&mut self, rebuild_index: bool) {
        if self.refresh_rx.is_some() {
            self.refresh_again = Some(self.refresh_again.unwrap_or(false) || rebuild_index);
        } else {
            self.refresh_rx = Some(start_refresh(Arc::clone(&self.map.tree), rebuild_index));
        }
    }

    /// A árvore para mudar. Só é chamada sem recálculo em andamento, então ninguém mais tem a
    /// árvore e `make_mut` não copia (conferido em debug).
    fn tree_for_mutation(&mut self) -> &mut Tree {
        debug_assert!(
            self.refresh_rx.is_none(),
            "mudança com recálculo em andamento"
        );
        debug_assert_eq!(
            Arc::strong_count(&self.map.tree),
            1,
            "a árvore seria copiada inteira"
        );
        self.map.tree_mut()
    }

    /// Aplica agora, ou enfileira se o recálculo ainda segura a árvore.
    fn mutate(&mut self, m: Mutation) {
        self.mutate_all(vec![m]);
    }

    /// Várias mudanças de uma vez (um recálculo só no fim).
    fn mutate_all(&mut self, ms: Vec<Mutation>) {
        if ms.is_empty() {
            return;
        }
        if self.refresh_rx.is_some() {
            self.pending.extend(ms);
        } else {
            self.apply_mutations(ms);
        }
    }

    fn apply_mutations(&mut self, ms: Vec<Mutation>) {
        let (mut changed, mut index) = (false, false);
        for m in ms {
            match m {
                Mutation::Remove(ids) => {
                    if !ids.is_empty() {
                        self.tree_for_mutation().remove_many(&ids);
                        changed = true;
                    }
                }
                Mutation::Resize(sizes) => {
                    for (id, size) in sizes {
                        changed |= self.tree_for_mutation().set_file_size(id, size).is_some();
                    }
                }
                Mutation::Rescanned { name, path, sub } => {
                    let (c, i) = self.apply_rescan(&name, &path, sub);
                    changed |= c;
                    index |= i;
                }
            }
        }
        if changed {
            self.tree_changed(index);
        }
    }

    /// Troca a subárvore da pasta do programa (ou tira, se a pasta sumiu). Devolve (mudou a
    /// árvore, entrou nó novo).
    fn apply_rescan(&mut self, name: &str, path: &Path, sub: Option<Tree>) -> (bool, bool) {
        let node = self.map.tree.find(path);
        match (node, sub) {
            (Some(id), Some(sub)) => {
                let left = sub.node(sub.root()).size;
                if self.tree_for_mutation().replace_subtree(id, &sub).is_none() {
                    self.map.notice = Some("Reanalise para atualizar o mapa.".to_string());
                    return (false, false);
                }
                self.map.notice = Some(if left > 0 {
                    format!(
                        "A pasta de {name} ainda tem {}. Se o desinstalador ainda estiver \
                         aberto, reanalise depois para atualizar.",
                        format_bytes(left)
                    )
                } else {
                    format!("Pasta de {name} atualizada.")
                });
                (true, true)
            }
            (Some(id), None) if !path.exists() => {
                self.tree_for_mutation().remove(id);
                self.map.notice = Some(format!("A pasta de {name} saiu do disco."));
                (true, false)
            }
            _ => {
                self.map.notice = Some(format!(
                    "Não consegui reanalisar a pasta de {name}. Reanalise para atualizar."
                ));
                (false, false)
            }
        }
    }

    /// Depois de mudar a árvore: mapa, sugestões podadas agora e recalculadas numa thread.
    fn tree_changed(&mut self, rebuild_index: bool) {
        self.map.after_remove();
        self.sugg.prune(&self.map.tree);
        self.sync();
        self.request_refresh(rebuild_index);
    }

    /// Recebe o que as threads terminaram. Chame a cada quadro.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let mut waiting = false;
        // Até duas listas: a primeira logo, a segunda com os apps da Store medidos.
        if let Some(rx) = &self.programs_rx {
            match rx.try_recv() {
                Ok(p) => {
                    self.sugg.set_programs(p, &self.map.tree);
                    self.sync();
                    waiting = true;
                }
                Err(TryRecvError::Empty) => waiting = true,
                Err(TryRecvError::Disconnected) => {
                    if self.sugg.loading_programs() {
                        self.sugg.set_programs(Vec::new(), &self.map.tree);
                        self.sync();
                    }
                    self.programs_rx = None;
                }
            }
        }
        waiting |= self.poll_refresh();
        waiting |= self.poll_job();
        waiting |= self.poll_background();
        waiting |= self.poll_rescan();
        if waiting {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Devolve `true` enquanto o recálculo roda.
    fn poll_refresh(&mut self) -> bool {
        let Some(rx) = &self.refresh_rx else {
            return false;
        };
        let got = match rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => return true,
            // A thread morreu (pânico): o desenrolar já soltou a árvore.
            Err(TryRecvError::Disconnected) => None,
        };
        self.refresh_rx = None;
        if let Some(r) = got {
            if let Some(index) = r.index {
                self.map.set_search_index(index);
            }
            // Calculadas sobre esta mesma árvore: as mudanças esperam na fila até aqui.
            self.map.set_kinds(r.kinds);
            self.sugg.set_base(r.insights, &self.map.tree);
            self.sync();
        }
        // A thread soltou a árvore antes de mandar: agora dá para aplicar a fila.
        let pending = std::mem::take(&mut self.pending);
        if !pending.is_empty() {
            self.apply_mutations(pending);
        }
        if let Some(index) = self.refresh_again.take() {
            self.request_refresh(index);
        }
        self.refresh_rx.is_some()
    }

    /// Devolve `true` enquanto o trabalho do diálogo roda.
    fn poll_job(&mut self) -> bool {
        let Some(Dialog::Running {
            rx, done, total, ..
        }) = &mut self.dialog
        else {
            return false;
        };
        let finished = loop {
            match rx.try_recv() {
                Ok(JobMsg::Progress(d, t)) => (*done, *total) = (d, t),
                Ok(JobMsg::Done(report, bin, resized)) => break Some((report, bin, resized)),
                Err(TryRecvError::Empty) => break None,
                Err(TryRecvError::Disconnected) => break Some((stuck_report(), 0, Vec::new())),
            }
        };
        let Some((report, bin_after, resized)) = finished else {
            return true;
        };
        let Some(Dialog::Running { kind, .. }) = self.dialog.take() else {
            unreachable!("conferido acima");
        };
        self.finish(&kind, &report, &resized);
        self.open(Dialog::Result {
            kind,
            report,
            bin_after,
            show_skipped: false,
            focused: false,
        });
        false
    }

    /// Desinstalação em segundo plano: devolve `true` enquanto roda.
    fn poll_background(&mut self) -> bool {
        let Some(bg) = &self.background else {
            return false;
        };
        let report = loop {
            match bg.rx.try_recv() {
                Ok(JobMsg::Progress(..)) => {}
                Ok(JobMsg::Done(report, ..)) => break report,
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => break stuck_report(),
            }
        };
        let Background { kind, .. } = self.background.take().expect("conferido acima");
        self.finish(&kind, &report, &[]);
        false
    }

    /// Aplica o resultado na árvore e nas sugestões. `resized`: discos que passaram pela
    /// compactação, com o tamanho relido (ficam no mapa, com o tamanho novo).
    fn finish(&mut self, kind: &Kind, report: &CleanupReport, resized: &[Resized]) {
        match kind {
            Kind::Cleanup | Kind::EmptyBin => {
                self.side.picks.clear();
                let tree = &self.map.tree;
                let disks: Vec<NodeId> = resized.iter().filter_map(|(p, _)| tree.find(p)).collect();
                // Compactar põe o disco em `done`, mas ele não saiu do disco.
                let ids: Vec<NodeId> = confirm::resolve_done(tree, report)
                    .into_iter()
                    .filter(|id| !disks.contains(id))
                    .collect();
                let sizes: Vec<(NodeId, u64)> = resized
                    .iter()
                    .filter_map(|(p, size)| Some((tree.find(p)?, (*size)?)))
                    .filter(|&(id, size)| tree.node(id).size != size)
                    .collect();
                // Discos do Docker conhecidos pela sugestão de compactar.
                let docker_disks: Vec<NodeId> = self
                    .sugg
                    .list()
                    .iter()
                    .flat_map(|i| &i.disks)
                    .filter(|d| d.distro.is_none() && d.label == "Docker")
                    .map(|d| d.node)
                    .collect();
                let docker_compacted = docker_disks.iter().any(|n| disks.contains(n));
                if report.done.iter().any(|p| p == Path::new(DOCKER_DONE)) && !docker_compacted {
                    // O prune libera espaço dentro do disco do Docker; o arquivo só encolhe
                    // compactando.
                    self.map.notice = Some(
                        "Para devolver o espaço ao Windows, compacte o disco do Docker."
                            .to_string(),
                    );
                }
                // Só pega a árvore para mudar se algo dela mudou (a Lixeira não está nela).
                let mut ms = Vec::new();
                if !ids.is_empty() {
                    ms.push(Mutation::Remove(ids));
                }
                if !sizes.is_empty() {
                    ms.push(Mutation::Resize(sizes));
                }
                if ms.is_empty() {
                    self.request_refresh(false);
                } else {
                    self.mutate_all(ms);
                }
            }
            Kind::Uninstall(p) => {
                if let Some((_, why)) = report.skipped.first() {
                    // Desinstalador cancelado ou com erro (ou a Store recusou): nada muda.
                    self.map.notice = Some(confirm::uninstall_failed(p, why));
                    return;
                }
                self.sugg.remove_program(p, &self.map.tree);
                self.sync();
                match p.location.clone() {
                    Some(path) if self.map.tree.find(&path).is_some() => {
                        self.rescan = Some(Rescan {
                            name: p.name.clone(),
                            rx: start_scan_with(path.clone(), false, false),
                            path,
                        });
                        self.map.notice = Some(format!("Reanalisando a pasta de {}…", p.name));
                    }
                    // Sem pasta conhecida, ou fora da pasta analisada.
                    _ => {
                        let verb = if p.is_store() {
                            "removido"
                        } else {
                            "desinstalado"
                        };
                        self.map.notice =
                            Some(format!("{} foi {verb}. Reanalise para atualizar.", p.name));
                    }
                }
            }
        }
    }

    /// Reanálise da pasta do programa: devolve `true` enquanto roda.
    fn poll_rescan(&mut self) -> bool {
        let Some(r) = &self.rescan else {
            return false;
        };
        let sub = loop {
            match r.rx.try_recv() {
                Ok(ScanEvent::Progress { .. } | ScanEvent::Preview { .. }) => {}
                Ok(ScanEvent::Done(sub, _)) => break Some(sub),
                Ok(ScanEvent::Failed(_)) | Err(TryRecvError::Disconnected) => break None,
                Err(TryRecvError::Empty) => return true,
            }
        };
        let Rescan { name, path, .. } = self.rescan.take().expect("conferido acima");
        self.mutate(Mutation::Rescanned { name, path, sub });
        false
    }

    /// "Por que isso está aqui?": devolve `true` se achou a sugestão (o painel deve abrir).
    pub fn explain(&mut self, id: NodeId) -> bool {
        let Some(iid) = suggestions::insight_for(self.sugg.list(), &self.map.tree, id) else {
            return false;
        };
        let Some(ins) = self.sugg.list().iter().find(|i| i.id == iid) else {
            return false;
        };
        self.side.reveal(ins, &mut self.map);
        true
    }

    /// "Mandar para a Lixeira…" do menu do mapa.
    pub fn ask_recycle(&mut self, id: NodeId) {
        if self.busy() || !self.map.tree.node(id).alive {
            if self.busy() {
                self.map.notice = Some("Espere a operação em andamento terminar.".to_string());
            }
            return;
        }
        let tree = &self.map.tree;
        let ops = vec![(Op::Recycle(tree.path(id)), tree.node(id).size)];
        let plan = make_plan(ops.clone(), tree);
        self.open(Dialog::confirm(Kind::Cleanup, plan, &ops));
    }

    /// Desenha o conteúdo do painel de sugestões e trata os botões dele.
    pub fn sidebar(&mut self, ui: &mut egui::Ui, elevated: bool) {
        let busy = self.busy();
        let action = sidebar::sidebar(
            ui,
            &self.sugg,
            &mut self.side,
            &mut self.map,
            elevated,
            busy,
        );
        if busy && !matches!(action, SidebarAction::None) {
            return;
        }
        let tree = &self.map.tree;
        match action {
            SidebarAction::None => {}
            SidebarAction::Clean => {
                let (ops, notes) = clean_ops(self.sugg.list(), &self.side.picks, tree, elevated);
                if !ops.is_empty() {
                    let plan = make_plan(ops.clone(), tree);
                    self.open(Dialog::confirm_with_notes(Kind::Cleanup, plan, &ops, notes));
                }
            }
            SidebarAction::Uninstall(p) => {
                let ops = vec![(confirm::uninstall_op(&p), p.size.unwrap_or(0))];
                let plan = make_plan(ops.clone(), tree);
                self.open(Dialog::confirm(Kind::Uninstall(p), plan, &ops));
            }
        }
    }

    /// Desenha o diálogo aberto e segue o fluxo conforme a resposta.
    pub fn show_dialog(&mut self, ctx: &egui::Context) {
        let allow_backdrop = self.dialog_opened.elapsed() >= confirm::BACKDROP_GRACE;
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        if !allow_backdrop {
            ctx.request_repaint_after(confirm::BACKDROP_GRACE);
        }
        match confirm::show(ctx, dialog, self.dialog_gen, allow_backdrop) {
            Outcome::Keep => {}
            Outcome::Close => self.dialog = None,
            Outcome::Run => {
                if let Some(Dialog::Confirm { kind, plan, .. }) = self.dialog.take() {
                    let total = plan.ops.len();
                    let measure_bin = matches!(kind, Kind::Cleanup);
                    self.open(Dialog::Running {
                        kind,
                        rx: confirm::start_job(plan, measure_bin),
                        done: 0,
                        total,
                    });
                    ctx.request_repaint();
                }
            }
            Outcome::OfferEmptyBin => {
                if let Some(Dialog::Result { bin_after, .. }) = self.dialog {
                    let ops = vec![(Op::EmptyRecycleBin, bin_after)];
                    let plan = make_plan(ops.clone(), &self.map.tree);
                    self.open(Dialog::confirm(Kind::EmptyBin, plan, &ops));
                }
            }
            Outcome::Background => {
                if let Some(Dialog::Running { kind, rx, .. }) = self.dialog.take() {
                    if let Kind::Uninstall(p) = &kind {
                        self.map.notice = Some(if p.is_store() {
                            format!("Removendo {} em segundo plano…", p.name)
                        } else {
                            format!("Desinstalador de {} rodando em segundo plano…", p.name)
                        });
                    }
                    self.background = Some(Background { kind, rx });
                }
            }
        }
    }
}

/// Operações do "Limpar selecionados", já sem a compactação quando o prune do Docker está junto,
/// e as observações para a confirmação.
fn clean_ops(
    list: &[Insight],
    picks: &std::collections::HashSet<suggestions::Pick>,
    tree: &Tree,
    elevated: bool,
) -> (Vec<(Op, u64)>, Vec<&'static str>) {
    let (ops, conflict) = suggestions::resolve_docker_conflict(suggestions::selected_ops(
        list, picks, tree, elevated,
    ));
    let notes = if conflict {
        vec![suggestions::DOCKER_CONFLICT_NOTE]
    } else {
        Vec::new()
    };
    (ops, notes)
}

/// O que `cleanup` põe em `done` quando o prune do Docker deu certo.
const DOCKER_DONE: &str = "Docker";

fn stuck_report() -> CleanupReport {
    CleanupReport {
        skipped: vec![(
            PathBuf::from("deixa-seco"),
            "a operação parou sem resposta".to_string(),
        )],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::map_view;

    /// D:\ { T\ {a 10, b 20} }
    fn screen() -> MapScreen {
        let mut t = Tree::new("D:\\".into());
        let r = t.root();
        let d = t.add(r, "T", true, 0, None);
        t.add(d, "a", false, 10, None);
        t.add(d, "b", false, 20, None);
        t.finalize();
        MapScreen::new(MapState::new(t), Vec::new(), None)
    }

    /// Recálculo "em andamento" controlado pelo teste: devolve quem manda o resultado e a
    /// referência da árvore que a thread seguraria.
    fn fake_refresh(s: &mut MapScreen) -> (mpsc::Sender<Refresh>, Arc<Tree>) {
        let (tx, rx) = mpsc::channel();
        s.refresh_rx = Some(rx);
        (tx, Arc::clone(&s.map.tree))
    }

    fn report(done: &[&str]) -> CleanupReport {
        CleanupReport {
            done: done.iter().map(PathBuf::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn mutation_waits_for_refresh_to_release_the_tree() {
        let mut s = screen();
        let a = s.map.tree.find(Path::new(r"D:\T\a")).unwrap();
        let (tx, held) = fake_refresh(&mut s);
        assert!(s.busy(), "recálculo em andamento bloqueia novas ações");
        let before = Arc::as_ptr(&s.map.tree);
        s.finish(&Kind::Cleanup, &report(&[r"D:\T\a"]), &[]);
        assert_eq!(s.pending.len(), 1, "mudança na fila");
        assert!(
            s.map.tree.node(a).alive,
            "árvore intacta enquanto compartilhada"
        );
        assert_eq!(Arc::as_ptr(&s.map.tree), before, "sem cópia");
        // A thread solta a árvore e manda o resultado: a fila é aplicada sem copiar.
        let kinds = map_view::dominant_kinds(&held);
        drop(held);
        tx.send(Refresh {
            insights: Vec::new(),
            index: None,
            kinds,
        })
        .unwrap();
        s.poll(&egui::Context::default());
        assert!(s.pending.is_empty());
        assert!(!s.map.tree.node(a).alive);
        assert_eq!(
            Arc::as_ptr(&s.map.tree),
            before,
            "mudou no lugar, sem cópia"
        );
        assert!(s.refresh_rx.is_some(), "novo recálculo depois da mudança");
    }

    /// D:\ { T\ {a 10, b 20}, WSL\ext4.vhdx 100 }
    fn disk_screen() -> MapScreen {
        let mut t = Tree::new("D:\\".into());
        let r = t.root();
        let d = t.add(r, "T", true, 0, None);
        t.add(d, "a", false, 10, None);
        t.add(d, "b", false, 20, None);
        let w = t.add(r, "WSL", true, 0, None);
        t.add(w, "ext4.vhdx", false, 100, None);
        t.finalize();
        MapScreen::new(MapState::new(t), Vec::new(), None)
    }

    #[test]
    fn compacted_disk_is_resized_not_removed() {
        let mut s = disk_screen();
        let disk = s.map.tree.find(Path::new(r"D:\WSL\ext4.vhdx")).unwrap();
        let a = s.map.tree.find(Path::new(r"D:\T\a")).unwrap();
        // `done` traz o disco compactado: ele continua no mapa, com o tamanho novo.
        let r = report(&[r"D:\WSL\ext4.vhdx", r"D:\T\a"]);
        let resized = vec![(PathBuf::from(r"D:\WSL\ext4.vhdx"), Some(40))];
        s.finish(&Kind::Cleanup, &r, &resized);
        assert!(s.map.tree.node(disk).alive, "compactar não apaga o disco");
        assert_eq!(s.map.tree.node(disk).size, 40);
        assert!(!s.map.tree.node(a).alive);
        assert_eq!(s.map.tree.node(s.map.tree.root()).size, 20 + 40);
        assert!(s.refresh_rx.is_some(), "sugestões recalculadas");
    }

    #[test]
    fn compacted_disk_waits_for_refresh_and_unmeasured_disk_stays() {
        let mut s = disk_screen();
        let disk = s.map.tree.find(Path::new(r"D:\WSL\ext4.vhdx")).unwrap();
        let (tx, held) = fake_refresh(&mut s);
        let r = report(&[r"D:\WSL\ext4.vhdx"]);
        s.finish(
            &Kind::Cleanup,
            &r,
            &[(PathBuf::from(r"D:\WSL\ext4.vhdx"), Some(70))],
        );
        assert_eq!(s.pending.len(), 1, "espera o recálculo soltar a árvore");
        assert_eq!(s.map.tree.node(disk).size, 100);
        let kinds = map_view::dominant_kinds(&held);
        drop(held);
        tx.send(Refresh {
            insights: Vec::new(),
            index: None,
            kinds,
        })
        .unwrap();
        s.poll(&egui::Context::default());
        assert_eq!(s.map.tree.node(disk).size, 70);
        // Sem medida (o arquivo não deu para ler): o nó fica como está, e não sai do mapa.
        let mut s = disk_screen();
        s.finish(
            &Kind::Cleanup,
            &r,
            &[(PathBuf::from(r"D:\WSL\ext4.vhdx"), None)],
        );
        assert!(s.map.tree.node(disk).alive);
        assert_eq!(s.map.tree.node(disk).size, 100);
        assert!(s.refresh_rx.is_some());
    }

    #[test]
    fn docker_prune_only_recomputes_and_says_compact() {
        let mut s = disk_screen();
        let before = s.map.tree.node(s.map.tree.root()).size;
        s.finish(&Kind::Cleanup, &report(&["Docker"]), &[]);
        assert_eq!(s.map.tree.node(s.map.tree.root()).size, before);
        assert!(s.refresh_rx.is_some());
        assert_eq!(
            s.map.notice.as_deref(),
            Some("Para devolver o espaço ao Windows, compacte o disco do Docker.")
        );
        // Compactou outro disco (não o do Docker): o aviso continua.
        let mut s = disk_screen();
        let disk = s.map.tree.find(Path::new(r"D:\WSL\ext4.vhdx")).unwrap();
        let mut compact = Insight {
            id: "vhdx_compact",
            title: String::new(),
            explanation: String::new(),
            risk: crate::insights::Risk::Seguro,
            size: 100,
            nodes: vec![disk],
            action: crate::insights::Action::CompactVhdx,
            needs_admin: true,
            hint: None,
            programs: Vec::new(),
            disks: vec![crate::insights::VirtDisk {
                node: disk,
                path: PathBuf::from(r"D:\WSL\ext4.vhdx"),
                label: "Ubuntu".into(),
                distro: Some("Ubuntu".into()),
            }],
            companions: Vec::new(),
        };
        s.sugg = Suggestions::new(vec![compact.clone()]);
        s.finish(
            &Kind::Cleanup,
            &report(&["Docker", r"D:\WSL\ext4.vhdx"]),
            &[(PathBuf::from(r"D:\WSL\ext4.vhdx"), Some(50))],
        );
        assert!(
            s.map.notice.is_some(),
            "o disco do Docker não foi compactado"
        );
        // O disco compactado é o do Docker: sem o aviso.
        let mut s = disk_screen();
        compact.disks[0].label = "Docker".into();
        compact.disks[0].distro = None;
        s.sugg = Suggestions::new(vec![compact]);
        s.finish(
            &Kind::Cleanup,
            &report(&["Docker", r"D:\WSL\ext4.vhdx"]),
            &[(PathBuf::from(r"D:\WSL\ext4.vhdx"), Some(50))],
        );
        assert_eq!(s.map.notice, None);
    }

    #[test]
    fn clean_with_prune_and_compaction_keeps_only_prune() {
        let s = disk_screen();
        let disk = s.map.tree.find(Path::new(r"D:\WSL\ext4.vhdx")).unwrap();
        let compact = Insight {
            id: "vhdx_compact",
            title: String::new(),
            explanation: String::new(),
            risk: crate::insights::Risk::Seguro,
            size: 100,
            nodes: vec![disk],
            action: crate::insights::Action::CompactVhdx,
            needs_admin: true,
            hint: None,
            programs: Vec::new(),
            disks: vec![crate::insights::VirtDisk {
                node: disk,
                path: PathBuf::from(r"D:\WSL\ext4.vhdx"),
                label: "Docker".into(),
                distro: None,
            }],
            companions: Vec::new(),
        };
        let prune = Insight {
            id: "docker_prune",
            size: 70,
            nodes: Vec::new(),
            action: crate::insights::Action::DockerPrune,
            needs_admin: false,
            disks: Vec::new(),
            ..compact.clone()
        };
        let list = vec![compact, prune];
        let both = std::collections::HashSet::from([
            suggestions::Pick::Item("vhdx_compact", disk),
            suggestions::Pick::Whole("docker_prune"),
        ]);
        let (ops, notes) = clean_ops(&list, &both, &s.map.tree, true);
        assert_eq!(ops, vec![(Op::DockerPrune, 70)]);
        assert_eq!(notes, vec![suggestions::DOCKER_CONFLICT_NOTE]);
        let only = std::collections::HashSet::from([suggestions::Pick::Item("vhdx_compact", disk)]);
        let (ops, notes) = clean_ops(&list, &only, &s.map.tree, true);
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0].0, Op::CompactVhdx { .. }));
        assert!(notes.is_empty());
    }

    #[test]
    fn compacted_sizes_are_measured_for_each_compact_op() {
        let plan = CleanupPlan {
            ops: vec![
                Op::DockerPrune,
                Op::CompactVhdx {
                    path: PathBuf::from(r"C:\a.vhdx"),
                    distro: None,
                },
                Op::Recycle(PathBuf::from(r"C:\x")),
                Op::CompactVhdx {
                    path: PathBuf::from(r"C:\sumiu.vhdx"),
                    distro: Some("Ubuntu".into()),
                },
            ],
            ..Default::default()
        };
        let got =
            confirm::measure_compacted(&plan, |p| (p == Path::new(r"C:\a.vhdx")).then_some(7));
        assert_eq!(
            got,
            vec![
                (PathBuf::from(r"C:\a.vhdx"), Some(7)),
                (PathBuf::from(r"C:\sumiu.vhdx"), None),
            ]
        );
    }

    #[test]
    fn empty_bin_never_takes_the_tree_for_mutation() {
        let mut s = screen();
        let (_tx, _held) = fake_refresh(&mut s);
        let before = Arc::as_ptr(&s.map.tree);
        s.finish(&Kind::EmptyBin, &report(&["Lixeira"]), &[]);
        assert!(s.pending.is_empty(), "nada da árvore saiu");
        assert_eq!(Arc::as_ptr(&s.map.tree), before);
        assert_eq!(s.refresh_again, Some(false), "recálculo pedido para depois");
    }

    #[test]
    fn mutation_without_refresh_applies_now_with_unique_tree() {
        let mut s = screen();
        let a = s.map.tree.find(Path::new(r"D:\T\a")).unwrap();
        assert_eq!(Arc::strong_count(&s.map.tree), 1);
        s.finish(&Kind::Cleanup, &report(&[r"D:\T\a"]), &[]);
        assert!(!s.map.tree.node(a).alive);
        assert!(s.refresh_rx.is_some());
    }

    #[test]
    fn uninstall_outside_scan_suggests_rescan() {
        let mut s = screen();
        let p = Program {
            name: "Foo".into(),
            publisher: String::new(),
            size: Some(1),
            installed: None,
            uninstall: "u".into(),
            source: crate::insights::ProgramSource::Registry,
            location: Some(PathBuf::from(r"C:\Program Files\Foo")),
        };
        s.finish(&Kind::Uninstall(p.clone()), &report(&["Foo"]), &[]);
        assert!(s.rescan.is_none());
        assert!(
            s.map
                .notice
                .as_deref()
                .unwrap()
                .contains("Reanalise para atualizar"),
            "{:?}",
            s.map.notice
        );
        let failed = CleanupReport {
            skipped: vec![(PathBuf::from("Foo"), "desinstalador retornou 1602".into())],
            ..Default::default()
        };
        s.finish(&Kind::Uninstall(p), &failed, &[]);
        assert!(s.map.notice.as_deref().unwrap().contains("1602"));
    }

    #[test]
    fn store_removal_notices() {
        let mut s = screen();
        let p = Program {
            name: "Bloco".into(),
            publisher: String::new(),
            size: None,
            installed: None,
            uninstall: String::new(),
            location: Some(PathBuf::from(
                r"C:\Program Files\WindowsApps\Bloco_1_x64__abc",
            )),
            source: crate::insights::ProgramSource::Store {
                full_name: "Bloco_1_x64__abc".into(),
            },
        };
        let failed = CleanupReport {
            skipped: vec![(PathBuf::from("Bloco"), "o app está aberto".into())],
            ..Default::default()
        };
        s.finish(&Kind::Uninstall(p.clone()), &failed, &[]);
        let n = s.map.notice.clone().unwrap();
        assert!(n.contains("Não consegui remover Bloco"), "{n}");
        assert!(n.contains("o app está aberto"), "{n}");
        // Fora da pasta analisada (D:\): sem reanálise, pede para reanalisar.
        s.finish(&Kind::Uninstall(p), &report(&["Bloco"]), &[]);
        assert!(s.rescan.is_none());
        let n = s.map.notice.clone().unwrap();
        assert!(n.contains("Bloco foi removido"), "{n}");
        assert!(n.contains("Reanalise para atualizar"), "{n}");
        // Pasta dentro da análise: reanalisa.
        let q = Program {
            name: "Tem".into(),
            publisher: String::new(),
            size: None,
            installed: None,
            uninstall: String::new(),
            location: Some(PathBuf::from(r"D:\T")),
            source: crate::insights::ProgramSource::Store {
                full_name: "Tem_1".into(),
            },
        };
        s.finish(&Kind::Uninstall(q), &report(&["Tem"]), &[]);
        assert!(s.rescan.is_some(), "reanalisa a pasta do app");
    }

    #[test]
    fn rescan_with_files_left_says_reanalise() {
        let mut s = screen();
        let mut sub = Tree::new(PathBuf::from(r"D:\T"));
        let r = sub.root();
        sub.add(r, "sobrou.dll", false, 5, None);
        sub.finalize();
        s.mutate(Mutation::Rescanned {
            name: "Foo".into(),
            path: PathBuf::from(r"D:\T"),
            sub: Some(sub),
        });
        let t = s.map.tree.find(Path::new(r"D:\T")).unwrap();
        assert_eq!(s.map.tree.node(t).size, 5, "subárvore trocada");
        assert!(
            s.map
                .notice
                .as_deref()
                .unwrap()
                .contains("reanalise depois")
        );
    }

    #[test]
    fn background_uninstall_keeps_screen_busy() {
        let mut s = screen();
        assert!(!s.busy());
        let (tx, rx) = mpsc::channel();
        let p = Program {
            name: "Foo".into(),
            publisher: String::new(),
            size: Some(1),
            installed: None,
            uninstall: "u".into(),
            source: crate::insights::ProgramSource::Registry,
            location: None,
        };
        s.background = Some(Background {
            kind: Kind::Uninstall(p),
            rx,
        });
        assert!(
            s.busy(),
            "desinstalação em segundo plano bloqueia Nova análise"
        );
        s.poll(&egui::Context::default());
        assert!(s.busy(), "continua enquanto não termina");
        drop(tx);
        s.poll(&egui::Context::default());
        assert!(s.background.is_none());
    }

    #[test]
    fn kinds_are_recomputed_by_the_refresh_not_the_ui() {
        // D:\ { V\ { v.mp4 30 }, x.jpg 10 }: tirar V muda a raiz de Vídeo para Imagem.
        let mut t = Tree::new("D:\\".into());
        let r = t.root();
        let v = t.add(r, "V", true, 0, None);
        t.add(v, "v.mp4", false, 30, None);
        t.add(r, "x.jpg", false, 10, None);
        t.finalize();
        let mut s = MapScreen::new(MapState::new(t), Vec::new(), None);
        let root = s.map.tree.root();
        assert_eq!(s.map.dominant_kind(root), FileKind::Video);
        s.finish(&Kind::Cleanup, &report(&[r"D:\V"]), &[]);
        assert!(!s.map.tree.node(v).alive);
        assert_eq!(
            s.map.dominant_kind(root),
            FileKind::Video,
            "a thread da UI não refaz as cores"
        );
        let ctx = egui::Context::default();
        let start = Instant::now();
        while s.refresh_rx.is_some() {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "recálculo travou"
            );
            std::thread::sleep(Duration::from_millis(5));
            s.poll(&ctx);
        }
        assert_eq!(
            s.map.dominant_kind(root),
            FileKind::Image,
            "cores novas chegaram com o recálculo"
        );
    }

    #[test]
    fn refresh_runs_off_thread_and_rebuilds_index() {
        let mut t = Tree::new("D:\\".into());
        let r = t.root();
        t.add(r, "Fotos de São Paulo.jpg", false, 10, None);
        t.finalize();
        let tree = Arc::new(t);
        let rx = start_refresh(Arc::clone(&tree), true);
        let got = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("recalculado");
        assert_eq!(
            got.kinds,
            map_view::dominant_kinds(&tree),
            "cores por tipo calculadas na thread"
        );
        assert_eq!(got.kinds[tree.root().0 as usize], FileKind::Image);
        let index = got.index.expect("índice refeito");
        assert_eq!(index.matches("sao paulo").len(), 1);
        assert!(got.insights.iter().all(|i| i.id != "temp"));
        // A thread solta a árvore antes de avisar: dá para mudar sem copiar.
        let mut tree = tree;
        assert!(Arc::get_mut(&mut tree).is_some());
        let rx = start_refresh(Arc::clone(&tree), false);
        assert!(
            rx.recv_timeout(Duration::from_secs(30))
                .unwrap()
                .index
                .is_none()
        );
    }

    #[test]
    fn bin_limit_without_drive_letter_is_zero() {
        let t = Tree::new(PathBuf::from(r"\\servidor\share"));
        assert_eq!(bin_limit(&t), 0);
    }

    #[test]
    fn programs_load_in_background() {
        let rx = start_programs();
        let v = rx.recv_timeout(Duration::from_secs(60)).expect("lista");
        assert!(v.iter().all(|p| !p.name.is_empty()));
    }
}
