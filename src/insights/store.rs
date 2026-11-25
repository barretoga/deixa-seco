//! Apps da Microsoft Store do usuário atual (WinRT `PackageManager`).
//!
//! Só `store_apps` (leitura) e `remove_package` (remoção, chamada só pela execução de um plano
//! confirmado) tocam o sistema; filtro, conversão e junção com o registro são puros.
//!
//! Filtro (o que nunca aparece): framework, pacote de recurso, assinado como sistema
//! (`SignatureKind::System`: apps embutidos do Windows), pacotes sem nenhum app na lista de
//! apps (runtimes como `WinAppRuntime.Main`, extensões de shell, bundles sem app) e os pacotes
//! de `NON_REMOVABLE`. O crate `windows` não expõe o `NonRemovable` do `Get-AppxPackage`; nesta
//! máquina todos os não removíveis são `System`, menos os dois primeiros de `NON_REMOVABLE`.

use super::programs::{Program, ProgramSource};
use crate::format::format_date;
use crate::scan::walk::walk_scan_serial;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};
use windows::ApplicationModel::{Package, PackageSignatureKind};
use windows::Management::Deployment::PackageManager;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::HSTRING;

/// `Package.Id.Name` de pacotes que não saem (ou não devem sair) por aqui, sem diferenciar
/// maiúsculas. Os dois primeiros têm `NonRemovable = True` sem serem `System`; a própria Store
/// fica de fora porque sem ela não dá para reinstalar nada.
pub const NON_REMOVABLE: &[&str] = &[
    "Microsoft.DesktopAppInstaller",
    "Microsoft.SecHealthUI",
    "Microsoft.WindowsStore",
    "Microsoft.StorePurchaseApp",
];

/// O que se lê de um `Package`, já em tipos Rust (testável sem WinRT).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageFacts {
    pub id_name: String,
    pub full_name: String,
    pub display_name: String,
    pub publisher: String,
    pub location: Option<PathBuf>,
    pub installed: Option<String>,
    pub is_framework: bool,
    pub is_resource: bool,
    pub is_bundle: bool,
    pub system_signed: bool,
    /// Tem pelo menos um app na lista de apps (`GetAppListEntries`).
    pub has_apps: bool,
}

/// `true` = o pacote aparece na lista e pode ser removido.
pub fn keep_package(f: &PackageFacts) -> bool {
    let non_removable = NON_REMOVABLE
        .iter()
        .any(|n| n.eq_ignore_ascii_case(&f.id_name));
    // `is_bundle` sem app cai em `!has_apps`; bundle com app é um app normal.
    !(f.is_framework || f.is_resource || f.system_signed || !f.has_apps || non_removable)
}

/// O pacote como `Program` da Store. O tamanho fica desconhecido: vem da árvore depois
/// (`programs_insight`), se a pasta de instalação estiver nela e tiver sido lida.
pub fn package_program(f: PackageFacts) -> Program {
    let display = f.display_name.trim();
    let name = if display.is_empty() || display.starts_with("ms-resource:") {
        f.id_name.clone()
    } else {
        display.to_string()
    };
    Program {
        name,
        publisher: f.publisher.trim().to_string(),
        size: None,
        installed: f.installed,
        uninstall: String::new(),
        location: f.location,
        source: ProgramSource::Store {
            full_name: f.full_name,
        },
    }
}

/// Junta registro e Store. Registro × Store: pelo nome (sem diferenciar maiúsculas), fica a
/// entrada do registro (tem o desinstalador), completada com o tamanho e a pasta do app da
/// Store se faltarem. Store × Store: pelo `full_name` (dois pacotes diferentes com o mesmo nome
/// aparecem os dois). Ordenado por nome.
pub fn merge_programs(registry: Vec<Program>, store: Vec<Program>) -> Vec<Program> {
    let mut out = registry;
    let by_name: HashMap<String, usize> = out
        .iter()
        .enumerate()
        .filter(|(_, p)| p.source == ProgramSource::Registry)
        .map(|(i, p)| (p.name.to_lowercase(), i))
        .collect();
    let mut full_names: HashSet<String> = HashSet::new();
    for s in store {
        let ProgramSource::Store { full_name } = &s.source else {
            continue;
        };
        if !full_names.insert(full_name.to_lowercase()) {
            continue;
        }
        match by_name.get(&s.name.to_lowercase()) {
            Some(&i) => {
                let p = &mut out[i];
                if p.size.is_none() {
                    p.size = s.size;
                }
                if p.location.is_none() {
                    p.location = s.location;
                }
            }
            None => out.push(s),
        }
    }
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// Tempo máximo medindo um pacote.
pub const SIZE_PER_PACKAGE: Duration = Duration::from_secs(2);
/// Tempo máximo medindo todos os pacotes.
pub const SIZE_TOTAL: Duration = Duration::from_secs(20);

/// Tamanho alocado da pasta de um pacote, só leitura (a varredura de pastas: não segue reparse
/// points; numa thread só, sem o pool global do rayon, que a varredura principal pode estar
/// ocupando). Listar `WindowsApps` é negado sem admin, mas a pasta de cada pacote costuma ser
/// legível. `None` se não deu para ler, se passou de `limit`, ou se deu 0 B (ilegível também
/// aparece como pasta vazia: nunca mostrar 0 B).
pub fn package_size(path: &Path, limit: Duration) -> Option<u64> {
    let start = Instant::now();
    let cancel = AtomicBool::new(false);
    let tree = walk_scan_serial(
        path,
        &mut |_, _| {
            if start.elapsed() > limit {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        &cancel,
    )
    .ok()?;
    if start.elapsed() > limit {
        return None;
    }
    Some(tree.node(tree.root()).size).filter(|&s| s > 0)
}

/// Mede os apps da Store com tamanho desconhecido e pasta conhecida, com `sizer(pasta, limite)`,
/// até `total` de tempo ao todo. Devolve quantos ganharam tamanho.
fn fill_sizes_with(
    programs: &mut [Program],
    total: Duration,
    mut sizer: impl FnMut(&Path, Duration) -> Option<u64>,
) -> usize {
    let start = Instant::now();
    let mut n = 0;
    for p in programs
        .iter_mut()
        .filter(|p| p.is_store() && p.size.is_none())
    {
        let left = total.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        let Some(loc) = p.location.clone() else {
            continue;
        };
        if let Some(s) = sizer(&loc, left.min(SIZE_PER_PACKAGE)) {
            p.size = Some(s);
            n += 1;
        }
    }
    n
}

/// Mede os apps da Store pela pasta de cada um (chame fora da thread da UI; até `SIZE_TOTAL`).
pub fn fill_store_sizes(programs: &mut [Program]) -> usize {
    fill_sizes_with(programs, SIZE_TOTAL, package_size)
}

/// `DateTime.UniversalTime` (ticks de 100 ns desde 1601, UTC) como `dd/mm/aaaa` local.
fn ticks_to_date(ticks: i64) -> Option<String> {
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600; // 1601 -> 1970
    let secs = (ticks / 10_000_000).checked_sub(EPOCH_DIFF_SECS)?;
    if ticks <= 0 || secs <= 0 {
        return None;
    }
    Some(format_date(
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64),
    ))
}

/// Motivo em pt-BR de uma remoção que falhou (HRESULT do `PackageManager`).
pub fn removal_reason(code: i32) -> String {
    match code as u32 {
        0x8007_3D02 => "o app está aberto; feche-o e tente de novo".into(),
        0x8007_3CFA => "o Windows não deixou remover (pode fazer parte do sistema)".into(),
        0x8007_0005 => "sem permissão".into(),
        0x8007_3CF1 => "o app não está instalado (já tinha saído?)".into(),
        c => format!("o Windows recusou a remoção (código 0x{c:08X})"),
    }
}

/// WinRT nesta thread (MTA) enquanto o guarda viver.
struct RoGuard(bool);

impl RoGuard {
    fn new() -> RoGuard {
        // SAFETY: inicializa o WinRT só nesta thread; desfeito no `Drop` se deu certo. Se a
        // thread já tem outro modo (RPC_E_CHANGED_MODE), segue com o que ela tem.
        RoGuard(unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for RoGuard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: par do `RoInitialize` que deu certo nesta mesma thread.
            unsafe { RoUninitialize() };
        }
    }
}

/// Lê um pacote. Erro ao ler uma das marcas do filtro = `None` (fica de fora: na dúvida, não
/// oferece). A lista de apps só é lida se as outras marcas já deixam o pacote entrar.
fn read_facts(pkg: &Package) -> Option<PackageFacts> {
    let id = pkg.Id().ok()?;
    let is_framework = pkg.IsFramework().ok()?;
    let is_resource = pkg.IsResourcePackage().ok()?;
    let is_bundle = pkg.IsBundle().ok()?;
    let system_signed = pkg.SignatureKind().ok()? == PackageSignatureKind::System;
    let has_apps = !(is_framework || is_resource || system_signed)
        && pkg
            .GetAppListEntries()
            .and_then(|v| v.Size())
            .is_ok_and(|n| n > 0);
    let text = |r: windows::core::Result<HSTRING>| r.map(|h| h.to_string()).unwrap_or_default();
    let location = text(pkg.InstalledPath());
    Some(PackageFacts {
        id_name: id.Name().ok()?.to_string(),
        full_name: id.FullName().ok()?.to_string(),
        display_name: text(pkg.DisplayName()),
        publisher: text(pkg.PublisherDisplayName()),
        location: (!location.trim().is_empty()).then(|| PathBuf::from(location.trim())),
        installed: pkg
            .InstalledDate()
            .ok()
            .and_then(|d| ticks_to_date(d.UniversalTime)),
        is_framework,
        is_resource,
        is_bundle,
        system_signed,
        has_apps,
    })
}

/// Apps da Store do usuário atual que podem ser removidos, sem duplicatas pelo nome, por nome.
/// Pode levar alguns segundos: chame fora da thread da UI. Falha = lista vazia.
pub fn store_apps() -> Vec<Program> {
    let _ro = RoGuard::new();
    let Ok(pm) = PackageManager::new() else {
        return Vec::new();
    };
    // SID vazio = usuário atual (não precisa de admin).
    let Ok(pkgs) = pm.FindPackagesByUserSecurityId(&HSTRING::new()) else {
        return Vec::new();
    };
    let apps = pkgs
        .into_iter()
        .filter_map(|p| read_facts(&p))
        .filter(keep_package)
        .map(package_program)
        .collect();
    merge_programs(Vec::new(), apps)
}

/// Remove o pacote do usuário atual e espera o fim (chame fora da thread da UI). Só a execução
/// de um plano confirmado chama isto.
pub fn remove_package(full_name: &str) -> Result<(), String> {
    let _ro = RoGuard::new();
    let pm = PackageManager::new().map_err(|e| removal_reason(e.code().0))?;
    let op = pm
        .RemovePackageAsync(&HSTRING::from(full_name))
        .map_err(|e| removal_reason(e.code().0))?;
    let result = op.join().map_err(|e| removal_reason(e.code().0))?;
    removal_outcome(
        result
            .ExtendedErrorCode()
            .map(|h| h.0)
            .map_err(|e| e.code().0),
    )
}

/// Resultado da remoção a partir do `ExtendedErrorCode`: só sucesso legível conta como
/// removido; código ilegível vira item pulado.
fn removal_outcome(code: Result<i32, i32>) -> Result<(), String> {
    match code {
        Ok(c) if c >= 0 => Ok(()),
        Ok(c) => Err(removal_reason(c)),
        Err(c) => Err(format!(
            "o Windows não informou se removeu (código 0x{:08X}); reanalise para conferir",
            c as u32
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insights::programs::ProgramSource;
    use std::path::PathBuf;

    fn facts(name: &str) -> PackageFacts {
        PackageFacts {
            id_name: format!("Acme.{name}"),
            full_name: format!("Acme.{name}_1.0.0.0_x64__abc"),
            display_name: name.into(),
            publisher: "Acme".into(),
            location: Some(PathBuf::from(format!(
                r"C:\Program Files\WindowsApps\Acme.{name}_1.0.0.0_x64__abc"
            ))),
            installed: Some("05/03/2024".into()),
            is_framework: false,
            is_resource: false,
            is_bundle: false,
            system_signed: false,
            has_apps: true,
        }
    }

    #[test]
    fn filter_drops_framework_system_resource_and_non_removable() {
        assert!(keep_package(&facts("App")));
        let mut f = facts("A");
        f.is_framework = true;
        assert!(!keep_package(&f), "framework");
        let mut f = facts("A");
        f.is_resource = true;
        assert!(!keep_package(&f), "recurso");
        let mut f = facts("A");
        f.system_signed = true;
        assert!(!keep_package(&f), "assinado como sistema");
        let mut f = facts("A");
        f.is_bundle = true;
        f.has_apps = false;
        assert!(!keep_package(&f), "bundle sem app");
        let mut f = facts("A");
        f.has_apps = false;
        assert!(!keep_package(&f), "sem app nenhum (runtime, extensão)");
        for n in NON_REMOVABLE {
            let mut f = facts("A");
            f.id_name = n.to_string();
            assert!(!keep_package(&f), "{n} não é removível");
            f.id_name = n.to_lowercase();
            assert!(!keep_package(&f), "{n} em minúsculas");
        }
    }

    #[test]
    fn package_becomes_store_program_with_unknown_size() {
        let p = package_program(facts("Bloco"));
        assert_eq!(p.name, "Bloco");
        assert_eq!(p.publisher, "Acme");
        assert_eq!(p.size, None, "tamanho vem da árvore depois");
        assert_eq!(p.uninstall, "");
        assert_eq!(p.installed.as_deref(), Some("05/03/2024"));
        assert_eq!(
            p.source,
            ProgramSource::Store {
                full_name: "Acme.Bloco_1.0.0.0_x64__abc".into()
            }
        );
        // Nome vazio ou recurso não resolvido: cai no nome do pacote.
        let mut f = facts("x");
        f.display_name = "ms-resource:AppName".into();
        assert_eq!(package_program(f).name, "Acme.x");
        let mut f = facts("x");
        f.display_name = "  ".into();
        assert_eq!(package_program(f).name, "Acme.x");
    }

    fn reg(name: &str, size: Option<u64>) -> Program {
        Program {
            name: name.into(),
            publisher: String::new(),
            size,
            installed: None,
            uninstall: "u.exe".into(),
            location: None,
            source: ProgramSource::Registry,
        }
    }

    #[test]
    fn merge_keeps_registry_entry_for_same_name() {
        let mut st = package_program(facts("Spotify"));
        st.size = Some(500);
        let other = package_program(facts("Calculadora"));
        let dup = package_program(facts("Calculadora"));
        let merged = merge_programs(
            vec![reg("spotify", None), reg("Zoom", Some(7))],
            vec![st, other, dup],
        );
        let names: Vec<_> = merged.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Calculadora", "spotify", "Zoom"], "sem duplicar");
        let sp = merged.iter().find(|p| p.name == "spotify").unwrap();
        assert_eq!(sp.source, ProgramSource::Registry, "fica o do registro");
        assert_eq!(sp.uninstall, "u.exe");
        assert_eq!(
            sp.size,
            Some(500),
            "sem tamanho no registro: usa o da Store"
        );
        let merged = merge_programs(
            vec![reg("Spotify", Some(9))],
            vec![{
                let mut s = package_program(facts("Spotify"));
                s.size = Some(500);
                s
            }],
        );
        assert_eq!(merged[0].size, Some(9), "o do registro vale se existe");
    }

    #[test]
    fn store_duplicates_are_by_full_name() {
        let a = package_program(facts("Bloco"));
        let same = package_program(facts("Bloco"));
        let mut other = package_program(facts("Bloco"));
        other.source = ProgramSource::Store {
            full_name: "Outro.Bloco_2_x64__xyz".into(),
        };
        let merged = merge_programs(Vec::new(), vec![a, same, other]);
        assert_eq!(merged.len(), 2, "mesmo full_name some; outro pacote fica");
    }

    #[test]
    fn removal_outcome_needs_a_readable_success() {
        assert_eq!(removal_outcome(Ok(0)), Ok(()));
        assert!(
            removal_outcome(Ok(0x80073D02_u32 as i32))
                .unwrap_err()
                .contains("aberto")
        );
        let unread = removal_outcome(Err(0x8000_FFFF_u32 as i32)).unwrap_err();
        assert!(unread.contains("não informou"), "{unread}");
    }

    fn sized(name: &str, size: Option<u64>, store: bool) -> Program {
        let mut p = package_program(facts(name));
        p.size = size;
        if !store {
            p.source = ProgramSource::Registry;
        }
        p
    }

    #[test]
    fn fill_sizes_only_unknown_store_apps_within_budget() {
        let mut v = vec![
            sized("A", None, true),
            sized("B", Some(7), true),
            sized("C", None, false),
            sized("D", None, true),
        ];
        let mut asked = Vec::new();
        let n = fill_sizes_with(&mut v, Duration::from_secs(60), |p, _| {
            asked.push(p.to_path_buf());
            if p.to_string_lossy().contains("Acme.D_") {
                None
            } else {
                Some(100)
            }
        });
        assert_eq!(n, 1);
        assert_eq!(asked.len(), 2, "só Store com tamanho desconhecido");
        assert_eq!(v[0].size, Some(100));
        assert_eq!(v[1].size, Some(7));
        assert_eq!(v[2].size, None, "registro não é medido aqui");
        assert_eq!(v[3].size, None, "falhou: continua desconhecido");
        // Orçamento total esgotado: não mede mais nada.
        let mut w = vec![sized("A", None, true)];
        assert_eq!(fill_sizes_with(&mut w, Duration::ZERO, |_, _| Some(1)), 0);
        assert_eq!(w[0].size, None);
    }

    #[test]
    fn package_size_reads_folder_and_zero_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("App de São Paulo_1.0_x64__abc");
        std::fs::create_dir_all(app.join("sub")).unwrap();
        std::fs::write(app.join("sub").join("a.bin"), vec![1u8; 10_000]).unwrap();
        let s = package_size(&app, Duration::from_secs(10)).unwrap();
        assert!(s >= 10_000, "{s}");
        let empty = dir.path().join("vazio");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(package_size(&empty, Duration::from_secs(10)), None);
        assert_eq!(
            package_size(&dir.path().join("nao_existe"), Duration::from_secs(10)),
            None
        );
    }

    #[test]
    fn removal_reasons_are_pt_br() {
        assert!(removal_reason(0x80073D02_u32 as i32).contains("aberto"));
        assert!(removal_reason(0x80073CFA_u32 as i32).contains("não deixou"));
        assert_eq!(removal_reason(0x80070005_u32 as i32), "sem permissão");
        assert!(removal_reason(0x80073CF1_u32 as i32).contains("não está instalado"));
        let other = removal_reason(0x8007_0057_u32 as i32);
        assert!(other.contains("0x80070057"), "{other}");
    }

    #[test]
    fn ticks_to_date_converts_from_1601() {
        // 2024-03-05 12:00 UTC em ticks de 100 ns desde 1601.
        let unix = 1_709_640_000_i64;
        let ticks = (unix + 11_644_473_600) * 10_000_000;
        assert_eq!(ticks_to_date(ticks).as_deref(), Some("05/03/2024"));
        assert_eq!(ticks_to_date(0), None, "data vazia");
    }

    #[test]
    #[ignore = "lista os apps da Store desta máquina (só leitura)"]
    fn store_apps_smoke() {
        let start = std::time::Instant::now();
        let mut v = store_apps();
        eprintln!("{} apps em {:?}", v.len(), start.elapsed());
        let start = std::time::Instant::now();
        let n = fill_store_sizes(&mut v);
        eprintln!("{n} com tamanho em {:?}", start.elapsed());
        for p in &v {
            eprintln!(
                "  {} | {} | {:?} | {:?}",
                p.name, p.publisher, p.size, p.location
            );
        }
        assert!(!v.is_empty());
        assert!(v.iter().all(|p| p.is_store() && !p.name.is_empty()));
    }
}
