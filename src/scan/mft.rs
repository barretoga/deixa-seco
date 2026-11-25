//! Varredura pela MFT (NTFS): lê o volume cru (`\\.\C:`) e monta a árvore a partir dos
//! file records, sem abrir pastas. Precisa de administrador.
//!
//! Camadas (as duas últimas testáveis sem admin, sobre qualquer `Read + Seek`):
//! - [`AlignedReader`]: só faz leituras alinhadas ao setor (exigência de handles de volume),
//!   em blocos de 4 MB na leitura sequencial e de 64 KB nas leituras soltas, com cache LRU.
//! - [`read_records`]: percorre todos os file records com o crate `ntfs` e extrai [`MftRecord`]s.
//! - [`build_tree`]: registros → [`Tree`] (hardlinks, nome Win32, órfãos, metafiles).
//!
//! Decisões:
//! - Registros fora de uso (bitmap da `$MFT` ou flag IN_USE) são ignorados: apagados somem.
//! - Registros de extensão (sem `$STANDARD_INFORMATION`) são ignorados; o crate já segue a
//!   attribute list a partir do registro base.
//! - Nome: um nó por `$FILE_NAME` que não é só-DOS (Win32, Win32+DOS ou POSIX); o nome 8.3 só é
//!   usado se o registro não tiver outro.
//! - Órfão = registro em uso cujo pai não é uma pasta em uso (pai inexistente, que é arquivo ou
//!   com número de sequência diferente, ou seja, reutilizado). Órfãos vão para a raiz, para o
//!   espaço aparecer. Pastas inalcançáveis por ciclo (FS inconsistente) são descartadas.
//! - Metafiles `$*` na raiz são ignorados, com tudo dentro (ex.: `$Extend\$UsnJrnl`), exceto
//!   `$MFT`, que conta como arquivo. Metafile = registro reservado (número < 24); pastas comuns
//!   que começam com `$` (`$Recycle.Bin`, `$WinREAgent`, `$Windows.~BT`) continuam na árvore.
//! - Tamanho alocado = soma dos data runs não esparsos do `$DATA` sem nome, mais o stream
//!   `WofCompressedData` (onde o Windows guarda o conteúdo de arquivos compactados pelo
//!   CompactOS/WOF; o `$DATA` sem nome deles é esparso). `$DATA` residente = 0 (mora na própria
//!   MFT). `$DATA` espalhado em vários registros (attribute list, ex.: VHDX grande e
//!   fragmentado): soma os runs de cada pedaço, lidos pela `$ATTRIBUTE_LIST`.
//! - Outros streams alternativos (ADS) não contam, de propósito, como na varredura de pastas.
//! - Hardlinks: o tamanho entra só no primeiro nome colocado na árvore; os outros nomes ficam
//!   com 0 (mas contam como arquivo).
//! - Pastas têm tamanho próprio 0, como na varredura de pastas.

use crate::tree::{NodeId, Tree};
use ntfs::attribute_value::{NtfsAttributeValue, NtfsNonResidentAttributeValue};
use ntfs::structured_values::{
    NtfsAttributeList, NtfsFileName, NtfsFileNamespace, NtfsStandardInformation,
};
use ntfs::{NtfsAttribute, NtfsAttributeType, NtfsError, NtfsFile, NtfsFileFlags, NtfsReadSeek};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceW;
use windows::core::PCWSTR;

/// Bloco grande, para leitura sequencial (a varredura da MFT).
pub(crate) const CHUNK: usize = 4 << 20;
/// Bloco pequeno, para leituras soltas (registro 0, registros de extensão, attribute lists).
const SMALL: usize = 64 << 10;
/// Blocos de cada tamanho mantidos em cache.
const SLOTS: usize = 4;
/// Alinhamento mínimo das leituras; múltiplo de setores de 512 e de 4096 bytes.
const MIN_ALIGN: u64 = 4096;
/// Número do registro da pasta raiz.
const ROOT_FRN: u64 = 5;
/// Registros 0..24 são reservados para metafiles (`$MFT` … `$Extend` e reservas).
const FIRST_USER_FRN: u64 = 24;
/// Diferença entre 1601-01-01 e 1970-01-01 em unidades de 100 ns.
const NT_TO_UNIX: u64 = 116_444_736_000_000_000;

struct Slot {
    start: u64,
    data: Vec<u8>,
    used: u64,
    big: bool,
}

impl Slot {
    fn covers(&self, pos: u64) -> bool {
        pos >= self.start && pos - self.start < self.data.len() as u64
    }
}

/// `Read + Seek` que só repassa ao leitor interno leituras com offset e tamanho múltiplos de
/// `align` (exigência de handles de volume cru).
///
/// Leitura sequencial (a posição pedida continua um bloco em cache) carrega blocos grandes de
/// `chunk` bytes; leitura solta carrega blocos pequenos de `small` bytes. Cada tipo tem um
/// cache LRU de `slots` blocos. Isso importa porque `Ntfs::file` relê o registro 0 a cada
/// chamada e attribute lists apontam para registros longe da posição corrente: sem cache,
/// cada registro custaria recargas de 4 MB.
pub(crate) struct AlignedReader<R> {
    inner: R,
    align: u64,
    chunk: usize,
    small: usize,
    max_slots: usize,
    slots: Vec<Slot>,
    pos: u64,
    tick: u64,
}

impl<R: Read + Seek> AlignedReader<R> {
    /// `chunk` e `small` precisam ser múltiplos de `align`.
    pub(crate) fn new(inner: R, align: u64, chunk: usize, small: usize, slots: usize) -> Self {
        let align = align.max(1);
        for size in [chunk, small] {
            assert!(
                size as u64 >= align && (size as u64).is_multiple_of(align),
                "bloco deve ser múltiplo do alinhamento"
            );
        }
        AlignedReader {
            inner,
            align,
            chunk,
            small,
            max_slots: slots.max(1),
            slots: Vec::new(),
            pos: 0,
            tick: 0,
        }
    }

    /// Lê `len` bytes a partir de `start` (alinhado). Volta curto no fim do volume. Se uma
    /// leitura grande falhar (ex.: passa do fim de um volume cru), tenta com pedaços menores.
    fn load(&mut self, start: u64, len: usize) -> io::Result<Vec<u8>> {
        let align = self.align as usize;
        let mut buf = vec![0u8; len];
        let mut filled = 0usize;
        let mut step = len;
        self.inner.seek(SeekFrom::Start(start))?;
        while filled < len {
            let want = step.min(len - filled);
            match self.inner.read(&mut buf[filled..filled + want]) {
                Ok(0) => break,
                Ok(n) => {
                    filled += n;
                    if n % align != 0 {
                        break; // cauda do volume: a próxima leitura estaria desalinhada
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    if step > align {
                        step = (step / 2 / align).max(1) * align;
                        self.inner.seek(SeekFrom::Start(start + filled as u64))?;
                    } else if filled > 0 {
                        break; // fim do volume no meio do bloco
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }

    fn slot_for(&mut self, pos: u64) -> io::Result<usize> {
        self.tick += 1;
        if let Some(i) = self.slots.iter().position(|s| s.covers(pos)) {
            self.slots[i].used = self.tick;
            return Ok(i);
        }
        let start = pos - pos % self.align;
        let sequential = start > 0 && self.slots.iter().any(|s| s.covers(start - 1));
        let len = if sequential { self.chunk } else { self.small };
        let slot = Slot {
            start,
            data: self.load(start, len)?,
            used: self.tick,
            big: sequential,
        };
        let same_kind = self.slots.iter().filter(|s| s.big == sequential).count();
        if same_kind < self.max_slots {
            self.slots.push(slot);
            return Ok(self.slots.len() - 1);
        }
        let lru = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.big == sequential)
            .min_by_key(|(_, s)| s.used)
            .map_or(0, |(i, _)| i);
        self.slots[lru] = slot;
        Ok(lru)
    }
}

impl<R: Read + Seek> Read for AlignedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let i = self.slot_for(self.pos)?;
        let slot = &self.slots[i];
        let off = (self.pos - slot.start) as usize;
        if off >= slot.data.len() {
            return Ok(0); // fim dos dados
        }
        let n = buf.len().min(slot.data.len() - off);
        buf[..n].copy_from_slice(&slot.data[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for AlignedReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "seek inválido");
        self.pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).ok_or_else(invalid)?,
            SeekFrom::End(d) => {
                let end = self.inner.seek(SeekFrom::End(0))?;
                end.checked_add_signed(d).ok_or_else(invalid)?
            }
        };
        Ok(self.pos)
    }
}

/// Um `$FILE_NAME` de um registro: um nome em uma pasta pai.
#[derive(Clone, Debug)]
pub(crate) struct MftName {
    pub parent: u64,
    pub name: Box<str>,
    /// Número de sequência esperado do pai; 0 = não conferir.
    pub parent_seq: u16,
    /// Namespace DOS (nome 8.3 que acompanha um nome Win32).
    pub dos_only: bool,
}

/// O que interessa de um file record base em uso. Layout enxuto: num C: são milhões destes
/// vivos ao mesmo tempo, então o primeiro nome fica inline e só hardlinks alocam `more`.
#[derive(Clone, Debug)]
pub(crate) struct MftRecord {
    pub frn: u64,
    /// Tamanho alocado (0 para pastas).
    pub size: u64,
    /// Carimbo NT bruto (100 ns desde 1601); 0 = sem data. Convertido só ao montar a árvore.
    pub mtime: u64,
    /// Primeiro nome (quase todo registro tem um só).
    pub name: MftName,
    /// Nomes extras (hardlinks); vazio = sem alocação.
    pub more: Box<[MftName]>,
    pub seq: u16,
    pub is_dir: bool,
}

impl MftRecord {
    /// Monta o registro descartando já aqui os nomes 8.3 (DOS) quando há nome longo.
    /// `None` se não houver nome nenhum (registro que não dá para pendurar na árvore).
    pub(crate) fn new(
        frn: u64,
        seq: u16,
        is_dir: bool,
        size: u64,
        mtime: u64,
        mut names: Vec<MftName>,
    ) -> Option<MftRecord> {
        if names.iter().any(|n| !n.dos_only) {
            names.retain(|n| !n.dos_only);
        }
        let mut it = names.into_iter();
        let name = it.next()?;
        Some(MftRecord {
            frn,
            size: if is_dir { 0 } else { size },
            mtime,
            name,
            more: it.collect(),
            seq,
            is_dir,
        })
    }

    fn names(&self) -> impl Iterator<Item = &MftName> {
        std::iter::once(&self.name).chain(self.more.iter())
    }

    fn name_mut(&mut self, i: usize) -> &mut Box<str> {
        if i == 0 {
            &mut self.name.name
        } else {
            &mut self.more[i - 1].name
        }
    }
}

fn nt_err(e: NtfsError) -> io::Error {
    io::Error::other(format!("NTFS: {e}"))
}

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "varredura cancelada")
}

/// Carimbo NT (100 ns desde 1601) → `SystemTime`; 0 = sem data.
fn nt_time(ts: u64) -> Option<SystemTime> {
    if ts == 0 {
        return None;
    }
    let to_dur =
        |d: u64| Duration::from_secs(d / 10_000_000) + Duration::from_nanos(d % 10_000_000 * 100);
    if ts >= NT_TO_UNIX {
        SystemTime::UNIX_EPOCH.checked_add(to_dur(ts - NT_TO_UNIX))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(to_dur(NT_TO_UNIX - ts))
    }
}

/// Soma dos data runs não esparsos (runs esparsos não ocupam disco).
fn runs_allocated(v: &NtfsNonResidentAttributeValue) -> ntfs::Result<u64> {
    let mut sum = 0u64;
    for run in v.data_runs() {
        let run = run?;
        if run.data_position().value().is_some() {
            sum += run.allocated_size();
        }
    }
    Ok(sum)
}

/// `$DATA` espalhado em vários registros: percorre a `$ATTRIBUTE_LIST` do registro base e
/// soma os runs de cada pedaço (extent) do stream `stream`.
fn list_allocated<R: Read + Seek>(file: &NtfsFile, fs: &mut R, stream: &str) -> ntfs::Result<u64> {
    let mut sum = 0u64;
    for attr in file.attributes_raw() {
        let attr = attr?;
        if attr.ty()? != NtfsAttributeType::AttributeList {
            continue;
        }
        let list = attr.structured_value::<_, NtfsAttributeList>(fs)?;
        let mut entries = list.entries();
        while let Some(entry) = entries.next(fs) {
            let entry = entry?;
            if entry.ty()? != NtfsAttributeType::Data || entry.name().to_string_lossy() != stream {
                continue;
            }
            let extent_file = entry.to_file(file.ntfs(), fs)?;
            let extent = entry.to_attribute(&extent_file)?;
            if extent.is_resident() {
                continue;
            }
            if let NtfsAttributeValue::NonResident(v) = extent.value(fs)? {
                sum += runs_allocated(&v)?;
            }
        }
    }
    Ok(sum)
}

/// Bytes ocupados no disco pelo atributo `$DATA` `attr` (stream `stream`) de `file`.
fn allocated<R: Read + Seek>(
    file: &NtfsFile,
    attr: &NtfsAttribute,
    fs: &mut R,
    stream: &str,
) -> ntfs::Result<u64> {
    if attr.is_resident() {
        return Ok(0);
    }
    match attr.value(fs)? {
        NtfsAttributeValue::NonResident(v) => runs_allocated(&v),
        NtfsAttributeValue::AttributeListNonResident(_) => list_allocated(file, fs, stream),
        NtfsAttributeValue::Resident(_) => Ok(0),
    }
}

/// Extrai um [`MftRecord`]; `None` para registros de extensão.
fn parse_record<R: Read + Seek>(file: &NtfsFile, fs: &mut R) -> ntfs::Result<Option<MftRecord>> {
    let mut has_si = false;
    let mut mtime = 0;
    let mut size = 0u64;
    let mut names = Vec::new();
    let mut iter = file.attributes();
    while let Some(item) = iter.next(fs) {
        let item = item?;
        let attr = item.to_attribute()?;
        match attr.ty()? {
            NtfsAttributeType::StandardInformation => {
                has_si = true;
                let si = attr.resident_structured_value::<NtfsStandardInformation>()?;
                mtime = si.modification_time().nt_timestamp();
            }
            NtfsAttributeType::FileName => {
                let f = attr.structured_value::<_, NtfsFileName>(fs)?;
                let parent = f.parent_directory_reference();
                names.push(MftName {
                    parent: parent.file_record_number(),
                    parent_seq: parent.sequence_number(),
                    name: f.name().to_string_lossy().into_boxed_str(),
                    dos_only: f.namespace() == NtfsFileNamespace::Dos,
                });
            }
            NtfsAttributeType::Data => {
                let stream = attr.name()?.to_string_lossy();
                if stream.is_empty() || stream == "WofCompressedData" {
                    size += allocated(file, &attr, fs, &stream)?;
                }
            }
            _ => {}
        }
    }
    if !has_si {
        return Ok(None);
    }
    Ok(MftRecord::new(
        file.file_record_number(),
        file.sequence_number(),
        file.is_directory(),
        size,
        mtime,
        names,
    ))
}

/// Tamanho do `$DATA` da `$MFT` e o seu `$BITMAP` (registros em uso), se legível.
fn mft_layout<R: Read + Seek>(mft: &NtfsFile, fs: &mut R) -> ntfs::Result<(u64, Option<Vec<u8>>)> {
    let mut len = None;
    let mut bitmap = None;
    let mut iter = mft.attributes();
    while let Some(item) = iter.next(fs) {
        let item = item?;
        let attr = item.to_attribute()?;
        if !attr.name()?.is_empty() {
            continue;
        }
        match attr.ty()? {
            NtfsAttributeType::Data if len.is_none() => len = Some(attr.value_length()),
            NtfsAttributeType::Bitmap if bitmap.is_none() => {
                let mut value = attr.value(fs)?;
                let mut bytes = vec![0u8; value.len() as usize];
                let mut done = 0;
                while done < bytes.len() {
                    let n = value.read(fs, &mut bytes[done..])?;
                    if n == 0 {
                        break;
                    }
                    done += n;
                }
                bytes.truncate(done);
                bitmap = Some(bytes);
            }
            _ => {}
        }
    }
    let len = len.ok_or(NtfsError::AttributeNotFound {
        position: mft.position(),
        ty: NtfsAttributeType::Data,
    })?;
    Ok((len, bitmap))
}

/// Percorre todos os file records. Erros em registros comuns só pulam o registro; erro na
/// `$MFT` ou na raiz aborta (o chamador cai para a varredura de pastas).
pub(crate) fn read_records<R: Read + Seek>(
    fs: &mut R,
    on_progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
    stride: u64,
) -> io::Result<Vec<MftRecord>> {
    let stride = stride.max(1);
    let ntfs = ntfs::Ntfs::new(fs).map_err(nt_err)?;
    let record_size = u64::from(ntfs.file_record_size()).max(1);
    let mft = ntfs.file(fs, 0).map_err(nt_err)?;
    let (mft_len, bitmap) = mft_layout(&mft, fs).map_err(nt_err)?;
    drop(mft);
    let total = mft_len / record_size;
    let in_use = |frn: u64| {
        bitmap.as_ref().is_none_or(|bm| {
            bm.get((frn / 8) as usize)
                .is_some_and(|b| b & (1 << (frn % 8)) != 0)
        })
    };
    // Capacidade exata pelo bitmap (registros em uso), para não sobrar memória nem realocar.
    let expected = bitmap.as_ref().map_or(total.min(1 << 20), |bm| {
        bm.iter().map(|b| u64::from(b.count_ones())).sum::<u64>()
    });
    let mut out = Vec::with_capacity(expected.min(total) as usize);
    let (mut files, mut bytes) = (0u64, 0u64);
    for frn in 0..total {
        if cancel.load(Ordering::Relaxed) {
            return Err(interrupted());
        }
        if frn > 0 && frn % stride == 0 {
            on_progress(files, bytes);
        }
        let essential = frn == 0 || frn == ROOT_FRN;
        if !essential && !in_use(frn) {
            continue;
        }
        let file = match ntfs.file(fs, frn) {
            Ok(f) => f,
            Err(e) if essential => return Err(nt_err(e)),
            Err(_) => continue,
        };
        if !file.flags().contains(NtfsFileFlags::IN_USE) {
            continue;
        }
        let rec = match parse_record(&file, fs) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) if essential => return Err(nt_err(e)),
            Err(_) => continue,
        };
        if !rec.is_dir {
            files += 1;
            bytes += rec.size;
        }
        out.push(rec);
    }
    if !out.iter().any(|r| r.frn == ROOT_FRN && r.is_dir) {
        return Err(io::Error::other("NTFS: pasta raiz não encontrada"));
    }
    Ok(out)
}

/// Uma aresta pasta pai → (registro, índice do nome). 12 bytes por nome.
struct Link {
    parent: u32,
    rec: u32,
    name: u32,
}

/// Monta a árvore (finalizada) a partir dos registros, consumindo-os: os nomes saem dos
/// registros conforme entram na árvore, e tudo é liberado antes de `finalize`. Sem mapas
/// por frn: os registros ficam ordenados por frn (busca binária) e as arestas num vetor
/// ordenado por pai. Ver regras no topo do módulo.
///
/// A cada [`CANCEL_EVERY`] registros/nós chama `heartbeat` (o chamador reenvia o último
/// progresso; se ninguém mais escuta, isso liga `cancel`) e confere `cancel`
/// (`ErrorKind::Interrupted`): uma montagem cancelada não fica rodando junto com a próxima.
pub(crate) fn build_tree(
    root_path: PathBuf,
    mut records: Vec<MftRecord>,
    cancel: &AtomicBool,
    heartbeat: &mut dyn FnMut(),
) -> io::Result<Tree> {
    let mut check = |i: usize| {
        if !i.is_multiple_of(CANCEL_EVERY) {
            return Ok(());
        }
        heartbeat();
        if cancel.load(Ordering::Relaxed) {
            Err(interrupted())
        } else {
            Ok(())
        }
    };
    check(0)?;
    if !records.is_sorted_by_key(|r| r.frn) {
        records.sort_unstable_by_key(|r| r.frn);
    }
    let find = |recs: &[MftRecord], frn: u64| recs.binary_search_by_key(&frn, |r| r.frn).ok();
    // Sem registro raiz (só em testes sintéticos), u32::MAX faz o papel dela.
    let root = find(&records, ROOT_FRN).map_or(u32::MAX, |i| i as u32);
    let mut links: Vec<Link> = Vec::with_capacity(records.len() + records.len() / 16);
    for (ri, rec) in records.iter().enumerate() {
        check(ri)?;
        if rec.frn == ROOT_FRN {
            continue;
        }
        for (ni, n) in rec.names().enumerate() {
            if rec.frn < FIRST_USER_FRN
                && n.parent == ROOT_FRN
                && n.name.starts_with('$')
                && &*n.name != "$MFT"
            {
                continue; // metafile na raiz
            }
            let parent = if n.parent == rec.frn {
                None
            } else {
                find(&records, n.parent)
            }
            .filter(|&pi| {
                let p = &records[pi];
                p.is_dir && (n.parent_seq == 0 || p.seq == n.parent_seq)
            })
            .map_or(root, |pi| pi as u32); // pai inválido: órfão, vai para a raiz
            links.push(Link {
                parent,
                rec: ri as u32,
                name: ni as u32,
            });
        }
    }
    check(0)?;
    links.sort_unstable_by_key(|l| (l.parent, l.rec, l.name));
    check(0)?;

    let mut tree = Tree::new(root_path);
    let mut placed = 0usize;
    let mut dir_placed = vec![false; records.len()];
    let mut queue: VecDeque<(u32, NodeId)> = VecDeque::from([(root, tree.root())]);
    while let Some((p, node)) = queue.pop_front() {
        let lo = links.partition_point(|l| l.parent < p);
        let hi = links.partition_point(|l| l.parent <= p);
        for l in &links[lo..hi] {
            placed += 1;
            check(placed)?;
            let ri = l.rec as usize;
            let rec = &mut records[ri];
            let name = std::mem::take(rec.name_mut(l.name as usize));
            let mtime = nt_time(rec.mtime);
            if rec.is_dir {
                if dir_placed[ri] {
                    continue; // pasta com mais de um nome: só o primeiro
                }
                dir_placed[ri] = true;
                let id = tree.add(node, &name, true, 0, mtime);
                queue.push_back((l.rec, id));
            } else {
                // Hardlinks: o primeiro nome leva o tamanho, os seguintes ficam com 0.
                let size = std::mem::take(&mut rec.size);
                tree.add(node, &name, false, size, mtime);
            }
        }
    }
    drop(links);
    drop(dir_placed);
    drop(records);
    check(0)?;
    tree.finalize();
    tree.shrink();
    Ok(tree)
}

/// De quantos em quantos registros (ou nós) `build_tree` confere o cancelamento.
pub(crate) const CANCEL_EVERY: usize = 64 * 1024;

/// Varre uma imagem NTFS qualquer (volume cru ou arquivo). O último `on_progress` traz os
/// totais da árvore pronta.
pub(crate) fn scan_image<R: Read + Seek>(
    fs: &mut R,
    root_path: PathBuf,
    on_progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
    stride: u64,
) -> io::Result<Tree> {
    let mut last = (0, 0);
    let records = read_records(
        fs,
        &mut |files, bytes| {
            last = (files, bytes);
            on_progress(files, bytes);
        },
        cancel,
        stride,
    )?;
    if cancel.load(Ordering::Relaxed) {
        return Err(interrupted());
    }
    // Durante a montagem, reenviar o último progresso é o que descobre que a tela desistiu.
    let tree = build_tree(root_path, records, cancel, &mut || {
        on_progress(last.0, last.1)
    })?;
    let root = tree.node(tree.root());
    on_progress(root.files, root.size);
    Ok(tree)
}

/// Bytes por setor do volume; 0 se não for possível obter.
fn sector_size(letter: char) -> u64 {
    let root: Vec<u16> = format!("{letter}:\\")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let (mut spc, mut bps) = (0u32, 0u32);
    // SAFETY: `root` é terminado em NUL e os ponteiros de saída são válidos.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            PCWSTR(root.as_ptr()),
            Some(&mut spc),
            Some(&mut bps),
            None,
            None,
        )
    };
    if ok.is_ok() { u64::from(bps) } else { 0 }
}

/// Varre o volume `letter` (NTFS) lendo a MFT direto. Precisa de administrador.
pub fn mft_scan(letter: char, on_progress: &mut dyn FnMut(u64, u64)) -> io::Result<Tree> {
    mft_scan_with(
        letter,
        on_progress,
        &AtomicBool::new(false),
        crate::scan::walk::PROGRESS_STRIDE,
    )
}

/// Como [`mft_scan`], com cancelamento (`ErrorKind::Interrupted`) e passo de progresso.
pub(crate) fn mft_scan_with(
    letter: char,
    on_progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
    stride: u64,
) -> io::Result<Tree> {
    if !letter.is_ascii_alphabetic() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "letra de volume inválida",
        ));
    }
    let letter = letter.to_ascii_uppercase();
    let volume = File::open(format!(r"\\.\{letter}:"))?;
    let bps = sector_size(letter);
    let align = if bps.is_power_of_two() && bps <= CHUNK as u64 {
        bps.max(MIN_ALIGN)
    } else {
        MIN_ALIGN
    };
    let mut reader = AlignedReader::new(volume, align, CHUNK, SMALL, SLOTS);
    scan_image(
        &mut reader,
        PathBuf::from(format!("{letter}:\\")),
        on_progress,
        cancel,
        stride,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    // ---- AlignedReader ----

    /// Leitor que falha se receber leitura/seek desalinhado, como um handle de volume.
    struct Strict {
        data: Vec<u8>,
        pos: u64,
        align: u64,
        reads: usize,
    }

    impl Read for Strict {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            assert_eq!(self.pos % self.align, 0, "offset desalinhado");
            assert_eq!(buf.len() as u64 % self.align, 0, "tamanho desalinhado");
            self.reads += 1;
            let start = (self.pos as usize).min(self.data.len());
            let n = buf.len().min(self.data.len() - start);
            buf[..n].copy_from_slice(&self.data[start..start + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl Seek for Strict {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.pos = match pos {
                SeekFrom::Start(p) => p,
                SeekFrom::End(d) => (self.data.len() as i64 + d) as u64,
                SeekFrom::Current(d) => (self.pos as i64 + d) as u64,
            };
            Ok(self.pos)
        }
    }

    fn strict(len: usize) -> Strict {
        Strict {
            data: (0..len).map(|i| (i % 251) as u8).collect(),
            pos: 0,
            align: 512,
            reads: 0,
        }
    }

    #[test]
    fn aligned_reader_serves_unaligned_reads_with_aligned_io() {
        let src = strict(10_000);
        let expected = src.data.clone();
        let mut r = AlignedReader::new(src, 512, 2048, 512, 2);
        // Leitura que cruza a fronteira entre blocos (2048).
        r.seek(SeekFrom::Start(2000)).unwrap();
        let mut buf = vec![0u8; 100];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(buf, expected[2000..2100]);
        // Seek relativo e leitura pequena desalinhada.
        r.seek(SeekFrom::Current(-1999)).unwrap();
        let mut b3 = [0u8; 3];
        r.read_exact(&mut b3).unwrap();
        assert_eq!(b3, expected[101..104]);
    }

    #[test]
    fn aligned_reader_handles_tail_and_eof() {
        // Tamanho que não é múltiplo do alinhamento: o último bloco volta curto.
        let src = strict(5_000);
        let expected = src.data.clone();
        let mut r = AlignedReader::new(src, 512, 2048, 512, 2);
        r.seek(SeekFrom::Start(4990)).unwrap();
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, expected[4990..]);
        let mut one = [0u8; 1];
        assert_eq!(r.read(&mut one).unwrap(), 0);
    }

    #[test]
    fn aligned_reader_caches_blocks() {
        let mut r = AlignedReader::new(strict(10_000), 512, 2048, 512, 2);
        let mut b = [0u8; 16];
        for _ in 0..5 {
            // Alterna entre dois blocos: com 2 slots, cada bloco é lido do disco uma vez só.
            r.seek(SeekFrom::Start(10)).unwrap();
            r.read_exact(&mut b).unwrap();
            r.seek(SeekFrom::Start(5000)).unwrap();
            r.read_exact(&mut b).unwrap();
        }
        assert_eq!(r.inner.reads, 2);
    }

    #[test]
    fn aligned_reader_uses_big_blocks_when_sequential() {
        let src = strict(10_000);
        let expected = src.data.clone();
        let mut r = AlignedReader::new(src, 512, 2048, 512, 2);
        let mut all = Vec::new();
        let mut piece = [0u8; 100];
        loop {
            let n = r.read(&mut piece).unwrap();
            if n == 0 {
                break;
            }
            all.extend_from_slice(&piece[..n]);
        }
        assert_eq!(all, expected);
        // 1 bloco pequeno (512) + blocos grandes de 2048 para o resto (9.488 bytes = 5 blocos),
        // em vez de 20 blocos pequenos.
        assert!(r.inner.reads <= 7, "leituras: {}", r.inner.reads);
    }

    // ---- build_tree ----

    fn name(parent: u64, n: &str) -> MftName {
        MftName {
            parent,
            parent_seq: 0,
            name: n.into(),
            dos_only: false,
        }
    }

    fn dos(parent: u64, n: &str) -> MftName {
        MftName {
            dos_only: true,
            ..name(parent, n)
        }
    }

    fn rec(frn: u64, is_dir: bool, size: u64, names: Vec<MftName>) -> MftRecord {
        MftRecord::new(frn, 1, is_dir, size, 0, names).unwrap()
    }

    fn root_rec() -> MftRecord {
        rec(5, true, 0, vec![name(5, ".")])
    }

    fn build(records: &[MftRecord]) -> Tree {
        build_tree(
            PathBuf::from("C:\\"),
            records.to_vec(),
            &AtomicBool::new(false),
            &mut || {},
        )
        .unwrap()
    }

    fn size_of(t: &Tree, p: &str) -> Option<u64> {
        t.find(Path::new(p)).map(|id| t.node(id).size)
    }

    #[test]
    fn record_layout_is_lean() {
        // Milhões de registros ficam vivos ao mesmo tempo: o layout inline precisa ser pequeno
        // e um registro com um nome só não pode alocar nada além do próprio nome.
        assert!(
            std::mem::size_of::<MftRecord>() <= 80,
            "{}",
            std::mem::size_of::<MftRecord>()
        );
        assert!(std::mem::size_of::<MftName>() <= 32);
        let r = rec(40, false, 1, vec![name(5, "a.txt")]);
        assert!(r.more.is_empty());
    }

    #[test]
    fn dos_names_dropped_at_parse_time() {
        let r = rec(
            40,
            false,
            1,
            vec![dos(5, "LONGNA~1.TXT"), name(5, "long name.txt")],
        );
        assert_eq!(&*r.name.name, "long name.txt");
        assert!(r.more.is_empty(), "nome 8.3 não fica guardado");
        let h = rec(
            41,
            false,
            1,
            vec![name(5, "a"), dos(5, "A~1"), name(6, "b")],
        );
        assert_eq!(h.names().count(), 2);
        assert!(MftRecord::new(42, 1, false, 0, 0, Vec::new()).is_none());
    }

    #[test]
    fn records_out_of_order_still_build() {
        // A raiz e os pais podem vir depois dos filhos; build_tree ordena por frn.
        let t = build(&[
            rec(42, false, 4096, vec![name(41, "f.txt")]),
            rec(41, true, 0, vec![name(40, "b")]),
            root_rec(),
            rec(40, true, 0, vec![name(5, "a")]),
        ]);
        assert_eq!(size_of(&t, r"C:\a\b\f.txt"), Some(4096));
    }

    #[test]
    fn nested_dirs_and_sizes() {
        let t = build(&[
            root_rec(),
            rec(40, true, 0, vec![name(5, "a")]),
            rec(41, true, 0, vec![name(40, "b")]),
            rec(42, false, 4096, vec![name(41, "f.txt")]),
            rec(43, false, 8192, vec![name(5, "g.bin")]),
        ]);
        assert_eq!(size_of(&t, r"C:\a\b\f.txt"), Some(4096));
        assert_eq!(size_of(&t, r"C:\a"), Some(4096));
        assert_eq!(t.node(t.root()).size, 4096 + 8192);
        assert_eq!(t.node(t.root()).files, 2);
        assert!(t.node(t.find(Path::new(r"C:\a\b")).unwrap()).is_dir);
    }

    #[test]
    fn prefers_win32_name_over_dos() {
        let t = build(&[
            root_rec(),
            rec(
                40,
                false,
                100,
                vec![dos(5, "PROGRA~1.TXT"), name(5, "Programa longo.txt")],
            ),
        ]);
        assert_eq!(size_of(&t, r"C:\Programa longo.txt"), Some(100));
        assert_eq!(size_of(&t, r"C:\PROGRA~1.TXT"), None);
        assert_eq!(t.children(t.root()).len(), 1);
    }

    #[test]
    fn dos_only_name_used_when_nothing_else() {
        let t = build(&[root_rec(), rec(40, false, 7, vec![dos(5, "ONLY.TXT")])]);
        assert_eq!(size_of(&t, r"C:\ONLY.TXT"), Some(7));
    }

    #[test]
    fn hardlink_size_counted_once() {
        let t = build(&[
            root_rec(),
            rec(40, true, 0, vec![name(5, "a")]),
            rec(41, true, 0, vec![name(5, "b")]),
            rec(50, false, 1000, vec![name(40, "x.dll"), name(41, "y.dll")]),
        ]);
        let a = size_of(&t, r"C:\a\x.dll").unwrap();
        let b = size_of(&t, r"C:\b\y.dll").unwrap();
        assert_eq!(a + b, 1000, "tamanho só no primeiro nome");
        assert_eq!(t.node(t.root()).size, 1000);
        assert_eq!(t.node(t.root()).files, 2, "os dois nomes aparecem");
    }

    #[test]
    fn orphans_go_under_root() {
        let mut stale = name(40, "velho.txt");
        stale.parent_seq = 9; // pasta 40 foi reutilizada (seq 1 != 9)
        let t = build(&[
            root_rec(),
            rec(40, true, 0, vec![name(5, "a")]),
            rec(60, false, 10, vec![name(999, "perdido.txt")]), // pai inexistente
            rec(61, false, 20, vec![stale]),
            rec(62, false, 30, vec![name(63, "pai-arquivo.txt")]), // pai não é pasta
            rec(63, false, 0, vec![name(5, "arquivo")]),
        ]);
        assert_eq!(size_of(&t, r"C:\perdido.txt"), Some(10));
        assert_eq!(size_of(&t, r"C:\velho.txt"), Some(20));
        assert_eq!(size_of(&t, r"C:\pai-arquivo.txt"), Some(30));
        assert_eq!(t.node(t.root()).size, 60);
    }

    #[test]
    fn root_metafiles_skipped_except_mft() {
        let t = build(&[
            rec(0, false, 1 << 20, vec![name(5, "$MFT")]),
            rec(2, false, 64 << 20, vec![name(5, "$LogFile")]),
            root_rec(),
            rec(6, false, 1 << 16, vec![name(5, "$Bitmap")]),
            rec(11, true, 0, vec![name(5, "$Extend")]),
            rec(30, false, 5 << 20, vec![name(11, "$UsnJrnl")]),
            rec(40, true, 0, vec![name(5, "Users")]),
            rec(41, false, 3, vec![name(40, "$nao-e-metafile")]),
        ]);
        assert_eq!(size_of(&t, r"C:\$MFT"), Some(1 << 20));
        assert_eq!(size_of(&t, r"C:\$LogFile"), None);
        assert_eq!(size_of(&t, r"C:\$Bitmap"), None);
        assert_eq!(size_of(&t, r"C:\$Extend"), None);
        assert_eq!(
            size_of(&t, r"C:\$UsnJrnl"),
            None,
            "filho de metafile não vira órfão"
        );
        assert_eq!(size_of(&t, r"C:\Users\$nao-e-metafile"), Some(3));
        assert_eq!(t.node(t.root()).size, (1 << 20) + 3);
    }

    #[test]
    fn dollar_folders_that_are_not_metafiles_are_kept() {
        // `$Recycle.Bin`, `$WinREAgent` etc. são pastas comuns (registro >= 24), não metafiles.
        let t = build(&[
            root_rec(),
            rec(40, true, 0, vec![name(5, "$Recycle.Bin")]),
            rec(41, false, 500, vec![name(40, "$R0001.zip")]),
            rec(
                42,
                false,
                7,
                vec![name(5, "$WINRE_BACKUP_PARTITION.MARKER")],
            ),
        ]);
        assert_eq!(size_of(&t, r"C:\$Recycle.Bin\$R0001.zip"), Some(500));
        assert_eq!(size_of(&t, r"C:\$WINRE_BACKUP_PARTITION.MARKER"), Some(7));
    }

    // ---- imagem NTFS real ----

    /// Imagem de 2 MB do crate `ntfs` (MIT/Apache-2.0, testdata/testfs1, criada com mkntfs,
    /// cluster de 512 bytes). Conteúdo: empty-file, file-with-12345 (residente),
    /// 1000-bytes-file, sparse-file (500.005 bytes lógicos, esparso) e many_subdirs/1..512.
    fn fixture() -> Cursor<Vec<u8>> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ntfs-testfs1.img"
        );
        Cursor::new(std::fs::read(path).unwrap())
    }

    fn scan_fixture(progress: &mut dyn FnMut(u64, u64)) -> io::Result<Tree> {
        let mut reader = AlignedReader::new(fixture(), 512, 64 * 1024, 4096, 4);
        scan_image(
            &mut reader,
            PathBuf::from("X:\\"),
            progress,
            &AtomicBool::new(false),
            16,
        )
    }

    #[test]
    fn scans_ntfs_image() {
        let mut calls = Vec::new();
        let t = scan_fixture(&mut |f, b| calls.push((f, b))).unwrap();
        assert_eq!(size_of(&t, r"X:\1000-bytes-file"), Some(1024)); // 2 clusters de 512
        assert_eq!(size_of(&t, r"X:\file-with-12345"), Some(0)); // residente: nada fora da MFT
        assert_eq!(size_of(&t, r"X:\empty-file"), Some(0));
        let sparse = size_of(&t, r"X:\sparse-file").unwrap();
        assert!(sparse > 0 && sparse < 500_000, "esparso: {sparse}");
        // O registro 255 da imagem tem um setor rasgado (falha no fixup do USN): esse registro
        // é pulado e a varredura segue, então aparecem 511 das 512 subpastas.
        let subdirs = t.find(Path::new(r"X:\many_subdirs")).unwrap();
        assert_eq!(t.children(subdirs).len(), 511);
        assert!(t.find(Path::new(r"X:\many_subdirs\512")).is_some());
        assert!(size_of(&t, r"X:\$MFT").unwrap() > 0);
        assert_eq!(size_of(&t, r"X:\$Bitmap"), None);
        assert_eq!(size_of(&t, r"X:\$Extend"), None);
        let empty = t.find(Path::new(r"X:\empty-file")).unwrap();
        assert!(t.node(empty).mtime.is_some());
        // Progresso: chamado durante a leitura, e o último bate com o total da árvore.
        assert!(calls.len() > 1, "esperava progresso intermediário");
        let root = t.node(t.root());
        assert_eq!(calls.last(), Some(&(root.files, root.size)));
    }

    #[test]
    fn image_scan_cancels() {
        let mut reader = AlignedReader::new(fixture(), 512, 64 * 1024, 4096, 4);
        let cancel = AtomicBool::new(false);
        let err = scan_image(
            &mut reader,
            PathBuf::from("X:\\"),
            &mut |_, _| cancel.store(true, Ordering::Relaxed),
            &cancel,
            1,
        )
        .err()
        .expect("deveria cancelar");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn build_tree_honors_cancel() {
        // Sintético: mais registros que o intervalo de checagem, cancelado de antemão.
        let mut records = vec![root_rec(), rec(40, true, 0, vec![name(5, "d")])];
        for i in 0..(CANCEL_EVERY as u64 + 10) {
            records.push(rec(41 + i, false, 1, vec![name(40, &format!("f{i}"))]));
        }
        let root = || PathBuf::from("C:\\");
        let cancel = AtomicBool::new(true);
        let err = build_tree(root(), records.clone(), &cancel, &mut || {})
            .err()
            .expect("deveria cancelar");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);

        // Cancelado no meio (o heartbeat descobre que ninguém escuta na segunda batida).
        let cancel = AtomicBool::new(false);
        let mut beats = 0;
        let err = build_tree(root(), records.clone(), &cancel, &mut || {
            beats += 1;
            if beats == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .err()
        .expect("deveria cancelar no meio");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert_eq!(beats, 2, "parou na batida que viu o cancelamento");

        // Sem cancelar, os mesmos registros montam a árvore (com batidas periódicas).
        let mut beats = 0;
        let t = build_tree(root(), records, &AtomicBool::new(false), &mut || beats += 1).unwrap();
        assert_eq!(t.node(t.root()).files, CANCEL_EVERY as u64 + 10);
        assert!(beats >= 4, "batidas: {beats}");

        // Registros reais da imagem de teste.
        let mut reader = AlignedReader::new(fixture(), 512, 64 * 1024, 4096, 4);
        let recs = read_records(&mut reader, &mut |_, _| {}, &AtomicBool::new(false), 16).unwrap();
        let err = build_tree(
            PathBuf::from("X:\\"),
            recs,
            &AtomicBool::new(true),
            &mut || {},
        )
        .err()
        .expect("deveria cancelar");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn non_ntfs_image_is_error() {
        let mut reader = AlignedReader::new(Cursor::new(vec![0u8; 64 * 1024]), 512, 4096, 512, 2);
        let r = scan_image(
            &mut reader,
            PathBuf::from("X:\\"),
            &mut |_, _| {},
            &AtomicBool::new(false),
            16,
        );
        assert!(r.is_err());
    }
}
