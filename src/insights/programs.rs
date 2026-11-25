//! Programas instalados, lidos do registro (chaves Uninstall). Só `installed_programs` toca o
//! registro; `programs_insight` é pura. Nada aqui executa desinstaladores.

use super::{Action, Insight, Risk, known};
use crate::tree::Tree;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use winreg::RegKey;
use winreg::enums::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
};

const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
const UNINSTALL_WOW: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";

/// De onde o programa veio (e, portanto, como ele sai).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProgramSource {
    /// Chave Uninstall do registro: sai pelo `UninstallString`.
    Registry,
    /// App da Microsoft Store: sai pelo `PackageManager` com o nome completo do pacote.
    Store { full_name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub name: String,
    pub publisher: String,
    /// Tamanho em bytes; `None` = desconhecido (sem `EstimatedSize` e pasta fora da árvore, ou
    /// app da Store em `WindowsApps`, que sem admin não dá para ler).
    pub size: Option<u64>,
    /// Data de instalação já formatada como `dd/mm/aaaa`.
    pub installed: Option<String>,
    /// `UninstallString` do registro (vazio para apps da Store).
    pub uninstall: String,
    pub location: Option<PathBuf>,
    pub source: ProgramSource,
}

impl Program {
    pub fn is_store(&self) -> bool {
        matches!(self.source, ProgramSource::Store { .. })
    }
}

/// Valores crus de uma chave Uninstall.
#[derive(Default)]
struct Raw {
    display_name: Option<String>,
    version: Option<String>,
    publisher: Option<String>,
    uninstall_string: Option<String>,
    install_location: Option<String>,
    install_date: Option<String>,
    system_component: Option<u32>,
    parent_key_name: Option<String>,
    release_type: Option<String>,
    estimated_size_kb: Option<u32>,
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// "YYYYMMDD" -> "dd/mm/aaaa"; qualquer outra coisa vira `None`.
fn format_install_date(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (y, m, d): (u32, u32, u32) = (
        s[0..4].parse().ok()?,
        s[4..6].parse().ok()?,
        s[6..8].parse().ok()?,
    );
    ((1..=12).contains(&m) && (1..=31).contains(&d) && y >= 1970)
        .then(|| format!("{d:02}/{m:02}/{y:04}"))
}

/// Aplica os filtros e monta o `Program`; `None` para entradas que não devem aparecer.
fn build_program(raw: &Raw) -> Option<Program> {
    let name = non_empty(&raw.display_name)?;
    let uninstall = non_empty(&raw.uninstall_string)?;
    if raw.system_component == Some(1) || non_empty(&raw.parent_key_name).is_some() {
        return None;
    }
    if let Some(r) = non_empty(&raw.release_type)
        && ["Update", "Hotfix", "Security Update"]
            .iter()
            .any(|x| r.eq_ignore_ascii_case(x))
    {
        return None;
    }
    let location = non_empty(&raw.install_location)
        .map(|l| l.trim_matches('"').trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from);
    Some(Program {
        name: name.to_string(),
        publisher: non_empty(&raw.publisher).unwrap_or_default().to_string(),
        size: raw
            .estimated_size_kb
            .filter(|&kb| kb > 0)
            .map(|kb| u64::from(kb) * 1024),
        installed: raw.install_date.as_deref().and_then(format_install_date),
        uninstall: uninstall.to_string(),
        location,
        source: ProgramSource::Registry,
    })
}

fn read_raw(key: &RegKey) -> Raw {
    let s = |n: &str| key.get_value::<String, _>(n).ok();
    Raw {
        display_name: s("DisplayName"),
        version: s("DisplayVersion"),
        publisher: s("Publisher"),
        uninstall_string: s("UninstallString"),
        install_location: s("InstallLocation"),
        install_date: s("InstallDate"),
        system_component: key.get_value::<u32, _>("SystemComponent").ok(),
        parent_key_name: s("ParentKeyName"),
        release_type: s("ReleaseType"),
        estimated_size_kb: key.get_value::<u32, _>("EstimatedSize").ok(),
    }
}

/// Lê as três raízes Uninstall (HKLM 64 bits, HKLM WOW6432Node, HKCU), sem duplicatas
/// (nome + versão), ordenado por nome. Falhas ao abrir uma chave só a pulam.
pub fn installed_programs() -> Vec<Program> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let roots = [
        (&hklm, UNINSTALL, KEY_READ | KEY_WOW64_64KEY),
        (&hklm, UNINSTALL_WOW, KEY_READ | KEY_WOW64_64KEY),
        (&hkcu, UNINSTALL, KEY_READ | KEY_WOW64_32KEY),
    ];
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = Vec::new();
    for (hive, path, flags) in roots {
        let Ok(root) = hive.open_subkey_with_flags(path, flags) else {
            continue;
        };
        for name in root.enum_keys().flatten() {
            let Ok(key) = root.open_subkey_with_flags(&name, KEY_READ) else {
                continue;
            };
            let raw = read_raw(&key);
            let Some(p) = build_program(&raw) else {
                continue;
            };
            let version = non_empty(&raw.version).unwrap_or_default().to_string();
            if seen.insert((p.name.to_lowercase(), version)) {
                out.push(p);
            }
        }
    }
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// Tamanho efetivo: o conhecido, ou o da pasta de instalação na árvore se o conhecido faltar ou
/// for menor que 10% dele. Pasta na árvore com 0 B não conta (é o que sobra de uma pasta que a
/// varredura não conseguiu ler, como `WindowsApps` sem admin): fica desconhecido, nunca 0 B.
fn effective_size(p: &Program, tree: &Tree) -> Option<u64> {
    let tree_size = p
        .location
        .as_ref()
        .and_then(|l| tree.find(l))
        .map(|n| tree.node(n).size)
        .filter(|&s| s > 0);
    match (p.size, tree_size) {
        (Some(known), Some(t)) if known < t / 10 => Some(t),
        (Some(known), _) => Some(known),
        (None, t) => t,
    }
}

/// Ordem da lista: tamanhos conhecidos primeiro (maiores antes), desconhecidos no fim; empate
/// pelo nome sem diferenciar maiúsculas.
pub fn sort_programs(list: &mut [Program]) {
    let by_name = |a: &Program, b: &Program| a.name.to_lowercase().cmp(&b.name.to_lowercase());
    list.sort_by(|a, b| match (a.size, b.size) {
        (Some(x), Some(y)) => y.cmp(&x).then_with(|| by_name(a, b)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => by_name(a, b),
    });
}

/// Uma sugestão "Programas instalados"; cada programa mantém seu desinstalador (a ação do
/// insight é `None`). Ordenados por tamanho, maiores primeiro, desconhecidos no fim. O total só
/// soma os tamanhos conhecidos.
pub fn programs_insight(programs: &[Program], tree: &Tree) -> Option<Insight> {
    let drive = std::env::var_os("SystemDrive")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("C:"));
    programs_insight_on(programs, tree, &drive)
}

/// Caminho em minúsculas, com `\` e sem barra no fim, para comparar prefixos.
fn norm(p: &Path) -> String {
    p.to_string_lossy()
        .replace('/', "\\")
        .to_lowercase()
        .trim_end_matches('\\')
        .to_string()
}

/// O programa mora dentro da pasta varrida? Sem pasta conhecida, só entra quando a varredura
/// é a raiz do disco do sistema (onde os instaladores costumam ficar).
fn belongs_to_scan(location: Option<&Path>, scan_root: &Path, system_drive: &Path) -> bool {
    let root = norm(scan_root);
    match location {
        Some(loc) => {
            let loc = norm(loc);
            loc == root || loc.starts_with(&format!("{root}\\"))
        }
        None => root == norm(system_drive),
    }
}

/// Igual a [`programs_insight`], com o disco do sistema explícito (para testes).
fn programs_insight_on(programs: &[Program], tree: &Tree, system_drive: &Path) -> Option<Insight> {
    let scan_root = tree.path(tree.root());
    let mut list: Vec<Program> = programs
        .iter()
        .filter(|p| belongs_to_scan(p.location.as_deref(), &scan_root, system_drive))
        .map(|p| Program {
            size: effective_size(p, tree),
            ..p.clone()
        })
        .collect();
    if list.is_empty() {
        return None;
    }
    sort_programs(&mut list);
    let size = list.iter().filter_map(|p| p.size).sum();
    let nodes = known::dedup_nested(
        tree,
        list.iter()
            .filter_map(|p| p.location.as_ref().and_then(|l| tree.find(l)))
            .collect(),
    );
    Some(Insight {
        id: "programs",
        title: "Programas instalados".into(),
        explanation: "Programas e apps da Microsoft Store que ocupam espaço. Desinstale os que você não usa mais; o desinstalador do próprio programa (ou a Store) faz a limpeza.".into(),
        risk: Risk::Revisar,
        size,
        nodes,
        action: Action::None,
        needs_admin: false,
        hint: None,
        programs: list,
        disks: Vec::new(),
        companions: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    fn prog(name: &str, size: u64, loc: Option<&str>) -> Program {
        Program {
            name: name.into(),
            publisher: "P".into(),
            size: (size > 0).then_some(size),
            installed: None,
            uninstall: "x.exe /u".into(),
            location: loc.map(PathBuf::from),
            source: ProgramSource::Registry,
        }
    }

    fn store(name: &str, full: &str, loc: &str) -> Program {
        Program {
            source: ProgramSource::Store {
                full_name: full.into(),
            },
            uninstall: String::new(),
            ..prog(name, 0, Some(loc))
        }
    }

    #[test]
    fn only_programs_inside_the_scan_are_listed() {
        let c = Tree::new("C:\\".into());
        let list = [
            prog("Jogo", 37, Some("D:/Games/Estudio/Jogo")),
            prog("Editor", 5, Some(r"C:\Program Files\Editor")),
            prog("SemPasta", 3, None),
        ];
        let names = |i: Option<Insight>| -> Vec<String> {
            i.map(|i| i.programs.into_iter().map(|p| p.name).collect())
                .unwrap_or_default()
        };
        // Varrendo o C: (disco do sistema): fica o do C: e o sem pasta; o do D: some.
        assert_eq!(
            names(programs_insight_on(&list, &c, Path::new("C:"))),
            ["Editor", "SemPasta"]
        );
        // Varrendo o D:: só o que mora no D:; sem pasta não aparece fora do disco do sistema.
        let d = Tree::new("D:\\".into());
        assert_eq!(
            names(programs_insight_on(&list, &d, Path::new("C:"))),
            ["Jogo"]
        );
        // Varrendo uma pasta: só o que está dentro dela.
        let pf = Tree::new(r"C:\Program Files".into());
        assert_eq!(
            names(programs_insight_on(&list, &pf, Path::new("C:"))),
            ["Editor"]
        );
        assert!(belongs_to_scan(
            Some(Path::new("c:/program files/editor/")),
            Path::new(r"C:\Program Files"),
            Path::new("C:")
        ));
        assert!(!belongs_to_scan(
            Some(Path::new(r"C:\Program FilesX\a")),
            Path::new(r"C:\Program Files"),
            Path::new("C:")
        ));
    }

    #[test]
    fn unknown_size_stays_unknown_sorted_last_and_not_summed() {
        // Pasta na árvore com 0 B (WindowsApps sem admin): desconhecido, não 0 B.
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let wa = t.add(r, "WindowsApps", true, 0, None);
        t.add(wa, "App_1.0_x64__abc", true, 0, None);
        t.finalize();
        let i = programs_insight(
            &[
                prog("Alfa", 0, None),
                store(
                    "Zeta Store",
                    "App_1.0_x64__abc",
                    r"C:\WindowsApps\App_1.0_x64__abc",
                ),
                prog("Grande", 30, None),
                prog("Pequeno", 10, None),
            ],
            &t,
        )
        .unwrap();
        let names: Vec<_> = i.programs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Grande", "Pequeno", "Alfa", "Zeta Store"]);
        assert_eq!(i.programs[3].size, None, "0 B na árvore = desconhecido");
        assert_eq!(i.size, 40, "só soma os conhecidos");
    }

    #[test]
    fn sort_tie_break_ignores_case() {
        let mut v = vec![
            prog("Beta", 10, None),
            prog("alfa", 10, None),
            prog("Zeta", 0, None),
            prog("gama", 0, None),
        ];
        sort_programs(&mut v);
        let names: Vec<_> = v.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["alfa", "Beta", "gama", "Zeta"]);
    }

    #[test]
    fn store_size_comes_from_tree_when_readable() {
        let t = tree_with_dir(r"C:\WindowsApps\App_1.0_x64__abc", GB);
        let s = store(
            "App",
            "App_1.0_x64__abc",
            r"C:\WindowsApps\App_1.0_x64__abc",
        );
        assert!(s.is_store());
        assert_eq!(
            programs_insight(&[s], &t).unwrap().programs[0].size,
            Some(GB)
        );
    }

    fn tree_with_dir(path: &str, size: u64) -> Tree {
        let mut t = Tree::new("C:\\".into());
        let mut cur = t.root();
        let comps: Vec<&str> = path.trim_start_matches("C:\\").split('\\').collect();
        for (i, c) in comps.iter().enumerate() {
            cur = if i + 1 == comps.len() {
                let d = t.add(cur, c, true, 0, None);
                t.add(d, "data.bin", false, size, None);
                d
            } else {
                t.add(cur, c, true, 0, None)
            };
        }
        t.finalize();
        t
    }

    #[test]
    fn program_size_fallback() {
        let t = tree_with_dir(r"C:\Apps\Big", 2 * GB);
        let p = prog("Big", 1 << 20, Some(r"C:\Apps\Big"));
        let i = programs_insight(&[p], &t).unwrap();
        assert_eq!(i.programs[0].size, Some(2 * GB));
        assert_eq!(i.size, 2 * GB);
        assert_eq!(i.nodes.len(), 1);
    }

    #[test]
    fn program_registry_size_kept_when_plausible() {
        let t = tree_with_dir(r"C:\Apps\Big", 2 * GB);
        let p = prog("Big", GB, Some(r"C:\Apps\Big"));
        assert_eq!(
            programs_insight(&[p], &t).unwrap().programs[0].size,
            Some(GB)
        );
        // Sem local na árvore: usa o do registro.
        let q = prog("Q", 5, Some(r"C:\Nada"));
        assert_eq!(
            programs_insight(&[q], &t).unwrap().programs[0].size,
            Some(5)
        );
        // Sem tamanho e sem pasta na árvore: desconhecido.
        let z = prog("Z", 0, Some(r"C:\Nada"));
        assert_eq!(programs_insight(&[z], &t).unwrap().programs[0].size, None);
    }

    #[test]
    fn insight_shape_sorted_and_summed() {
        let t = Tree::new("C:\\".into());
        let i = programs_insight(
            &[
                prog("A", 10, None),
                prog("B", 30, None),
                prog("C", 20, None),
            ],
            &t,
        )
        .unwrap();
        assert_eq!(i.id, "programs");
        assert_eq!(i.risk, Risk::Revisar);
        assert_eq!(i.action, Action::None);
        assert_eq!(i.size, 60);
        let names: Vec<_> = i.programs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["B", "C", "A"]);
        assert!(programs_insight(&[], &t).is_none());
    }

    fn raw(name: &str) -> Raw {
        Raw {
            display_name: Some(name.into()),
            uninstall_string: Some("u.exe".into()),
            ..Raw::default()
        }
    }

    #[test]
    fn build_program_filters() {
        assert!(build_program(&raw("A")).is_some());
        assert!(build_program(&Raw::default()).is_none());
        let mut r = raw("A");
        r.uninstall_string = None;
        assert!(build_program(&r).is_none());
        let mut r = raw("A");
        r.system_component = Some(1);
        assert!(build_program(&r).is_none());
        let mut r = raw("A");
        r.parent_key_name = Some("Office".into());
        assert!(build_program(&r).is_none());
        for rel in ["Update", "Hotfix", "Security Update"] {
            let mut r = raw("A");
            r.release_type = Some(rel.into());
            assert!(build_program(&r).is_none(), "{rel}");
        }
        let mut r = raw("A");
        r.release_type = Some("Application".into());
        assert!(build_program(&r).is_some());
    }

    #[test]
    fn build_program_fields() {
        let mut r = raw("App");
        r.install_location = Some(r#" "C:\Apps\App" "#.into());
        r.install_date = Some("20240305".into());
        r.estimated_size_kb = Some(2);
        r.publisher = Some("Acme".into());
        let p = build_program(&r).unwrap();
        assert_eq!(p.location, Some(PathBuf::from(r"C:\Apps\App")));
        assert_eq!(p.installed.as_deref(), Some("05/03/2024"));
        assert_eq!(p.size, Some(2048));
        assert_eq!(p.source, ProgramSource::Registry);
        r.estimated_size_kb = Some(0);
        assert_eq!(build_program(&r).unwrap().size, None, "0 KB = desconhecido");
        assert_eq!(p.publisher, "Acme");
        assert_eq!(format_install_date("2024"), None);
        assert_eq!(format_install_date("20241399"), None);
    }

    #[test]
    #[ignore = "lê o registro desta máquina"]
    fn installed_programs_smoke() {
        let v = installed_programs();
        assert!(!v.is_empty());
    }
}
