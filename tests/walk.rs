use deixa_seco::scan::walk::{cluster_size, walk_scan};
use deixa_seco::scan::{ScanEvent, ScanMethod, start_scan};
use deixa_seco::volumes::list_volumes;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

fn find(t: &deixa_seco::tree::Tree, p: &std::path::Path) -> deixa_seco::tree::NodeId {
    t.find(p)
        .unwrap_or_else(|| panic!("nó não encontrado: {}", p.display()))
}

#[test]
fn walk_counts_files_and_sizes() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("a")).unwrap();
    fs::create_dir(dir.path().join("b")).unwrap();
    fs::write(dir.path().join("a").join("x.bin"), vec![1u8; 5000]).unwrap();
    fs::write(dir.path().join("b").join("y.bin"), vec![1u8; 10]).unwrap();

    let t = walk_scan(dir.path(), &mut |_, _| {}).unwrap();
    assert_eq!(t.node(t.root()).files, 2);
    let a = find(&t, &dir.path().join("a"));
    let cluster = cluster_size(dir.path());
    assert!(cluster > 0);
    assert!(t.node(a).size >= 5000);
    assert_eq!(t.node(a).size % cluster, 0);
}

#[test]
fn walk_does_not_follow_junction() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("big.bin"), vec![7u8; 1_000_000]).unwrap();
    let link = dir.path().join("link");
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&real)
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "mklink /J falhou");

    let t = walk_scan(dir.path(), &mut |_, _| {}).unwrap();
    assert!(t.node(t.root()).size < 2_000_000);
    let l = find(&t, &link);
    assert_eq!(t.node(l).size, 0);
    assert!(t.children(l).is_empty());
    // A junção não é um arquivo: só big.bin conta.
    assert_eq!(t.node(t.root()).files, 1);
}

#[test]
fn start_scan_emits_done() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), b"oi").unwrap();
    let rx = start_scan(dir.path().to_path_buf(), false);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("timeout esperando Done") {
            ScanEvent::Progress { .. } | ScanEvent::Preview { .. } => continue,
            ScanEvent::Done(tree, method) => {
                assert_eq!(method, ScanMethod::Walk);
                assert_eq!(tree.node(tree.root()).files, 1);
                break;
            }
            ScanEvent::Failed(e) => panic!("falhou: {e}"),
        }
    }
}

#[test]
fn walk_missing_root_is_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(walk_scan(&dir.path().join("nao-existe"), &mut |_, _| {}).is_err());
}

#[test]
fn list_volumes_has_c() {
    let v = list_volumes();
    let c = v.iter().find(|v| v.letter == 'C').expect("volume C:");
    assert!(c.total > c.free);
    assert!(!c.fs.is_empty());
}

#[test]
fn walk_cancel_flag_stops_early() {
    use std::sync::atomic::AtomicBool;
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("f.txt"), b"x").unwrap();
    let cancel = AtomicBool::new(true);
    let err = deixa_seco::scan::walk::walk_scan_cancellable(dir.path(), &mut |_, _| {}, &cancel)
        .err()
        .expect("deveria cancelar");
    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
}

#[test]
fn walk_file_symlink_is_zero_and_normal_file_counts() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.bin");
    fs::write(&target, vec![3u8; 100_000]).unwrap();
    let link = dir.path().join("flink.bin");
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
        eprintln!("sem privilégio para criar symlink; teste ignorado");
        return;
    }
    let t = walk_scan(dir.path(), &mut |_, _| {}).unwrap();
    let l = find(&t, &link);
    assert_eq!(t.node(l).size, 0);
    let n = find(&t, &target);
    let cluster = cluster_size(dir.path());
    assert!(t.node(n).size >= 100_000);
    assert_eq!(t.node(n).size % cluster, 0);
    assert_eq!(t.node(t.root()).size, t.node(n).size);
}
