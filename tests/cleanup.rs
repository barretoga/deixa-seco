//! Testes da limpeza. Só criam e apagam dentro de `tempfile::tempdir()`.
//! Nada aqui executa Recycle, EmptyRecycleBin ou Uninstall: esses ficam no nível do plano.

use deixa_seco::cleanup::{
    CleanupPlan, Op, ProtectCtx, bin_max_bytes, execute, execute_with, is_protected, plan,
    plan_with, recycle_bin_size,
};
use deixa_seco::insights::Env;
use std::fs::{self, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

/// Contexto sintético: perfil `C:\Users\t`, Windows em `C:\Windows`.
fn ctx(allowed: &[&str]) -> ProtectCtx {
    let allowed: Vec<PathBuf> = allowed.iter().map(|s| p(s)).collect();
    ProtectCtx::from_parts(&allowed, &Env::for_test("C:\\"))
}

fn exec(plan: &CleanupPlan) -> deixa_seco::cleanup::CleanupReport {
    execute(plan, &mut |_, _| {})
}

fn plan_of(ops: Vec<(Op, u64)>) -> CleanupPlan {
    plan(ops, &[], u64::MAX)
}

#[test]
fn protects_windows() {
    assert!(is_protected(Path::new("C:\\Windows\\System32\\x.dll"), &[]));
    assert!(!is_protected(
        Path::new("C:\\Windows\\Temp\\a"),
        &[p("C:\\Windows\\Temp")]
    ));
    assert!(!is_protected(
        Path::new("C:\\Users\\usuario\\Downloads\\a.zip"),
        &[]
    ));
    assert!(is_protected(Path::new("C:\\"), &[]));
}

#[test]
fn protects_system_roots_on_any_drive() {
    let c = ctx(&[]);
    for s in [
        r"D:\",
        r"D:\Windows",
        r"D:\Program Files\App",
        r"E:\Program Files (x86)\App\a.exe",
        r"D:\ProgramData\x",
        r"C:\Windows",
        r"C:\Program Files",
    ] {
        assert!(c.is_protected(Path::new(s)), "{s}");
    }
    assert!(!c.is_protected(Path::new(r"D:\Jogos\x")));
}

#[test]
fn normalizes_case_prefix_dots_and_separators() {
    let c = ctx(&[r"C:\Windows\Temp"]);
    for s in [
        r"c:\WINDOWS\system32",
        r"\\?\C:\Windows\System32",
        r"C:\Windows\Temp\..\System32",
        r"C:\Windows\Temp\..",
        r"C:/Program Files/App",
        r"C:\Windows\",
        r"C:\Windows\.",
        r"C:\Windows.",
        r"C:\Program Files ",
        r"C:\Windows::$INDEX_ALLOCATION",
        r"C:\Users\x\..\..",
    ] {
        assert!(c.is_protected(Path::new(s)), "{s}");
    }
    assert!(!c.is_protected(Path::new(r"\\?\c:\windows\temp\a")));
    assert!(!c.is_protected(Path::new(r"C:\Windows\Temp\.\a")));
}

#[test]
fn relative_paths_are_protected() {
    let c = ctx(&[]);
    assert!(c.is_protected(Path::new(r"a\b")));
    assert!(c.is_protected(Path::new(r"\Windows")));
    assert!(c.is_protected(Path::new(r"C:Windows")));
    assert!(c.is_protected(Path::new("")));
}

#[test]
fn allowed_entry_itself_is_not_deletable_but_its_contents_are() {
    let c = ctx(&[
        r"C:\Windows\Temp",
        r"C:\Windows\SoftwareDistribution\Download",
    ]);
    assert!(c.is_protected(Path::new(r"C:\Windows\Temp")));
    assert!(!c.is_protected_container(Path::new(r"C:\Windows\Temp")));
    assert!(!c.is_protected(Path::new(r"C:\Windows\SoftwareDistribution\Download\x")));
    assert!(c.is_protected(Path::new(r"C:\Windows\SoftwareDistribution\x")));
    assert!(c.is_protected_container(Path::new(r"C:\Windows")));
}

#[test]
fn allowed_entries_equal_to_protected_roots_are_ignored() {
    let c = ctx(&[r"C:\Windows", r"C:\", r"C:\Program Files"]);
    assert!(c.is_protected(Path::new(r"C:\Windows\System32")));
    assert!(c.is_protected(Path::new(r"C:\Program Files\App")));
    assert!(c.is_protected_container(Path::new(r"C:\Windows")));
}

#[test]
fn profile_folders_protected_as_whole_but_not_contents() {
    let c = ctx(&[]);
    for s in [
        r"C:\Users\t",
        r"C:\Users\t\Desktop",
        r"C:\Users\t\documents",
        r"C:\Users\t\Downloads",
        r"C:\Users\t\Pictures",
        r"C:\Users\t\Music",
        r"C:\Users\t\Videos",
        r"C:\Users\t\AppData",
        r"C:\Users\t\AppData\Local",
        r"C:\Users\t\AppData\Roaming",
        r"C:\Users",
    ] {
        assert!(c.is_protected(Path::new(s)), "{s}");
    }
    for s in [
        r"C:\Users\t\Downloads\a.zip",
        r"C:\Users\t\AppData\Local\Temp",
        r"C:\Users\t\AppData\Local\Temp\x",
        r"C:\Users\t\proj\node_modules",
    ] {
        assert!(!c.is_protected(Path::new(s)), "{s}");
    }
    // Conteúdo das pastas do perfil pode ser apagado; ancestrais do perfil, não.
    assert!(!c.is_protected_container(Path::new(r"C:\Users\t\Downloads")));
    assert!(!c.is_protected_container(Path::new(r"C:\Users\t\AppData\Local\Temp")));
    assert!(c.is_protected_container(Path::new(r"C:\Users")));
    assert!(c.is_protected_container(Path::new(r"C:\")));
    assert!(c.is_protected_container(Path::new(r"D:\")));
}

#[test]
fn plan_rejects_protected() {
    let pl = plan(
        vec![(Op::Recycle(p("C:\\Program Files\\App")), 10)],
        &[],
        u64::MAX,
    );
    assert_eq!(pl.rejected, vec![p("C:\\Program Files\\App")]);
    assert!(pl.ops.is_empty());
    assert_eq!(pl.bytes_to_bin, 0);
}

#[test]
fn plan_rejects_delete_contents_of_protected_dir() {
    let c = ctx(&[r"C:\Windows\Temp"]);
    let pl = plan_with(
        vec![
            (Op::DeleteContents(p(r"C:\Windows")), 5),
            (Op::DeleteContents(p(r"C:\Windows\Temp")), 7),
            (Op::DeletePermanent(p(r"C:\Windows\Temp")), 9),
        ],
        &c,
        u64::MAX,
    );
    assert_eq!(pl.rejected, vec![p(r"C:\Windows"), p(r"C:\Windows\Temp")]);
    assert_eq!(pl.ops, vec![Op::DeleteContents(p(r"C:\Windows\Temp"))]);
    assert_eq!(pl.bytes_now, 7);
}

#[test]
fn plan_splits_bytes() {
    let pl = plan_of(vec![
        (
            Op::DeleteContents(p("C:\\Users\\usuario\\AppData\\Local\\Temp")),
            100,
        ),
        (Op::Recycle(p("C:\\Users\\usuario\\Downloads\\a.zip")), 50),
    ]);
    assert_eq!(pl.bytes_now, 100);
    assert_eq!(pl.bytes_to_bin, 50);
    assert_eq!(pl.ops.len(), 2);
    assert!(pl.rejected.is_empty());
}

#[test]
fn plan_keeps_bin_and_uninstall_ops() {
    let pl = plan_of(vec![
        (Op::EmptyRecycleBin, 30),
        (
            Op::Uninstall {
                name: "App".into(),
                command: "\"C:\\Program Files\\App\\unins000.exe\"".into(),
            },
            20,
        ),
    ]);
    assert_eq!(pl.ops.len(), 2);
    assert_eq!(pl.bytes_now, 50);
    assert!(pl.rejected.is_empty());
}

#[test]
fn plan_flags_too_big() {
    let big = p("C:\\Users\\usuario\\Videos\\grande.mkv");
    let pl = plan(vec![(Op::Recycle(big.clone()), 200)], &[], 100);
    assert_eq!(pl.too_big_for_bin, vec![big.clone()]);
    assert_eq!(pl.ops, vec![Op::Recycle(big)]);
    let pl = plan(
        vec![(Op::Recycle(p("C:\\Users\\usuario\\a")), 100)],
        &[],
        100,
    );
    assert!(pl.too_big_for_bin.is_empty());
}

#[test]
fn delete_contents_skips_locked() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, vec![1u8; 1000]).unwrap();
    fs::write(&b, vec![2u8; 500]).unwrap();
    let _lock = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&b)
        .unwrap();

    let pl = plan_of(vec![(Op::DeleteContents(dir.path().to_path_buf()), 1500)]);
    assert_eq!(pl.ops.len(), 1);
    let mut calls = Vec::new();
    let r = execute(&pl, &mut |d, t| calls.push((d, t)));
    assert_eq!(r.done.len(), 1, "{r:?}");
    assert_eq!(r.skipped.len(), 1, "{r:?}");
    assert!(r.done[0].ends_with("a.txt"));
    assert!(r.skipped[0].0.ends_with("b.txt"));
    assert_eq!(r.skipped[0].1, "arquivo em uso");
    assert_eq!(r.freed_now, 1000);
    assert!(dir.path().exists());
    assert!(!a.exists());
    assert!(b.exists());
    assert_eq!(calls.last(), Some(&(1, 1)));
}

#[test]
fn delete_long_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut deep = dir.path().to_path_buf();
    for _ in 0..6 {
        deep.push("pasta_com_nome_bem_comprido_para_passar_de_260_caracteres");
    }
    let file = deep.join("arquivo_no_fundo.txt");
    assert!(file.as_os_str().len() > 300);
    let vdeep = PathBuf::from(format!(r"\\?\{}", deep.display()));
    fs::create_dir_all(&vdeep).unwrap();
    fs::write(vdeep.join("arquivo_no_fundo.txt"), b"oi").unwrap();

    let pl = plan_of(vec![(Op::DeletePermanent(file.clone()), 2)]);
    let r = exec(&pl);
    assert!(r.skipped.is_empty(), "{r:?}");
    assert_eq!(r.done, vec![file]);
    assert_eq!(r.freed_now, 2);
    assert!(!vdeep.join("arquivo_no_fundo.txt").exists());
}

#[test]
fn delete_permanent_removes_long_tree() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().join("node_modules");
    let mut deep = top.clone();
    for _ in 0..6 {
        deep.push("pacote_com_nome_bem_comprido_para_passar_de_260_caracteres");
    }
    let vdeep = PathBuf::from(format!(r"\\?\{}", deep.display()));
    fs::create_dir_all(&vdeep).unwrap();
    fs::write(vdeep.join("index.js"), vec![0u8; 300]).unwrap();
    fs::write(top.join("a.js"), vec![0u8; 20]).unwrap();

    let r = exec(&plan_of(vec![(Op::DeletePermanent(top.clone()), 320)]));
    assert!(r.skipped.is_empty(), "{r:?}");
    assert_eq!(r.done, vec![top.clone()]);
    assert_eq!(r.freed_now, 320);
    assert!(!top.exists());
}

#[test]
fn delete_clears_readonly() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join(".git");
    fs::create_dir(&sub).unwrap();
    let f = sub.join("pack.idx");
    fs::write(&f, vec![0u8; 64]).unwrap();
    let mut perm = fs::metadata(&f).unwrap().permissions();
    perm.set_readonly(true);
    fs::set_permissions(&f, perm).unwrap();
    let solo = dir.path().join("solo.txt");
    fs::write(&solo, vec![0u8; 8]).unwrap();
    let mut perm = fs::metadata(&solo).unwrap().permissions();
    perm.set_readonly(true);
    fs::set_permissions(&solo, perm).unwrap();

    // A própria pasta também somente leitura.
    let mut perm = fs::metadata(&sub).unwrap().permissions();
    perm.set_readonly(true);
    fs::set_permissions(&sub, perm).unwrap();
    assert!(fs::metadata(&sub).unwrap().permissions().readonly());

    let r = exec(&plan_of(vec![
        (Op::DeletePermanent(sub.clone()), 64),
        (Op::DeletePermanent(solo.clone()), 8),
    ]));
    assert!(r.skipped.is_empty(), "{r:?}");
    assert_eq!(r.freed_now, 72);
    assert!(!sub.exists());
    assert!(!solo.exists());
}

#[test]
fn delete_does_not_follow_junction() {
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let keep = target.path().join("importante.txt");
    fs::write(&keep, vec![9u8; 4096]).unwrap();

    let parent = dir.path().join("pai");
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("lixo.txt"), vec![0u8; 10]).unwrap();
    let link = parent.join("link");
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(target.path())
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "mklink /J falhou");
    assert!(link.join("importante.txt").exists());

    let r = exec(&plan_of(vec![(Op::DeletePermanent(parent.clone()), 10)]));
    assert!(r.skipped.is_empty(), "{r:?}");
    assert_eq!(r.freed_now, 10);
    assert!(!parent.exists());
    assert!(keep.exists());
    assert_eq!(fs::read(&keep).unwrap().len(), 4096);
}

#[test]
fn delete_contents_through_junction_is_checked_at_target() {
    // Uma junção num lugar liberado apontando para outro tempdir: o alvo real também não é
    // protegido, então o conteúdo sai — mas a verificação acontece no caminho resolvido.
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("x.bin"), vec![0u8; 5]).unwrap();
    let link = dir.path().join("cache");
    let ok = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(target.path())
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok);
    let r = exec(&plan_of(vec![(Op::DeleteContents(link.clone()), 5)]));
    assert!(r.skipped.is_empty(), "{r:?}");
    assert_eq!(r.freed_now, 5);
    assert!(link.exists());
    assert!(target.path().exists());
    assert!(!target.path().join("x.bin").exists());
}

#[test]
fn missing_path_is_skipped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("nao_existe.txt");
    let real = dir.path().join("existe.txt");
    fs::write(&real, b"abc").unwrap();
    let pl = plan_of(vec![
        (Op::DeletePermanent(gone.clone()), 1),
        (Op::DeletePermanent(real.clone()), 3),
    ]);
    let mut calls = Vec::new();
    let r = execute(&pl, &mut |d, t| calls.push((d, t)));
    assert_eq!(
        r.skipped,
        vec![(gone, "caminho não encontrado".to_string())]
    );
    assert_eq!(r.done, vec![real]);
    assert_eq!(r.freed_now, 3);
    assert_eq!(calls.last(), Some(&(2, 2)));
}

#[test]
#[ignore = "lê a Lixeira e o registro reais (só leitura)"]
fn recycle_bin_smoke() {
    let _ = recycle_bin_size();
    assert!(bin_max_bytes('C') > 0);
}

fn junction(link: &Path, target: &Path) {
    let ok = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "mklink /J falhou");
}

/// Contexto com o perfil dentro de um tempdir (`<tmp>\prof`). Tudo em tempdir.
fn tmp_profile_ctx(tmp: &Path) -> (ProtectCtx, PathBuf) {
    let tmp = fs::canonicalize(tmp).unwrap();
    let tmp = PathBuf::from(tmp.to_string_lossy().trim_start_matches(r"\\?\"));
    let prof = tmp.join("prof");
    fs::create_dir_all(prof.join("Downloads")).unwrap();
    let env = Env {
        local_app_data: prof.join(r"AppData\Local"),
        app_data: prof.join(r"AppData\Roaming"),
        user_profile: prof.clone(),
        windows: PathBuf::from(r"C:\Windows"),
        system_drive: PathBuf::from(r"C:\"),
    };
    (ProtectCtx::from_parts(&[], &env), tmp)
}

#[test]
fn recheck_blocks_protected_folder_reached_through_junction() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tmp) = tmp_profile_ctx(dir.path());
    let downloads = tmp.join("prof").join("Downloads");
    fs::write(downloads.join("a.zip"), b"zip").unwrap();
    let j = tmp.join("j");
    junction(&j, &tmp.join("prof"));

    let via = j.join("Downloads");
    let pl = plan_with(vec![(Op::DeletePermanent(via.clone()), 3)], &c, u64::MAX);
    assert_eq!(pl.ops.len(), 1, "o caminho lexical passa no plano");
    let r = execute_with(&pl, &c, &mut |_, _| {});
    assert_eq!(r.skipped, vec![(via, "caminho protegido".to_string())]);
    assert!(r.done.is_empty());
    assert_eq!(r.freed_now, 0);
    assert!(downloads.join("a.zip").exists());
}

#[test]
fn recheck_blocks_delete_contents_of_profile_ancestor_through_junction() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tmp) = tmp_profile_ctx(dir.path());
    fs::write(tmp.join("prof").join("Downloads").join("a.zip"), b"zip").unwrap();
    fs::write(tmp.join("solto.txt"), b"s").unwrap();
    let side = tempfile::tempdir().unwrap();
    let j = side.path().join("j");
    junction(&j, &tmp); // j -> ancestral do perfil

    let pl = plan_with(vec![(Op::DeleteContents(j.clone()), 4)], &c, u64::MAX);
    assert_eq!(pl.ops.len(), 1);
    let r = execute_with(&pl, &c, &mut |_, _| {});
    assert_eq!(r.skipped, vec![(j, "caminho protegido".to_string())]);
    assert!(r.done.is_empty());
    assert!(tmp.join("solto.txt").exists());
    assert!(tmp.join("prof").join("Downloads").join("a.zip").exists());
}

#[test]
fn delete_contents_removes_junction_child_as_link() {
    let dir = tempfile::tempdir().unwrap();
    let (c, tmp) = tmp_profile_ctx(dir.path());
    let target = tempfile::tempdir().unwrap();
    let keep = target.path().join("importante.txt");
    fs::write(&keep, vec![1u8; 100]).unwrap();
    let cont = tmp.join("cache");
    fs::create_dir(&cont).unwrap();
    fs::write(cont.join("lixo.bin"), vec![0u8; 7]).unwrap();
    junction(&cont.join("link"), target.path());

    let pl = plan_with(vec![(Op::DeleteContents(cont.clone()), 7)], &c, u64::MAX);
    let r = execute_with(&pl, &c, &mut |_, _| {});
    assert!(r.skipped.is_empty(), "{r:?}");
    assert_eq!(r.done.len(), 2);
    assert_eq!(r.freed_now, 7);
    assert!(cont.exists());
    assert!(!cont.join("link").exists());
    assert!(keep.exists());
}
