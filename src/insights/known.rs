//! Categorias de caminho fixo: Temp, caches de navegador e de desenvolvimento, Windows Update,
//! Lixeira e arquivos de sistema.
//!
//! Temp tem um caminho do usuário e um do Windows (que exige administrador), então vira dois
//! insights: `temp` (usuário) e `temp_windows` (needs_admin).

use super::{Action, Env, Insight, Risk, SysFacts};
use crate::tree::{NodeId, Tree};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// Caminhos de sistema que a proteção de `cleanup` libera para apagar o conteúdo.
pub fn allowed_system_paths(env: &Env) -> Vec<PathBuf> {
    [
        env.windows.join("Temp"),
        env.windows.join("SoftwareDistribution\\Download"),
    ]
    .into_iter()
    .filter(|p| p.is_absolute())
    .collect()
}

struct Category {
    id: &'static str,
    title: &'static str,
    explanation: &'static str,
    risk: Risk,
    action: Action,
    needs_admin: bool,
    hint: Option<&'static str>,
    /// Padrões de caminho; um componente `*` casa qualquer subpasta.
    patterns: Vec<PathBuf>,
}

fn categories(env: &Env) -> Vec<Category> {
    let l = &env.local_app_data;
    let browser = |base: &str| -> Vec<PathBuf> {
        let ud = l.join(base).join("User Data").join("*");
        vec![ud.join("Cache"), ud.join("Code Cache")]
    };
    let mut browsers = browser("Google\\Chrome");
    browsers.extend(browser("Microsoft\\Edge"));
    browsers.push(l.join("Mozilla\\Firefox\\Profiles\\*\\cache2"));
    let sys = |n: &str| env.system_drive.join(n);
    vec![
        Category {
            id: "temp",
            title: "Arquivos temporários",
            explanation: "Arquivos que programas criam e esquecem. O Windows e os apps recriam o que precisarem.",
            risk: Risk::Seguro,
            action: Action::DeleteContents,
            needs_admin: false,
            hint: None,
            patterns: vec![l.join("Temp")],
        },
        Category {
            id: "temp_windows",
            title: "Temporários do Windows",
            explanation: "Arquivos temporários do sistema, que o Windows recria quando precisa.",
            risk: Risk::Seguro,
            action: Action::DeleteContents,
            needs_admin: true,
            hint: None,
            patterns: vec![env.windows.join("Temp")],
        },
        Category {
            id: "browser_cache",
            title: "Cache dos navegadores",
            explanation: "Cópias de páginas e imagens que o navegador guarda para abrir mais rápido; ele baixa de novo se precisar.",
            risk: Risk::Seguro,
            action: Action::DeleteContents,
            needs_admin: false,
            hint: None,
            patterns: browsers,
        },
        Category {
            id: "windows_update",
            title: "Downloads do Windows Update",
            explanation: "Arquivos de atualização já baixados; o Windows baixa de novo o que for necessário.",
            risk: Risk::Seguro,
            action: Action::DeleteContents,
            needs_admin: true,
            hint: Some("Se sobrar algo travado, use a Limpeza de Disco do Windows."),
            patterns: vec![env.windows.join("SoftwareDistribution\\Download")],
        },
        Category {
            id: "dev_caches",
            title: "Caches de desenvolvimento",
            explanation: "Pacotes baixados por npm, pip, Cargo, Gradle e NuGet; as ferramentas baixam de novo quando um projeto pedir.",
            risk: Risk::Seguro,
            action: Action::DeleteContents,
            needs_admin: false,
            hint: None,
            patterns: vec![
                l.join("npm-cache"),
                env.app_data.join("npm-cache"),
                l.join("pip\\Cache"),
                env.user_profile.join(".cargo\\registry\\cache"),
                env.user_profile.join(".gradle\\caches"),
                env.user_profile.join(".nuget\\packages"),
            ],
        },
        Category {
            id: "system_files",
            title: "Arquivos grandes do sistema",
            explanation: "Arquivos que o Windows usa para memória virtual e hibernação; não dá para apagá-los daqui.",
            risk: Risk::Informativo,
            action: Action::None,
            needs_admin: false,
            hint: None, // montado por arquivo em `analyze_known`
            patterns: vec![
                sys("pagefile.sys"),
                sys("hiberfil.sys"),
                sys("swapfile.sys"),
            ],
        },
    ]
}

const HIBER_HINT: &str = "Desligar a hibernação libera esse espaço: rode `powercfg /h off` num terminal como administrador.";
const PAGE_HINT: &str = "O Windows gerencia esse arquivo. Dá para reduzir em Configurações avançadas do sistema > Desempenho > Memória virtual.";

fn lower(c: Component<'_>) -> String {
    c.as_os_str().to_string_lossy().to_lowercase()
}

/// Resolve um padrão com `*` percorrendo a árvore (prefixo fixo via `find`, depois `children`).
/// Com `dirs_only`, os trechos literais precisam ser pastas (categorias que apagam o conteúdo).
fn resolve(tree: &Tree, pattern: &Path, dirs_only: bool) -> Vec<NodeId> {
    if !pattern.is_absolute() {
        return Vec::new();
    }
    let comps: Vec<Component<'_>> = pattern.components().collect();
    let star = comps
        .iter()
        .position(|c| lower(*c) == "*")
        .unwrap_or(comps.len());
    let prefix: PathBuf = comps[..star].iter().collect();
    let Some(start) = tree.find(&prefix) else {
        return Vec::new();
    };
    if dirs_only && !tree.node(start).is_dir {
        return Vec::new();
    }
    let mut cur = vec![start];
    for c in &comps[star..] {
        let wild = lower(*c) == "*";
        let want = lower(*c);
        let mut next = Vec::new();
        for &n in &cur {
            next.extend(tree.children(n).iter().copied().filter(|&k| {
                let node = tree.node(k);
                if wild {
                    node.is_dir
                } else {
                    (!dirs_only || node.is_dir) && node.name.to_lowercase() == want
                }
            }));
        }
        cur = next;
    }
    cur
}

/// Remove duplicatas e nós dentro de outro nó da lista (evita contar duas vezes).
pub(super) fn dedup_nested(tree: &Tree, mut nodes: Vec<NodeId>) -> Vec<NodeId> {
    nodes.sort_by_key(|n| n.0);
    nodes.dedup();
    let set: std::collections::HashSet<NodeId> = nodes.iter().copied().collect();
    nodes
        .into_iter()
        .filter(|&n| {
            let anc = tree.ancestors(n);
            !anc[..anc.len() - 1].iter().any(|a| set.contains(a))
        })
        .collect()
}

pub(super) fn analyze_known(
    tree: &Tree,
    env: &Env,
    facts: &SysFacts,
    _now: SystemTime,
) -> Vec<Insight> {
    let mut out = Vec::new();
    for cat in categories(env) {
        let nodes = dedup_nested(
            tree,
            cat.patterns
                .iter()
                .flat_map(|p| resolve(tree, p, cat.action == Action::DeleteContents))
                .collect(),
        );
        let size: u64 = nodes.iter().map(|&n| tree.node(n).size).sum();
        if size == 0 {
            continue;
        }
        let hint = if cat.id == "system_files" {
            let mut hints: Vec<&str> = Vec::new();
            for &n in &nodes {
                let h = if tree.node(n).name.eq_ignore_ascii_case("hiberfil.sys") {
                    HIBER_HINT
                } else {
                    PAGE_HINT
                };
                if !hints.contains(&h) {
                    hints.push(h);
                }
            }
            Some(hints.join(" "))
        } else {
            cat.hint.map(str::to_string)
        };
        out.push(Insight {
            id: cat.id,
            title: cat.title.into(),
            explanation: cat.explanation.into(),
            risk: cat.risk,
            size,
            nodes,
            action: cat.action,
            needs_admin: cat.needs_admin,
            hint,
            programs: Vec::new(),
            disks: Vec::new(),
            companions: Vec::new(),
        });
    }
    if facts.recycle_bin_bytes > 0 {
        out.push(Insight {
            id: "recycle_bin",
            title: "Lixeira".into(),
            explanation:
                "Arquivos que você já apagou mas que ainda ocupam espaço até esvaziar a Lixeira."
                    .into(),
            risk: Risk::Seguro,
            size: facts.recycle_bin_bytes,
            nodes: Vec::new(),
            action: Action::EmptyRecycleBin,
            needs_admin: false,
            hint: None,
            programs: Vec::new(),
            disks: Vec::new(),
            companions: Vec::new(),
        });
    }
    out
}

/// Tamanho total da Lixeira de todas as unidades, via `SHQueryRecycleBinW`.
pub fn recycle_bin_size() -> Option<u64> {
    use windows::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};
    use windows::core::PCWSTR;
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` é válido e tem `cbSize` preenchido; raiz nula consulta todas as unidades.
    unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info) }.ok()?;
    Some(info.i64Size.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::super::analyze;
    use super::*;
    use std::time::SystemTime;

    const T: &str = r"C:\Users\t\AppData\Local";

    fn tree_with(paths: &[(&str, u64)]) -> Tree {
        let mut t = Tree::new("C:\\".into());
        for (p, size) in paths {
            let mut cur = t.root();
            let comps: Vec<&str> = p.trim_start_matches("C:\\").split('\\').collect();
            for (i, c) in comps.iter().enumerate() {
                let existing = t
                    .children(cur)
                    .iter()
                    .copied()
                    .find(|&k| &*t.node(k).name == *c);
                cur = match existing {
                    Some(k) => k,
                    None if i + 1 == comps.len() => t.add(cur, c, false, *size, None),
                    None => t.add(cur, c, true, 0, None),
                };
            }
        }
        t.finalize();
        t
    }

    fn run(t: &Tree, bin: u64) -> Vec<Insight> {
        analyze(
            t,
            &Env::for_test("C:\\"),
            &SysFacts {
                recycle_bin_bytes: bin,
                ..SysFacts::default()
            },
            SystemTime::UNIX_EPOCH,
        )
    }

    fn get<'a>(v: &'a [Insight], id: &str) -> &'a Insight {
        v.iter().find(|i| i.id == id).unwrap()
    }

    #[test]
    fn finds_temp() {
        let t = tree_with(&[(&format!(r"{T}\Temp\a.tmp"), 500)]);
        let r = run(&t, 0);
        let i = get(&r, "temp");
        assert_eq!(i.size, 500);
        assert_eq!(i.risk, Risk::Seguro);
        assert_eq!(i.action, Action::DeleteContents);
        assert!(!i.needs_admin);
        assert_eq!(i.nodes.len(), 1);
    }

    #[test]
    fn windows_paths_need_admin() {
        let t = tree_with(&[
            (r"C:\Windows\Temp\x", 10),
            (r"C:\Windows\SoftwareDistribution\Download\y", 20),
        ]);
        let r = run(&t, 0);
        assert!(get(&r, "temp_windows").needs_admin);
        let wu = get(&r, "windows_update");
        assert!(wu.needs_admin);
        assert!(wu.hint.as_deref().unwrap().contains("Limpeza de Disco"));
        assert_eq!(allowed_system_paths(&Env::for_test("C:\\")).len(), 2);
    }

    #[test]
    fn chrome_profiles_glob() {
        let t = tree_with(&[
            (
                &format!(r"{T}\Google\Chrome\User Data\Profile 1\Cache\f"),
                100,
            ),
            (
                &format!(r"{T}\Google\Chrome\User Data\Default\Cache\f"),
                200,
            ),
            (
                &format!(r"{T}\Google\Chrome\User Data\Default\Code Cache\f"),
                5,
            ),
            (
                &format!(r"{T}\Google\Chrome\User Data\Default\Cookies"),
                999,
            ),
            (
                &format!(r"{T}\Mozilla\Firefox\Profiles\abc.default\cache2\f"),
                7,
            ),
        ]);
        let r = run(&t, 0);
        let i = get(&r, "browser_cache");
        assert_eq!(i.nodes.len(), 4);
        assert_eq!(i.size, 312);
    }

    #[test]
    fn no_double_counting() {
        let t = tree_with(&[(&format!(r"{T}\Temp\a"), 50)]);
        let env = Env::for_test("C:\\");
        let node = t.find(&env.local_app_data.join("Temp")).unwrap();
        let inner = t.find(&env.local_app_data.join("Temp\\a")).unwrap();
        let v = dedup_nested(&t, vec![inner, node, node]);
        assert_eq!(v, vec![node]);
    }

    #[test]
    fn skips_empty() {
        let t = tree_with(&[(r"C:\Other\file", 10)]);
        assert!(run(&t, 0).is_empty());
        let r = run(&t, 42);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].id, "recycle_bin");
        assert_eq!(r[0].action, Action::EmptyRecycleBin);
        assert_eq!(r[0].size, 42);
    }

    #[test]
    fn system_files_hints() {
        let t = tree_with(&[(r"C:\hiberfil.sys", 800), (r"C:\pagefile.sys", 400)]);
        let r = run(&t, 0);
        let i = get(&r, "system_files");
        assert_eq!(i.risk, Risk::Informativo);
        assert_eq!(i.action, Action::None);
        assert_eq!(i.size, 1200);
        let h = i.hint.as_deref().unwrap();
        assert!(h.contains("powercfg /h off") && h.contains("Memória virtual"));
    }

    #[test]
    fn order() {
        let t = tree_with(&[
            (&format!(r"{T}\Temp\a"), 10),
            (&format!(r"{T}\npm-cache\a"), 300),
            (r"C:\pagefile.sys", 9999),
        ]);
        let r = run(&t, 0);
        let ids: Vec<_> = r.iter().map(|i| i.id).collect();
        assert_eq!(ids, ["dev_caches", "temp", "system_files"]);
    }

    #[test]
    fn literal_file_not_matched_for_delete_contents() {
        // `Temp` é um arquivo: categoria que apaga conteúdo não pode casar.
        let t = tree_with(&[(&format!(r"{T}\Temp"), 500)]);
        assert!(run(&t, 0).is_empty());
    }

    #[test]
    fn empty_env_yields_nothing() {
        let t = tree_with(&[
            (r"C:\Temp\f", 10),
            (r"C:\Windows\Temp\f", 10),
            (r"C:\pagefile.sys", 10),
            (r"C:\Downloads\f", 10),
        ]);
        let env = Env {
            user_profile: PathBuf::new(),
            local_app_data: PathBuf::new(),
            app_data: PathBuf::new(),
            windows: PathBuf::new(),
            system_drive: PathBuf::new(),
        };
        let r = analyze(&t, &env, &SysFacts::default(), SystemTime::UNIX_EPOCH);
        assert!(r.is_empty(), "{r:?}");
        assert!(allowed_system_paths(&env).is_empty());
    }
}
