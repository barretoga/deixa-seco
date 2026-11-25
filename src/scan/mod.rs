//! Varredura de disco: eventos, thread própria e escolha entre MFT e varredura de pastas.

pub mod mft;
pub mod walk;

use crate::kind::FileKind;
use crate::tree::Tree;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMethod {
    Mft,
    Walk,
}

pub enum ScanEvent {
    Progress {
        files: u64,
        bytes: u64,
    },
    /// Mapa parcial durante a varredura de pastas (`Tree::preview_with_kinds(PREVIEW_DEPTH)`
    /// da árvore ainda incompleta: a árvore pequena e o tipo dominante de cada nó dela, contado
    /// na árvore inteira). A MFT não manda: os dados dela só existem no fim.
    Preview {
        tree: Tree,
        kinds: Vec<FileKind>,
    },
    Done(Tree, ScanMethod),
    Failed(String),
}

/// Níveis abaixo da raiz copiados para o mapa parcial.
pub const PREVIEW_DEPTH: u8 = 3;

/// Ritmo dos eventos da varredura (injetável em testes).
#[derive(Clone, Copy, Debug)]
struct Pacing {
    /// Intervalo mínimo entre `Progress`.
    progress: Duration,
    /// A cada quantas entradas a varredura de pastas chama os callbacks.
    stride: u64,
    /// Manda `Preview`.
    previews: bool,
    /// Primeiro `Preview` depois do início.
    preview_first: Duration,
    /// Intervalo mínimo entre `Preview` (contado depois de montar o anterior; cresce com o custo,
    /// ver `next_preview_interval`).
    preview_every: Duration,
    /// Quantas vezes o tempo de montar o último `Preview` esperar até o próximo (0 = sem recuo:
    /// nos testes, o ritmo não pode depender do relógio).
    preview_backoff: u32,
}

const PACING: Pacing = Pacing {
    progress: Duration::from_millis(100),
    stride: walk::PROGRESS_STRIDE,
    previews: true,
    preview_first: Duration::from_millis(500),
    preview_every: Duration::from_secs(1),
    preview_backoff: PREVIEW_BACKOFF,
};

/// Recuo do `Preview` na varredura real (ver `next_preview_interval`).
const PREVIEW_BACKOFF: u32 = 8;

/// Inicia a varredura em uma thread própria. `Progress` sai no máximo a cada 100 ms e,
/// na varredura de pastas, `Preview` aos 500 ms e depois a cada 1 s; a thread termina com
/// exatamente um `Done` ou `Failed`. Se o `Receiver` for descartado, a varredura para cedo.
pub fn start_scan(root: PathBuf, try_mft: bool) -> Receiver<ScanEvent> {
    start_scan_with(root, try_mft, true)
}

/// `start_scan` com o mapa parcial opcional (`previews = false`: nenhum `Preview`).
pub fn start_scan_with(root: PathBuf, try_mft: bool, previews: bool) -> Receiver<ScanEvent> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        run_scan(&root, try_mft, &tx, Pacing { previews, ..PACING });
    });
    rx
}

/// Corpo da thread de varredura. Devolve o `ErrorKind` se a varredura terminou com erro.
fn run_scan(
    root: &Path,
    try_mft: bool,
    tx: &Sender<ScanEvent>,
    pacing: Pacing,
) -> Option<io::ErrorKind> {
    let cancel = AtomicBool::new(false);
    let mut error_kind = None;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut last = Instant::now();
        let mut on_progress = |files: u64, bytes: u64| {
            if last.elapsed() >= pacing.progress {
                last = Instant::now();
                if tx.send(ScanEvent::Progress { files, bytes }).is_err() {
                    // Receiver descartado: ninguém espera o resultado.
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        };
        // Roda na thread que consome o jwalk, entre lotes: os leitores paralelos não esperam
        // por ela (a árvore só é lida aqui, sem trava).
        let mut next_preview = Instant::now() + pacing.preview_first;
        let mut on_tree = |tree: &Tree| {
            if !pacing.previews || cancel.load(Ordering::Relaxed) || Instant::now() < next_preview {
                return;
            }
            let started = Instant::now();
            let (tree, kinds) = tree.preview_with_kinds(PREVIEW_DEPTH);
            let built = started.elapsed();
            if tx.send(ScanEvent::Preview { tree, kinds }).is_err() {
                cancel.store(true, Ordering::Relaxed);
            }
            next_preview = Instant::now()
                + next_preview_interval(pacing.preview_every, built, pacing.preview_backoff);
        };
        let letter = if try_mft { mft_letter(root) } else { None };
        scan_tree(
            root,
            letter,
            mft::mft_scan_with,
            &mut on_progress,
            &mut on_tree,
            &cancel,
            pacing.stride,
        )
    }));
    let event = match outcome {
        Ok(Ok((tree, method))) => ScanEvent::Done(tree, method),
        Ok(Err(e)) => {
            error_kind = Some(e.kind());
            ScanEvent::Failed(e.to_string())
        }
        Err(_) => ScanEvent::Failed("a varredura falhou inesperadamente".into()),
    };
    let _ = tx.send(event);
    error_kind
}

/// Espera até o próximo `Preview`: `every`, ou `backoff`× o tempo de montar o último, o que
/// for maior. Com a árvore crescendo, montar o preview fica mais caro; com o recuo de 8× a
/// thread que consome o jwalk nunca passa mais de 1/9 (~11%) do tempo nisso.
fn next_preview_interval(every: Duration, last_build: Duration, backoff: u32) -> Duration {
    every.max(last_build * backoff)
}

/// Letra de `root` se for a raiz de um volume (`C:\`, `C:` ou `C:/`).
fn drive_letter(root: &Path) -> Option<char> {
    match root.to_str()?.as_bytes() {
        [l, b':'] | [l, b':', b'\\' | b'/'] if l.is_ascii_alphabetic() => {
            Some(l.to_ascii_uppercase() as char)
        }
        _ => None,
    }
}

/// Letra do volume se a MFT pode ser usada: `root` é raiz de volume NTFS e o processo é admin.
fn mft_letter(root: &Path) -> Option<char> {
    let letter = drive_letter(root)?;
    if !crate::elevation::is_elevated() {
        return None;
    }
    crate::volumes::list_volumes()
        .iter()
        .any(|v| v.letter == letter && v.fs.eq_ignore_ascii_case("NTFS"))
        .then_some(letter)
}

/// Assinatura de `mft::mft_scan_with` (injetável em testes).
type MftFn = fn(char, &mut dyn FnMut(u64, u64), &AtomicBool, u64) -> io::Result<Tree>;

/// Tenta a MFT quando há letra; qualquer erro ou pânico nela cai para a varredura de pastas,
/// exceto o cancelamento, que é devolvido direto. `on_tree` (árvore parcial) só é chamado pela
/// varredura de pastas.
fn scan_tree(
    root: &Path,
    mft_letter: Option<char>,
    mft: MftFn,
    on_progress: &mut dyn FnMut(u64, u64),
    on_tree: &mut dyn FnMut(&Tree),
    cancel: &AtomicBool,
    stride: u64,
) -> io::Result<(Tree, ScanMethod)> {
    if let Some(letter) = mft_letter {
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            mft(letter, &mut *on_progress, cancel, stride)
        }));
        match attempt {
            Ok(Ok(tree)) => return Ok((tree, ScanMethod::Mft)),
            Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
            Ok(Err(_)) | Err(_) => {} // MFT ilegível: segue com a varredura de pastas
        }
    }
    walk::walk_scan_with(
        root,
        on_progress,
        on_tree,
        cancel,
        stride,
        walk::default_parallelism(),
    )
    .map(|tree| (tree, ScanMethod::Walk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letter_only_for_volume_roots() {
        assert_eq!(drive_letter(Path::new(r"C:\")), Some('C'));
        assert_eq!(drive_letter(Path::new("d:")), Some('D'));
        assert_eq!(drive_letter(Path::new("E:/")), Some('E'));
        assert_eq!(drive_letter(Path::new(r"C:\Users")), None);
        assert_eq!(drive_letter(Path::new(r"\\server\share")), None);
        assert_eq!(drive_letter(Path::new(r"1:\")), None);
    }

    fn mft_fails(_: char, _: &mut dyn FnMut(u64, u64), _: &AtomicBool, _: u64) -> io::Result<Tree> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "sem admin"))
    }

    fn mft_panics(
        _: char,
        _: &mut dyn FnMut(u64, u64),
        _: &AtomicBool,
        _: u64,
    ) -> io::Result<Tree> {
        panic!("registro corrompido")
    }

    fn mft_cancelled(
        _: char,
        _: &mut dyn FnMut(u64, u64),
        _: &AtomicBool,
        _: u64,
    ) -> io::Result<Tree> {
        Err(io::Error::new(io::ErrorKind::Interrupted, "cancelada"))
    }

    fn mft_ok(c: char, p: &mut dyn FnMut(u64, u64), _: &AtomicBool, _: u64) -> io::Result<Tree> {
        p(1, 10);
        let mut t = Tree::new(PathBuf::from(format!("{c}:\\")));
        t.add(t.root(), "$MFT", false, 10, None);
        t.finalize();
        Ok(t)
    }

    fn scan_dir(mft_letter: Option<char>, mft: MftFn) -> io::Result<(Tree, ScanMethod)> {
        let dir = tempfile::tempdir().unwrap();
        make_files(dir.path(), 3);
        scan_tree(
            dir.path(),
            mft_letter,
            mft,
            &mut |_, _| {},
            &mut |_| {},
            &AtomicBool::new(false),
            1,
        )
    }

    #[test]
    fn mft_success_is_used() {
        let (tree, method) = scan_dir(Some('C'), mft_ok).unwrap();
        assert_eq!(method, ScanMethod::Mft);
        assert_eq!(tree.node(tree.root()).size, 10);
    }

    #[test]
    fn mft_error_falls_back_to_walk() {
        let (tree, method) = scan_dir(Some('C'), mft_fails).unwrap();
        assert_eq!(method, ScanMethod::Walk);
        assert_eq!(tree.node(tree.root()).files, 3);
    }

    #[test]
    fn mft_panic_falls_back_to_walk() {
        let (_, method) = scan_dir(Some('C'), mft_panics).unwrap();
        assert_eq!(method, ScanMethod::Walk);
    }

    #[test]
    fn mft_cancel_does_not_fall_back() {
        let err = scan_dir(Some('C'), mft_cancelled).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn no_mft_letter_walks() {
        let (_, method) = scan_dir(None, mft_ok).unwrap();
        assert_eq!(method, ScanMethod::Walk);
    }

    #[test]
    fn try_mft_on_plain_folder_walks() {
        // Pasta comum (não é raiz de volume): sempre varredura de pastas, mesmo com try_mft.
        let dir = tempfile::tempdir().unwrap();
        make_files(dir.path(), 3);
        let (tx, rx) = mpsc::channel();
        assert_eq!(run_scan(dir.path(), true, &tx, eager(false)), None);
        let done = rx.try_iter().last();
        assert!(matches!(done, Some(ScanEvent::Done(_, ScanMethod::Walk))));
    }

    fn make_files(dir: &Path, n: usize) {
        for i in 0..n {
            std::fs::write(dir.join(format!("f{i}.txt")), b"x").unwrap();
        }
    }

    #[test]
    fn dropped_receiver_cancels_walk() {
        let dir = tempfile::tempdir().unwrap();
        make_files(dir.path(), 300);
        let (tx, rx) = mpsc::channel();
        drop(rx);
        // Intervalo zero e passo 1: o primeiro Progress já falha o send e aciona o cancelamento.
        let kind = run_scan(dir.path(), false, &tx, eager(false));
        assert_eq!(kind, Some(io::ErrorKind::Interrupted));
    }

    /// Tudo sem espera: Progress e Preview a cada entrada.
    fn eager(previews: bool) -> Pacing {
        Pacing {
            progress: Duration::ZERO,
            stride: 1,
            previews,
            preview_first: Duration::ZERO,
            preview_every: Duration::ZERO,
            // Sem recuo: um `Preview` depois de cada entrada, inclusive a última (o teste
            // `walk_emits_preview_before_done` confere que a última prévia viu tudo).
            preview_backoff: 0,
        }
    }

    /// tempdir com 5 níveis (a\b\c\d\fundo.bin) e 200 arquivos soltos.
    fn deep_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join(r"a\b\c\d");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("fundo.bin"), vec![1u8; 5000]).unwrap();
        make_files(dir.path(), 200);
        dir
    }

    fn max_depth(t: &Tree) -> usize {
        (0..t.len())
            .map(|i| t.ancestors(crate::tree::NodeId(i as u32)).len() - 1)
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn walk_emits_preview_before_done() {
        let dir = deep_dir();
        let (tx, rx) = mpsc::channel();
        assert_eq!(run_scan(dir.path(), false, &tx, eager(true)), None);
        let events: Vec<_> = rx.try_iter().collect();
        let first_preview = events
            .iter()
            .position(|e| matches!(e, ScanEvent::Preview { .. }))
            .expect("esperava ao menos um Preview");
        let done = events
            .iter()
            .position(|e| matches!(e, ScanEvent::Done(..)))
            .expect("Done");
        assert!(first_preview < done);
        assert_eq!(done, events.len() - 1, "Done é o último");
        let ScanEvent::Done(full, _) = &events[done] else {
            unreachable!()
        };
        for e in &events {
            if let ScanEvent::Preview { tree: p, kinds } = e {
                assert_eq!(kinds.len(), p.len());
                assert!(max_depth(p) <= 3, "preview só até o nível 3");
                assert!(p.node(p.root()).size <= full.node(full.root()).size);
            }
        }
        // A última prévia já viu o fundo.bin (5 níveis abaixo) pelo tamanho de `a`.
        let last = events[..done]
            .iter()
            .rev()
            .find_map(|e| match e {
                ScanEvent::Preview { tree, .. } => Some(tree),
                _ => None,
            })
            .unwrap();
        let a = last.find(&dir.path().join("a")).expect("a no preview");
        assert!(last.node(a).size >= 5000, "subárvore inteira somada");
    }

    #[test]
    fn previews_off_or_not_due_emit_none() {
        let dir = deep_dir();
        for pacing in [
            eager(false),
            Pacing {
                preview_first: Duration::from_secs(3600),
                ..eager(true)
            },
        ] {
            let (tx, rx) = mpsc::channel();
            assert_eq!(run_scan(dir.path(), false, &tx, pacing), None);
            assert!(
                !rx.try_iter()
                    .any(|e| matches!(e, ScanEvent::Preview { .. })),
                "sem Preview"
            );
        }
    }

    #[test]
    fn preview_interval_backs_off_with_build_time() {
        let base = Duration::from_secs(1);
        let ms = Duration::from_millis;
        assert_eq!(
            next_preview_interval(base, ms(10), PREVIEW_BACKOFF),
            base,
            "preview rápido: 1 s"
        );
        assert_eq!(
            next_preview_interval(base, ms(125), PREVIEW_BACKOFF),
            base,
            "125 ms × 8 = 1 s"
        );
        assert_eq!(
            next_preview_interval(base, ms(250), PREVIEW_BACKOFF),
            ms(2000)
        );
        // Fração do tempo da thread gasta montando previews: no máximo 1/9 (~11%).
        for build in [ms(1), ms(130), ms(400), ms(2000)] {
            let total = build + next_preview_interval(base, build, PREVIEW_BACKOFF);
            let share = build.as_secs_f64() / total.as_secs_f64();
            assert!(share <= 0.12, "{build:?}: {share}");
        }
    }

    #[test]
    fn mft_path_emits_no_preview() {
        let dir = deep_dir();
        let mut calls = 0;
        let (_, method) = scan_tree(
            dir.path(),
            Some('C'),
            mft_ok,
            &mut |_, _| {},
            &mut |_| calls += 1,
            &AtomicBool::new(false),
            1,
        )
        .unwrap();
        assert_eq!(method, ScanMethod::Mft);
        assert_eq!(calls, 0);
    }

    #[test]
    fn live_receiver_completes_walk() {
        let dir = tempfile::tempdir().unwrap();
        make_files(dir.path(), 300);
        let (tx, rx) = mpsc::channel();
        let kind = run_scan(dir.path(), false, &tx, eager(false));
        assert_eq!(kind, None);
        let events: Vec<_> = rx.try_iter().collect();
        assert!(matches!(events.last(), Some(ScanEvent::Done(..))));
        assert!(events.len() > 1, "esperava eventos Progress");
    }
}
