//! Sugestões sobre projetos parados (pastas de dependências/build) e Downloads antigos.
//! Puro: usa só a `Tree` (nomes e mtimes), sem tocar o disco.

use super::{Action, Env, Insight, Risk};
use crate::tree::{NodeId, Tree};
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

const STALE_PROJECT: Duration = Duration::from_secs(90 * 86_400);
const OLD_DOWNLOAD: Duration = Duration::from_secs(180 * 86_400);

/// Nomes de pastas de artefatos; ignoradas ao calcular a atividade de um projeto.
const ARTIFACT_NAMES: [&str; 6] = ["node_modules", "target", "build", "dist", ".venv", "venv"];

fn is_artifact_name(name: &str) -> bool {
    ARTIFACT_NAMES.iter().any(|a| name.eq_ignore_ascii_case(a))
}

fn older_than(now: SystemTime, m: SystemTime, limit: Duration) -> bool {
    now.duration_since(m).is_ok_and(|d| d > limit)
}

fn has_file(tree: &Tree, siblings: &[NodeId], name: &str) -> bool {
    siblings.iter().any(|&k| {
        let n = tree.node(k);
        !n.is_dir && n.name.eq_ignore_ascii_case(name)
    })
}

/// A pasta é candidata a artefato removível do projeto cujos filhos são `siblings`?
fn qualifies(tree: &Tree, name: &str, siblings: &[NodeId]) -> bool {
    match name.to_lowercase().as_str() {
        "node_modules" | ".venv" | "venv" => true,
        "target" => has_file(tree, siblings, "Cargo.toml"),
        "build" | "dist" => has_file(tree, siblings, "package.json"),
        _ => false,
    }
}

/// Nós que nunca entram: Windows, ProgramData, Program Files*, perfil/AppData do usuário.
/// Resolvidos por caminho (`find`), então valem mesmo se a árvore não começa na raiz do volume.
fn excluded_roots(tree: &Tree, env: &Env) -> HashSet<NodeId> {
    let mut set = HashSet::new();
    let abs = |p: std::path::PathBuf| p.is_absolute().then_some(p);
    let fixed = [
        abs(env.windows.clone()),
        abs(env.local_app_data.clone()),
        abs(env.app_data.clone()),
        abs(env.user_profile.join("AppData")),
        abs(env.system_drive.join("ProgramData")),
    ];
    set.extend(fixed.into_iter().flatten().filter_map(|p| tree.find(&p)));
    let mut volumes = vec![tree.root()];
    volumes.extend(abs(env.system_drive.clone()).and_then(|p| tree.find(&p)));
    for v in volumes {
        for &k in tree.children(v) {
            let n = tree.node(k);
            if n.is_dir && n.name.to_lowercase().starts_with("program files") {
                set.insert(k);
            }
        }
    }
    set
}

/// Pasta `AppData` de qualquer usuário (tem `Local`/`Roaming` dentro).
fn is_appdata(tree: &Tree, id: NodeId) -> bool {
    tree.node(id).name.eq_ignore_ascii_case("appdata")
        && tree.children(id).iter().any(|&c| {
            let n = tree.node(c);
            n.is_dir
                && (n.name.eq_ignore_ascii_case("local") || n.name.eq_ignore_ascii_case("roaming"))
        })
}

/// Mtime mais recente dos arquivos sob `project`, ignorando as pastas de artefatos.
fn newest_project_file(tree: &Tree, project: NodeId) -> Option<SystemTime> {
    let mut best: Option<SystemTime> = None;
    let mut stack = vec![project];
    while let Some(cur) = stack.pop() {
        for &k in tree.children(cur) {
            let n = tree.node(k);
            if n.is_dir {
                if !is_artifact_name(&n.name) {
                    stack.push(k);
                }
            } else if let Some(m) = n.mtime {
                best = Some(best.map_or(m, |b| b.max(m)));
            }
        }
    }
    best
}

/// Mtime mais recente do nó e de tudo abaixo dele.
fn newest_any(tree: &Tree, id: NodeId) -> Option<SystemTime> {
    let mut best: Option<SystemTime> = None;
    let mut stack = vec![id];
    while let Some(cur) = stack.pop() {
        if let Some(m) = tree.node(cur).mtime {
            best = Some(best.map_or(m, |b| b.max(m)));
        }
        stack.extend(tree.children(cur).iter().copied());
    }
    best
}

fn build(
    id: &'static str,
    title: &str,
    explanation: &str,
    hint: Option<&str>,
    tree: &Tree,
    mut nodes: Vec<NodeId>,
) -> Option<Insight> {
    nodes.sort_by_key(|&n| std::cmp::Reverse(tree.node(n).size));
    let size: u64 = nodes.iter().map(|&n| tree.node(n).size).sum();
    if size == 0 {
        return None;
    }
    Some(Insight {
        id,
        title: title.into(),
        explanation: explanation.into(),
        risk: Risk::Revisar,
        size,
        nodes,
        action: Action::Recycle,
        needs_admin: false,
        hint: hint.map(str::to_string),
        programs: Vec::new(),
        disks: Vec::new(),
        companions: Vec::new(),
    })
}

/// Pastas de dependências/build (`node_modules`, `target`, `.venv`…) de projetos sem arquivos
/// modificados há mais de 90 dias. Uma passada só; caminhos só são montados por quem usar os nós.
pub fn stale_artifacts(tree: &Tree, env: &Env, now: SystemTime) -> Option<Insight> {
    let skip = excluded_roots(tree, env);
    let mut found = Vec::new();
    let mut stack = vec![tree.root()];
    while let Some(cur) = stack.pop() {
        let kids = tree.children(cur);
        let mut cands = Vec::new();
        for &k in kids {
            let n = tree.node(k);
            if !n.is_dir || skip.contains(&k) || is_appdata(tree, k) {
                continue;
            }
            if is_artifact_name(&n.name) && qualifies(tree, &n.name, kids) {
                cands.push(k); // não desce: um artefato dentro de outro conta uma vez só
            } else {
                stack.push(k);
            }
        }
        if !cands.is_empty()
            && newest_project_file(tree, cur).is_some_and(|m| older_than(now, m, STALE_PROJECT))
        {
            found.extend(cands);
        }
    }
    build(
        "dev_artifacts",
        "Projetos parados",
        "Pastas de dependências e build (node_modules, target, .venv…) de projetos que você não mexe há mais de 90 dias. Dá para recriá-las quando voltar ao projeto.",
        Some("Vão para a Lixeira; para recriar, rode `npm install`, `cargo build` ou equivalente."),
        tree,
        found,
    )
}

/// Itens soltos em `Downloads` com mais de 180 dias (pasta: vale o arquivo mais novo dentro).
pub fn old_downloads(tree: &Tree, env: &Env, now: SystemTime) -> Option<Insight> {
    let dir = env.user_profile.join("Downloads");
    if !dir.is_absolute() {
        return None;
    }
    let dl = tree.find(&dir)?;
    let old: Vec<NodeId> = tree
        .children(dl)
        .iter()
        .copied()
        .filter(|&k| newest_any(tree, k).is_some_and(|m| older_than(now, m, OLD_DOWNLOAD)))
        .collect();
    build(
        "old_downloads",
        "Downloads antigos",
        "Arquivos e pastas em Downloads que ninguém mexe há mais de 180 dias.",
        None,
        tree,
        old,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_DAYS: u64 = 1000;
    const DAY: Duration = Duration::from_secs(86_400);

    fn env() -> Env {
        Env::for_test("C:\\")
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + DAY * NOW_DAYS as u32
    }

    fn ago(days: u64) -> Option<SystemTime> {
        Some(now() - DAY * days as u32)
    }

    /// Cria (ou reaproveita) as pastas do caminho e retorna a última.
    fn dir(t: &mut Tree, path: &str) -> NodeId {
        let mut cur = t.root();
        for c in path.trim_start_matches("C:\\").split('\\') {
            let found = t
                .children(cur)
                .iter()
                .copied()
                .find(|&k| &*t.node(k).name == c);
            cur = found.unwrap_or_else(|| t.add(cur, c, true, 0, None));
        }
        cur
    }

    fn file(t: &mut Tree, dir_path: &str, name: &str, size: u64, days: Option<u64>) {
        let d = dir(t, dir_path);
        t.add(d, name, false, size, days.and_then(ago));
    }

    const GB: u64 = 1 << 30;

    fn project(pkg_days: u64) -> Tree {
        let mut t = Tree::new("C:\\".into());
        file(&mut t, r"C:\dev\app", "package.json", 100, Some(pkg_days));
        file(&mut t, r"C:\dev\app\node_modules\x", "i.js", GB, Some(1));
        t.finalize();
        t
    }

    #[test]
    fn stale_node_modules() {
        let t = project(200);
        let i = stale_artifacts(&t, &env(), now()).unwrap();
        assert_eq!(i.id, "dev_artifacts");
        assert_eq!(i.risk, Risk::Revisar);
        assert_eq!(i.action, Action::Recycle);
        assert_eq!(i.size, GB);
        assert_eq!(i.nodes.len(), 1);
        assert_eq!(&*t.node(i.nodes[0]).name, "node_modules");
        // O mesmo projeto mexido há 10 dias: fora.
        assert!(stale_artifacts(&project(10), &env(), now()).is_none());
    }

    #[test]
    fn target_needs_cargo_toml() {
        let mut t = Tree::new("C:\\".into());
        file(&mut t, r"C:\dev\a", "main.rs", 10, Some(300));
        file(&mut t, r"C:\dev\a\target\debug", "x.bin", GB, Some(300));
        file(&mut t, r"C:\dev\b", "Cargo.toml", 10, Some(300));
        file(&mut t, r"C:\dev\b\target\debug", "y.bin", 5, Some(300));
        t.finalize();
        let i = stale_artifacts(&t, &env(), now()).unwrap();
        assert_eq!(i.size, 5);
        assert_eq!(i.nodes.len(), 1);
        assert_eq!(
            t.path(i.nodes[0]),
            std::path::PathBuf::from(r"C:\dev\b\target")
        );
    }

    #[test]
    fn nested_node_modules_once() {
        let mut t = Tree::new("C:\\".into());
        file(&mut t, r"C:\p", "package.json", 1, Some(400));
        file(
            &mut t,
            r"C:\p\node_modules\a\node_modules\b",
            "f",
            70,
            Some(1),
        );
        file(&mut t, r"C:\p\node_modules\a", "g", 30, Some(1));
        t.finalize();
        let i = stale_artifacts(&t, &env(), now()).unwrap();
        assert_eq!(i.nodes.len(), 1);
        assert_eq!(i.size, 100);
    }

    #[test]
    fn recency_ignores_artifact_folders_and_missing_mtime() {
        // node_modules recente (npm install) não deve reativar o projeto.
        let t = project(200);
        assert!(stale_artifacts(&t, &env(), now()).is_some());
        // Sem nenhum mtime fora dos artefatos: não inclui.
        let mut t = Tree::new("C:\\".into());
        file(&mut t, r"C:\p", "package.json", 1, None);
        file(&mut t, r"C:\p\node_modules", "f", 50, Some(500));
        t.finalize();
        assert!(stale_artifacts(&t, &env(), now()).is_none());
    }

    #[test]
    fn skips_system_and_appdata_dirs() {
        let mut t = Tree::new("C:\\".into());
        file(&mut t, r"C:\Program Files\X", "package.json", 1, Some(500));
        file(
            &mut t,
            r"C:\Program Files\X\node_modules",
            "f",
            50,
            Some(500),
        );
        file(
            &mut t,
            r"C:\Users\t\AppData\Local\y",
            "package.json",
            1,
            Some(500),
        );
        file(
            &mut t,
            r"C:\Users\t\AppData\Local\y\node_modules",
            "f",
            60,
            Some(500),
        );
        file(&mut t, r"C:\Users\t\proj", "package.json", 1, Some(500));
        file(&mut t, r"C:\Users\t\proj\node_modules", "f", 7, Some(500));
        t.finalize();
        let i = stale_artifacts(&t, &env(), now()).unwrap();
        assert_eq!(i.size, 7);
    }

    #[test]
    fn old_downloads_180() {
        let mut t = Tree::new("C:\\".into());
        file(&mut t, r"C:\Users\t\Downloads", "old.zip", 100, Some(200));
        file(&mut t, r"C:\Users\t\Downloads", "new.zip", 50, Some(30));
        file(&mut t, r"C:\Users\t\Downloads\pasta", "a", 10, Some(300));
        file(&mut t, r"C:\Users\t\Downloads\mista", "a", 20, Some(300));
        file(&mut t, r"C:\Users\t\Downloads\mista", "b", 20, Some(5));
        t.finalize();
        let env = Env::for_test("C:\\");
        let i = old_downloads(&t, &env, now()).unwrap();
        assert_eq!(i.id, "old_downloads");
        assert_eq!(i.size, 110);
        assert_eq!(i.nodes.len(), 2);
        assert_eq!(i.action, Action::Recycle);
    }

    #[test]
    fn old_downloads_empty_env_is_none() {
        let mut t = Tree::new("C:\\".into());
        file(&mut t, "C:\\Downloads", "old.zip", 100, Some(200));
        t.finalize();
        let env = Env {
            user_profile: std::path::PathBuf::new(),
            local_app_data: std::path::PathBuf::new(),
            app_data: std::path::PathBuf::new(),
            windows: std::path::PathBuf::new(),
            system_drive: std::path::PathBuf::new(),
        };
        assert!(old_downloads(&t, &env, now()).is_none());
    }

    const BS: char = '\\';

    fn rooted(root: &str) -> Tree {
        Tree::new(root.into())
    }

    fn add_path(t: &mut Tree, rel: &str, name: &str, size: u64, days: u64) {
        let mut cur = t.root();
        for c in rel.split(BS).filter(|c| !c.is_empty()) {
            let found = t
                .children(cur)
                .iter()
                .copied()
                .find(|&k| &*t.node(k).name == c);
            cur = found.unwrap_or_else(|| t.add(cur, c, true, 0, None));
        }
        t.add(cur, name, false, size, ago(days));
    }

    #[test]
    fn partial_tree_skips_own_appdata() {
        let mut t = rooted(r"C:\Users\t");
        add_path(
            &mut t,
            r"AppData\Local\Programs\app\resources\app",
            "package.json",
            1,
            500,
        );
        add_path(
            &mut t,
            r"AppData\Local\Programs\app\resources\app\node_modules",
            "f",
            90,
            500,
        );
        add_path(&mut t, r"projetos\p", "package.json", 1, 500);
        add_path(&mut t, r"projetos\p\node_modules", "f", 5, 500);
        t.finalize();
        let i = stale_artifacts(&t, &env(), now()).unwrap();
        assert_eq!(i.size, 5);
    }

    #[test]
    fn other_users_appdata_skipped() {
        let mut t = rooted(r"C:\Users");
        add_path(
            &mut t,
            r"other\AppData\Local\Programs\app",
            "package.json",
            1,
            500,
        );
        add_path(
            &mut t,
            r"other\AppData\Local\Programs\app\node_modules",
            "f",
            90,
            500,
        );
        t.finalize();
        assert!(stale_artifacts(&t, &env(), now()).is_none());
    }

    #[test]
    fn normal_project_in_partial_tree_included() {
        let mut t = rooted(r"C:\Users\t");
        add_path(&mut t, r"projetos\p", "package.json", 1, 500);
        add_path(&mut t, r"projetos\p\node_modules", "f", 5, 500);
        t.finalize();
        assert_eq!(stale_artifacts(&t, &env(), now()).unwrap().size, 5);
    }
}
