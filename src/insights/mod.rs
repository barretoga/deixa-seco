//! Sugestões de limpeza: categorias conhecidas (caminho fixo) calculadas sobre a `Tree`.
//! O núcleo é puro: `analyze` não toca o disco; os fatos do sistema vêm em `SysFacts`.

pub mod dev;
pub mod known;
pub mod programs;
pub mod store;
pub mod virt;

pub use programs::{Program, ProgramSource};
pub use virt::{AvdConfig, VirtDisk, WslDisk};

use crate::tree::{NodeId, Tree};
use std::path::PathBuf;
use std::time::SystemTime;

/// Ordem de declaração = ordem de exibição.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Risk {
    Seguro,
    Revisar,
    Informativo,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Apaga de vez o conteúdo, mantém a pasta.
    DeleteContents,
    Recycle,
    EmptyRecycleBin,
    Uninstall {
        command: String,
    },
    /// Compacta os discos virtuais (`.vhdx`) dos nós; precisa de admin.
    CompactVhdx,
    /// `docker system prune -a -f` (sem volumes).
    DockerPrune,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Insight {
    pub id: &'static str,
    pub title: String,
    pub explanation: String,
    pub risk: Risk,
    pub size: u64,
    pub nodes: Vec<NodeId>,
    pub action: Action,
    pub needs_admin: bool,
    pub hint: Option<String>,
    /// Só na categoria `programs`: um item por programa (o desinstalador é por programa).
    pub programs: Vec<Program>,
    /// Só em `vhdx_compact`: um item por disco (cada um compacta separado).
    pub disks: Vec<VirtDisk>,
    /// (item, acompanhante): nós que saem junto com um item e não aparecem sozinhos na lista
    /// (o `.ini` de um AVD sai com a pasta `.avd`). Os dois estão em `nodes`.
    pub companions: Vec<(NodeId, NodeId)>,
}

#[derive(Clone, Debug)]
pub struct Env {
    pub user_profile: PathBuf,
    pub local_app_data: PathBuf,
    pub app_data: PathBuf,
    pub windows: PathBuf,
    pub system_drive: PathBuf,
}

impl Env {
    pub fn from_system() -> Env {
        Self::from_lookup(|k| std::env::var_os(k))
    }

    /// Valores vazios ou relativos são ignorados (cai no caminho derivado).
    fn from_lookup(get: impl Fn(&str) -> Option<std::ffi::OsString>) -> Env {
        let var = |k: &str| get(k).map(PathBuf::from).filter(|p| p.is_absolute());
        let system_drive = get("SystemDrive")
            .map(|d| {
                let s = d.to_string_lossy().trim_end_matches('\\').to_string();
                PathBuf::from(format!("{s}\\"))
            })
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from("C:\\"));
        let user_profile = var("USERPROFILE").unwrap_or_else(|| system_drive.join("Users"));
        let local_app_data =
            var("LOCALAPPDATA").unwrap_or_else(|| user_profile.join("AppData\\Local"));
        let app_data = var("APPDATA").unwrap_or_else(|| user_profile.join("AppData\\Roaming"));
        let windows = var("WINDIR")
            .or_else(|| var("SystemRoot"))
            .unwrap_or_else(|| system_drive.join("Windows"));
        let env = Env {
            user_profile,
            local_app_data,
            app_data,
            windows,
            system_drive,
        };
        // Só em debug: perfil falso para testes manuais (DEIXA_SECO_PROFILE).
        #[cfg(debug_assertions)]
        let env = match var("DEIXA_SECO_PROFILE") {
            Some(p) => Env {
                local_app_data: p.join("AppData\\Local"),
                app_data: p.join("AppData\\Roaming"),
                user_profile: p,
                ..env
            },
            None => env,
        };
        env
    }

    /// Ambiente sintético para testes: perfil `<drive>Users\t`, Windows em `<drive>Windows`.
    pub fn for_test(drive: &str) -> Env {
        let d = PathBuf::from(drive);
        let profile = d.join("Users\\t");
        Env {
            local_app_data: profile.join("AppData\\Local"),
            app_data: profile.join("AppData\\Roaming"),
            user_profile: profile,
            windows: d.join("Windows"),
            system_drive: d,
        }
    }
}

/// Fatos do sistema que não vêm da árvore; injetáveis para testes.
#[derive(Clone, Debug, Default)]
pub struct SysFacts {
    pub recycle_bin_bytes: u64,
    /// Distros do WSL (registro `HKCU\...\Lxss`).
    pub wsl_disks: Vec<WslDisk>,
    /// Disco de dados do Docker Desktop, se existir.
    pub docker_vhdx: Option<PathBuf>,
    /// Soma do `Reclaimable` de `docker system df`; `None` = Docker ausente ou parado.
    pub docker_reclaimable: Option<u64>,
    /// AVDs conhecidos pelo emulador Android (com as imagens de sistema que usam).
    pub avds: Vec<AvdConfig>,
}

impl SysFacts {
    /// Lê Lixeira, registro do WSL, `docker system df` (até 5 s) e os `.ini` dos AVDs.
    /// Chame só fora da thread da UI.
    pub fn gather() -> SysFacts {
        SysFacts {
            recycle_bin_bytes: known::recycle_bin_size().unwrap_or(0),
            wsl_disks: virt::gather_wsl_disks(),
            docker_vhdx: virt::gather_docker_vhdx(&Env::from_system().local_app_data),
            docker_reclaimable: virt::gather_docker_reclaimable(),
            avds: virt::gather_avds(),
        }
    }
}

/// Categorias com algo a mostrar (em geral `size > 0`; a do Docker parado é Informativa com
/// 0 B), ordenadas: Seguro, Revisar, Informativo; dentro, por tamanho desc.
pub fn analyze(tree: &Tree, env: &Env, facts: &SysFacts, now: SystemTime) -> Vec<Insight> {
    let mut out = known::analyze_known(tree, env, facts, now);
    out.extend(dev::stale_artifacts(tree, env, now));
    out.extend(dev::old_downloads(tree, env, now));
    out.extend(virt::virt_insights(tree, env, facts, now));
    drop_covered_vms(tree, &mut out);
    out.sort_by(|a, b| {
        a.risk
            .cmp(&b.risk)
            .then(b.size.cmp(&a.size))
            .then(a.id.cmp(b.id))
    });
    out
}

/// `stale_vms` é a sugestão mais genérica (qualquer `.iso`/`.img` grande e velho): tira dela o
/// que já está em outra sugestão (um `.iso` em Downloads antigos, um `.img` dentro do Temp),
/// senão o total conta o mesmo arquivo duas vezes.
fn drop_covered_vms(tree: &Tree, list: &mut Vec<Insight>) {
    const GENERIC: &str = "stale_vms";
    let others: std::collections::HashSet<NodeId> = list
        .iter()
        .filter(|i| i.id != GENERIC)
        .flat_map(|i| i.nodes.iter().copied())
        .collect();
    if others.is_empty() {
        return;
    }
    list.retain_mut(|i| {
        if i.id != GENERIC {
            return true;
        }
        i.nodes
            .retain(|&n| !tree.ancestors(n).iter().any(|a| others.contains(a)));
        i.size = i.nodes.iter().map(|&n| tree.node(n).size).sum();
        i.size > 0
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn lookup(m: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let m: HashMap<String, OsString> = m
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn iso_in_old_downloads_is_not_counted_twice() {
        const DAY: std::time::Duration = std::time::Duration::from_secs(86_400);
        let now = SystemTime::UNIX_EPOCH + DAY * 1000;
        let mut t = Tree::new("C:\\".into());
        let r = t.root();
        let users = t.add(r, "Users", true, 0, None);
        let me = t.add(users, "t", true, 0, None);
        let dl = t.add(me, "Downloads", true, 0, None);
        let iso = t.add(dl, "ubuntu.iso", false, 600 << 20, Some(now - DAY * 200));
        let vms = t.add(r, "VMs", true, 0, None);
        let vdi = t.add(vms, "win.vdi", false, 700 << 20, Some(now - DAY * 200));
        t.finalize();
        let v = analyze(&t, &Env::for_test("C:\\"), &SysFacts::default(), now);
        let dl = v
            .iter()
            .find(|i| i.id == "old_downloads")
            .expect("old_downloads");
        assert_eq!(dl.nodes, vec![iso]);
        let vm = v.iter().find(|i| i.id == "stale_vms").expect("stale_vms");
        assert_eq!(vm.nodes, vec![vdi], "o .iso fica só em Downloads antigos");
        assert_eq!(vm.size, 700 << 20);
        let total: u64 = v.iter().map(|i| i.size).sum();
        assert_eq!(total, (600 << 20) + (700 << 20));
    }

    #[test]
    fn from_lookup_ignores_empty_and_relative() {
        let env = Env::from_lookup(lookup(&[
            ("SystemDrive", ""),
            ("USERPROFILE", ""),
            ("LOCALAPPDATA", r"relativo\x"),
            ("APPDATA", ""),
            ("WINDIR", "Windows"),
        ]));
        assert_eq!(env.system_drive, PathBuf::from(r"C:\"));
        assert_eq!(env.user_profile, PathBuf::from(r"C:\Users"));
        assert_eq!(env.local_app_data, PathBuf::from(r"C:\Users\AppData\Local"));
        assert_eq!(env.app_data, PathBuf::from(r"C:\Users\AppData\Roaming"));
        assert_eq!(env.windows, PathBuf::from(r"C:\Windows"));
    }

    #[test]
    fn from_lookup_uses_valid_values() {
        let env = Env::from_lookup(lookup(&[
            ("SystemDrive", "D:"),
            ("USERPROFILE", r"D:\Users\x"),
            ("WINDIR", r"D:\WIN"),
        ]));
        assert_eq!(env.system_drive, PathBuf::from(r"D:\"));
        assert_eq!(env.user_profile, PathBuf::from(r"D:\Users\x"));
        assert_eq!(env.windows, PathBuf::from(r"D:\WIN"));
    }
}
