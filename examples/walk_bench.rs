//! Mede a varredura de pastas: `cargo run --release --example walk_bench -- C:\`
//!
//! Com `--preview` ou `--no-preview` a varredura roda como no app (thread própria e eventos,
//! `start_scan_with`), com ou sem o mapa parcial; sem flag, chama `walk_scan` direto.

use deixa_seco::format::{format_bytes, format_count};
use deixa_seco::scan::walk::walk_scan;
use deixa_seco::scan::{PREVIEW_DEPTH, ScanEvent, start_scan_with};
use deixa_seco::tree::Tree;
use std::mem::size_of;
use std::path::PathBuf;
use std::time::Instant;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::GetCurrentProcess;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args
        .iter()
        .find(|a| a.starts_with("--"))
        .map(String::as_str);
    let root = PathBuf::from(
        args.iter()
            .find(|a| !a.starts_with("--"))
            .cloned()
            .unwrap_or_else(|| "C:\\".into()),
    );
    let start = Instant::now();
    let (tree, previews) = match mode {
        None => (
            walk_scan(&root, &mut |_, _| {}).expect("varredura falhou"),
            0,
        ),
        Some(flag @ ("--preview" | "--no-preview")) => {
            scan_with_events(root.clone(), flag == "--preview")
        }
        Some(other) => panic!("flag desconhecida: {other} (use --preview ou --no-preview)"),
    };
    let elapsed = start.elapsed();
    let r = tree.node(tree.root());
    println!("raiz:     {}", root.display());
    println!("modo:     {}", mode.unwrap_or("walk_scan direto"));
    println!("tempo:    {elapsed:.2?}");
    println!("tamanho:  {} ({} bytes)", format_bytes(r.size), r.size);
    println!("arquivos: {}", format_count(r.files));
    println!("nos:      {}", format_count(tree.len() as u64));
    if mode == Some("--preview") {
        println!("previews: {previews}");
        // Pior caso: preview da árvore inteira (o último durante a varredura é menor).
        let t0 = Instant::now();
        let (p, _) = tree.preview_with_kinds(PREVIEW_DEPTH);
        println!(
            "preview da árvore final: {:.2?} ({} nós)",
            t0.elapsed(),
            format_count(p.len() as u64)
        );
    }

    let mut pmc = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: `pmc` é válido e `cb` informa seu tamanho.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) };
    if ok.is_ok() {
        println!(
            "working set de pico:  {}",
            format_bytes(pmc.PeakWorkingSetSize as u64)
        );
        println!(
            "working set atual:    {}",
            format_bytes(pmc.WorkingSetSize as u64)
        );
    }
}

/// Varre pela thread do app e consome os eventos até o `Done`; devolve a árvore e quantos
/// `Preview` chegaram.
fn scan_with_events(root: PathBuf, previews: bool) -> (Tree, u32) {
    let rx = start_scan_with(root, false, previews);
    let mut count = 0;
    for ev in rx.iter() {
        match ev {
            ScanEvent::Progress { .. } => {}
            ScanEvent::Preview { .. } => count += 1,
            ScanEvent::Done(tree, _) => return (tree, count),
            ScanEvent::Failed(e) => panic!("varredura falhou: {e}"),
        }
    }
    panic!("a varredura terminou sem Done");
}
