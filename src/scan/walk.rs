//! Varredura paralela de pastas com jwalk. Sem seguir reparse points (junções, symlinks).
//!
//! Tamanho = tamanho alocado: tamanho lógico arredondado para cima ao cluster do volume;
//! arquivos compressed/sparse usam `GetCompressedFileSizeW`. Hardlinks NÃO são deduplicados
//! aqui (só na varredura pela MFT).

use crate::tree::{NodeId, Tree};
use jwalk::{ClientState, DirEntry, Parallelism, WalkDirGeneric};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::Storage::FileSystem::{GetCompressedFileSizeW, GetDiskFreeSpaceW};
use windows::core::PCWSTR;

const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_SPARSE: u32 = 0x200;
const ATTR_REPARSE_POINT: u32 = 0x400;
const ATTR_OFFLINE: u32 = 0x1000;
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
const ATTR_COMPRESSED: u32 = 0x800;
const FALLBACK_CLUSTER: u64 = 4096;
pub(crate) const PROGRESS_STRIDE: u64 = 512;

/// Metadados coletados em paralelo dentro do `process_read_dir`.
#[derive(Default, Debug, Clone, Copy)]
struct Info {
    size: u64,
    mtime: Option<SystemTime>,
    is_dir: bool,
}

#[derive(Default, Debug)]
struct Gen;

impl ClientState for Gen {
    /// Tamanho de cluster do volume, herdado por todos os diretórios.
    type ReadDirState = u64;
    type DirEntryState = Option<Info>;
}

/// Caminho largo terminado em NUL, com prefixo `\\?\` para suportar mais de 260 caracteres.
fn wide(path: &Path) -> Vec<u16> {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text = abs.to_string_lossy();
    let mut v: Vec<u16> = Vec::with_capacity(text.len() + 8);
    if text.starts_with(r"\\?\") {
        v.extend(abs.as_os_str().encode_wide());
    } else if let Some(rest) = text.strip_prefix(r"\\") {
        v.extend(r"\\?\UNC\".encode_utf16());
        v.extend(OsStr::new(rest).encode_wide());
    } else {
        v.extend(r"\\?\".encode_utf16());
        v.extend(abs.as_os_str().encode_wide());
    }
    v.push(0);
    v
}

/// Tamanho de cluster (bytes) do volume que contém `path`; 4096 se não for possível obter.
pub fn cluster_size(path: &Path) -> u64 {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut root = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => root.push(c.as_os_str()),
            _ => break,
        }
    }
    let mut w: Vec<u16> = root.as_os_str().encode_wide().collect();
    if w.last() != Some(&(b'\\' as u16)) {
        w.push(b'\\' as u16);
    }
    w.push(0);
    let (mut spc, mut bps) = (0u32, 0u32);
    // SAFETY: `w` é terminado em NUL e os ponteiros de saída são válidos.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            PCWSTR(w.as_ptr()),
            Some(&mut spc),
            Some(&mut bps),
            None,
            None,
        )
    };
    match ok {
        Ok(()) if spc > 0 && bps > 0 => u64::from(spc) * u64::from(bps),
        _ => FALLBACK_CLUSTER,
    }
}

fn compressed_size(path: &Path) -> Option<u64> {
    let w = wide(path);
    let mut high = 0u32;
    // SAFETY: `w` é terminado em NUL; `high` é um ponteiro de saída válido.
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(w.as_ptr()), Some(&mut high)) };
    // SAFETY: sem argumentos; lê o último erro da thread.
    if low == u32::MAX && unsafe { GetLastError() }.0 != 0 {
        return None;
    }
    Some((u64::from(high) << 32) | u64::from(low))
}

fn round_up(size: u64, cluster: u64) -> u64 {
    size.div_ceil(cluster) * cluster
}

/// Roda em paralelo por diretório: lê metadados, decide tamanho e impede descer em reparse points.
fn collect(cluster: u64, entries: &mut [Result<DirEntry<Gen>, jwalk::Error>]) {
    let cluster = cluster.max(1);
    for entry in entries.iter_mut().flatten() {
        let Ok(md) = entry.metadata() else {
            // Sem metadados: conta como entrada vazia.
            entry.client_state = Some(Info {
                is_dir: entry.file_type.is_dir(),
                ..Info::default()
            });
            continue;
        };
        let attrs = md.file_attributes();
        let is_dir = attrs & ATTR_DIRECTORY != 0;
        let mut info = Info {
            size: 0,
            mtime: md.modified().ok(),
            is_dir,
        };
        // Symlink/junção (reparse points "name surrogate"): entrada de tamanho 0, sem descer.
        // Os demais reparse points (OneDrive, WOF/CompactOS, dedup) são arquivos/pastas normais.
        if entry.file_type.is_symlink() {
            entry.read_children = None;
        } else if !is_dir {
            let logical = md.len();
            info.size = if attrs & (ATTR_RECALL_ON_DATA_ACCESS | ATTR_OFFLINE) != 0 {
                // Placeholder não hidratado: não ocupa disco. Nunca abrir o conteúdo.
                0
            } else if attrs & (ATTR_COMPRESSED | ATTR_SPARSE | ATTR_REPARSE_POINT) != 0 {
                compressed_size(&entry.path()).unwrap_or_else(|| round_up(logical, cluster))
            } else {
                round_up(logical, cluster)
            };
        }
        entry.client_state = Some(info);
    }
}

/// Varre `root` em paralelo e devolve a árvore já finalizada.
/// `on_progress(arquivos, bytes)` é chamado a cada poucas centenas de entradas.
pub fn walk_scan(root: &Path, on_progress: &mut dyn FnMut(u64, u64)) -> io::Result<Tree> {
    walk_scan_cancellable(root, on_progress, &AtomicBool::new(false))
}

/// Como `walk_scan`, mas para cedo com `ErrorKind::Interrupted` quando `cancel` vira `true`.
pub fn walk_scan_cancellable(
    root: &Path,
    on_progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> io::Result<Tree> {
    walk_scan_with(
        root,
        on_progress,
        &mut |_| {},
        cancel,
        PROGRESS_STRIDE,
        default_parallelism(),
    )
}

/// Como `walk_scan_cancellable`, mas numa thread só, sem o pool global do rayon: para pastas
/// pequenas medidas enquanto outra varredura pode estar ocupando o pool (apps da Store).
pub fn walk_scan_serial(
    root: &Path,
    on_progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> io::Result<Tree> {
    walk_scan_with(
        root,
        on_progress,
        &mut |_| {},
        cancel,
        PROGRESS_STRIDE,
        Parallelism::Serial,
    )
}

/// O padrão do jwalk: pool global do rayon, desistindo se ele não atender em 1 s.
pub(crate) fn default_parallelism() -> Parallelism {
    Parallelism::RayonDefaultPool {
        busy_timeout: std::time::Duration::from_secs(1),
    }
}

/// Mensagem quando o pool do rayon não atende (outra varredura o ocupa).
pub const BUSY_MESSAGE: &str =
    "o computador estava ocupado com outra varredura; tente analisar de novo";

/// Erro de um item do jwalk: `Some` = a varredura inteira tem que falhar (pool ocupado: seguir
/// daria uma árvore vazia, como se a pasta estivesse vazia); `None` = item ilegível, ignora.
fn fatal_item_error(busy: bool) -> Option<io::Error> {
    busy.then(|| io::Error::other(BUSY_MESSAGE))
}

/// Núcleo da varredura; `stride` = a cada quantas entradas chama `on_progress` e `on_tree`
/// (injetável em testes). `on_tree` recebe a árvore parcial, ainda não finalizada (para o mapa
/// parcial); roda nesta thread, entre os lotes que o jwalk entrega.
pub(crate) fn walk_scan_with(
    root: &Path,
    on_progress: &mut dyn FnMut(u64, u64),
    on_tree: &mut dyn FnMut(&Tree),
    cancel: &AtomicBool,
    stride: u64,
    parallelism: Parallelism,
) -> io::Result<Tree> {
    let stride = stride.max(1);
    std::fs::symlink_metadata(root)?;
    let cluster = cluster_size(root);
    let walker = WalkDirGeneric::<Gen>::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false)
        .parallelism(parallelism)
        .root_read_dir_state(cluster)
        .process_read_dir(|_, _, cluster, entries| collect(*cluster, entries));

    let mut tree = Tree::new(root.to_path_buf());
    // Só diretórios entram aqui; jwalk sempre entrega o diretório pai antes dos filhos.
    let mut dirs: HashMap<PathBuf, NodeId> = HashMap::new();
    dirs.insert(root.to_path_buf(), tree.root());
    let (mut files, mut bytes, mut seen) = (0u64, 0u64, 0u64);

    for item in walker {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "varredura cancelada",
            ));
        }
        let entry = match item {
            Ok(entry) => entry,
            Err(e) => match fatal_item_error(e.is_busy()) {
                Some(fatal) => return Err(fatal),
                None => continue, // entrada ilegível: ignora
            },
        };
        let denied = entry
            .read_children
            .as_ref()
            .is_some_and(|rc| rc.error().is_some());
        if entry.depth == 0 {
            if denied {
                tree.set_no_access(tree.root());
            }
            continue;
        }
        let Some(&parent) = dirs.get(entry.parent_path()) else {
            continue;
        };
        let info = entry.client_state.unwrap_or_default();
        let name = entry.file_name().to_string_lossy();
        let id = tree.add(parent, &name, info.is_dir, info.size, info.mtime);
        if info.is_dir {
            if denied {
                tree.set_no_access(id);
            }
            if entry.read_children.is_some() {
                dirs.insert(entry.path(), id);
            }
        } else {
            files += 1;
            bytes += info.size;
        }
        seen += 1;
        if seen % stride == 0 {
            on_progress(files, bytes);
            on_tree(&tree);
        }
    }
    on_progress(files, bytes);
    drop(dirs);
    tree.finalize();
    tree.shrink();
    Ok(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_pool_is_fatal_other_item_errors_are_skipped() {
        let e = fatal_item_error(true).expect("pool ocupado derruba a varredura");
        assert_eq!(e.to_string(), BUSY_MESSAGE);
        assert!(
            fatal_item_error(false).is_none(),
            "item ilegível só é pulado"
        );
    }

    #[test]
    fn serial_walk_matches_parallel_walk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a").join("b")).unwrap();
        std::fs::write(
            dir.path().join("a").join("b").join("x.bin"),
            vec![0u8; 9000],
        )
        .unwrap();
        std::fs::write(dir.path().join("y.bin"), vec![0u8; 10]).unwrap();
        let cancel = AtomicBool::new(false);
        let s = walk_scan_serial(dir.path(), &mut |_, _| {}, &cancel).unwrap();
        let p = walk_scan(dir.path(), &mut |_, _| {}).unwrap();
        assert_eq!(s.node(s.root()).size, p.node(p.root()).size);
        assert_eq!(s.node(s.root()).files, 2);
    }
}
