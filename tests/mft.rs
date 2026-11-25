//! Varredura pela MFT contra a varredura de pastas no C: de verdade.
//!
//! O teste comparativo precisa de terminal elevado (administrador):
//! `cargo test --release --test mft -- --ignored --nocapture`

use deixa_seco::elevation::is_elevated;
use deixa_seco::scan::mft::mft_scan;
use deixa_seco::scan::walk::walk_scan;
use deixa_seco::tree::Tree;
use std::path::Path;
use std::time::Instant;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::GetCurrentProcess;

fn size_at(tree: &Tree, path: &str) -> u64 {
    let id = tree.find(Path::new(path)).expect(path);
    tree.node(id).size
}

/// Pico de working set do processo, em bytes (0 se a consulta falhar).
fn peak_working_set() -> u64 {
    let mut pmc = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: `pmc` é válido e `cb` informa seu tamanho.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) };
    if ok.is_ok() {
        pmc.PeakWorkingSetSize as u64
    } else {
        0
    }
}

fn within(a: u64, b: u64, tolerance: f64) -> bool {
    (a as f64 - b as f64).abs() <= tolerance * b as f64
}

#[test]
fn mft_scan_without_admin_is_error() {
    if is_elevated() {
        eprintln!("terminal elevado: teste não se aplica");
        return;
    }
    assert!(mft_scan('C', &mut |_, _| {}).is_err());
}

#[test]
#[ignore = "precisa de administrador"]
fn mft_matches_walk_on_c() {
    assert!(is_elevated(), "rode num terminal de administrador");
    let t = Instant::now();
    let mft = mft_scan('C', &mut |_, _| {}).expect("varredura pela MFT");
    let mft_time = t.elapsed();
    // Medido antes da varredura de pastas, para o pico ser só da MFT (meta: ~1 GB).
    let mft_peak = peak_working_set();
    println!(
        "pico de working set após a MFT: {} MB",
        mft_peak / (1024 * 1024)
    );
    let t = Instant::now();
    let walk = walk_scan(Path::new("C:\\"), &mut |_, _| {}).expect("varredura de pastas");
    let walk_time = t.elapsed();

    let (m, w) = (mft.node(mft.root()), walk.node(walk.root()));
    let (mw, ww) = (size_at(&mft, r"C:\Windows"), size_at(&walk, r"C:\Windows"));
    println!(
        "MFT : {:>8.2?}  {:>16} bytes  {:>9} arquivos  {:>9} nós",
        mft_time,
        m.size,
        m.files,
        mft.len()
    );
    println!(
        "walk: {:>8.2?}  {:>16} bytes  {:>9} arquivos  {:>9} nós",
        walk_time,
        w.size,
        w.files,
        walk.len()
    );
    println!(r"C:\Windows  MFT {mw}  walk {ww}");

    assert!(
        within(m.size, w.size, 0.05),
        "total: MFT {} vs walk {}",
        m.size,
        w.size
    );
    assert!(within(mw, ww, 0.10), r"C:\Windows: MFT {mw} vs walk {ww}");
}
