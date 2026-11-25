//! Lista de volumes (letra, rótulo, sistema de arquivos, total, livre).

use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows::core::PCWSTR;

#[derive(Clone, Debug)]
pub struct VolumeInfo {
    pub letter: char,
    pub label: String,
    pub fs: String,
    pub total: u64,
    pub free: u64,
}

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Volumes fixos, removíveis e de rede com mídia presente.
pub fn list_volumes() -> Vec<VolumeInfo> {
    // SAFETY: chamada sem argumentos.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{letter}:\\")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let root = PCWSTR(root.as_ptr());
        // SAFETY: `root` é terminado em NUL e vive até o fim da iteração.
        let kind = unsafe { GetDriveTypeW(root) };
        if !matches!(kind, DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_REMOTE) {
            continue;
        }
        let mut label = [0u16; 261];
        let mut fs = [0u16; 261];
        // SAFETY: buffers e ponteiros válidos durante a chamada.
        let info = unsafe {
            GetVolumeInformationW(root, Some(&mut label), None, None, None, Some(&mut fs))
        };
        if info.is_err() {
            continue; // sem mídia ou inacessível
        }
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: ponteiros de saída válidos.
        let space = unsafe {
            GetDiskFreeSpaceExW(root, Some(&mut avail), Some(&mut total), Some(&mut free))
        };
        if space.is_err() {
            continue;
        }
        out.push(VolumeInfo {
            letter,
            label: from_wide(&label),
            fs: from_wide(&fs),
            total,
            free: avail,
        });
    }
    out
}
