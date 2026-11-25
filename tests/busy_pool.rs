//! Pool global do rayon ocupado (outra varredura em andamento): a varredura de pastas falha
//! com erro claro (nunca devolve árvore vazia), e a medição de um app da Store, que é serial,
//! não depende do pool. Arquivo próprio: segura o pool global do processo de teste inteiro.

use deixa_seco::insights::store::package_size;
use deixa_seco::scan::walk::walk_scan;
use deixa_seco::scan::{ScanEvent, start_scan_with};
use jwalk::rayon;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Ocupa todas as threads do pool global até `release` virar `true`.
fn hold_pool(release: &Arc<AtomicBool>) {
    let n = rayon::current_num_threads();
    let started = Arc::new(AtomicUsize::new(0));
    for _ in 0..n {
        let (release, started) = (Arc::clone(release), Arc::clone(&started));
        rayon::spawn(move || {
            started.fetch_add(1, Ordering::SeqCst);
            while !release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
    }
    let t = Instant::now();
    while started.load(Ordering::SeqCst) < n {
        assert!(t.elapsed() < Duration::from_secs(10), "pool não ocupou");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn busy_pool_fails_walk_but_not_package_size() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join("Fotos de São Paulo_1.0_x64__abc");
    std::fs::create_dir_all(app.join("Assets")).unwrap();
    std::fs::write(app.join("Assets").join("a.bin"), vec![1u8; 50_000]).unwrap();

    let release = Arc::new(AtomicBool::new(false));
    hold_pool(&release);

    // Medição do app: serial, não espera o pool.
    let started = Instant::now();
    let size = package_size(&app, Duration::from_secs(10));
    assert!(size.is_some_and(|s| s >= 50_000), "{size:?}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "não esperou o pool"
    );

    // Varredura normal com o pool ocupado: erro, não uma árvore vazia.
    let Err(err) = walk_scan(dir.path(), &mut |_, _| {}) else {
        panic!("pool ocupado não pode virar uma árvore (vazia)");
    };
    assert!(err.to_string().contains("ocupad"), "{err}");

    // E a tela de varredura recebe Failed com a mensagem em pt-BR.
    let rx = start_scan_with(dir.path().to_path_buf(), false, false);
    let failed = loop {
        match rx.recv_timeout(Duration::from_secs(30)).expect("evento") {
            ScanEvent::Failed(msg) => break msg,
            ScanEvent::Done(..) => panic!("varredura com pool ocupado não pode terminar ok"),
            _ => {}
        }
    };
    assert!(failed.contains("ocupad"), "{failed}");

    release.store(true, Ordering::SeqCst);
    // Pool livre de novo: a varredura volta a funcionar.
    let tree = walk_scan(dir.path(), &mut |_, _| {}).expect("pool livre");
    assert!(tree.node(tree.root()).size >= 50_000);
}
