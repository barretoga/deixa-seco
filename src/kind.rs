//! Classificação de arquivos por extensão (define a cor do bloco no mapa).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    Folder,
    Video,
    Image,
    Audio,
    Archive,
    Code,
    Executable,
    Document,
    Cache,
    /// Pacotes de jogos, assets e dados binários (`.pak`, `.vpk`, `.bin`, `data.000`…).
    GameData,
    Other,
}

/// Classifica pelo nome (extensão sem diferenciar maiúsculas de minúsculas). Não aloca: roda
/// uma vez por arquivo da árvore inteira a cada recálculo das cores.
pub fn kind_of(name: &str, is_dir: bool) -> FileKind {
    if is_dir {
        return FileKind::Folder;
    }
    const CACHE_NAMES: [&str; 5] = [
        "thumbs.db",
        "desktop.ini",
        "pagefile.sys",
        "hiberfil.sys",
        "swapfile.sys",
    ];
    if CACHE_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n)) {
        return FileKind::Cache;
    }
    let raw = match name.rfind('.') {
        Some(i) if i + 1 < name.len() => &name[i + 1..],
        _ => return FileKind::Other,
    };
    // Extensão em minúsculas num buffer na pilha (nenhuma conhecida passa de 16 bytes).
    let mut buf = [0u8; 16];
    let Some(lower) = buf.get_mut(..raw.len()) else {
        return FileKind::Other;
    };
    lower.copy_from_slice(raw.as_bytes());
    lower.make_ascii_lowercase();
    let Ok(ext) = std::str::from_utf8(lower) else {
        return FileKind::Other;
    };
    // Volumes numerados de dados de jogo: `data.000`, `data.001`…
    if name.len() == ext.len() + 5
        && name[..5].eq_ignore_ascii_case("data.")
        && ext.bytes().all(|b| b.is_ascii_digit())
    {
        return FileKind::GameData;
    }
    match ext {
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg" => {
            FileKind::Video
        }
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "raw" | "heic" | "heif" | "tif"
        | "tiff" | "svg" | "ico" | "psd" | "cr2" | "nef" | "dng" => FileKind::Image,
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "wma" | "opus" | "aiff" | "mid" => {
            FileKind::Audio
        }
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "iso" | "cab" | "tgz"
        | "img" | "vhd" | "vhdx" | "wim" | "esd" => FileKind::Archive,
        "rs" | "js" | "ts" | "tsx" | "jsx" | "py" | "c" | "cpp" | "cc" | "h" | "hpp" | "cs"
        | "java" | "go" | "json" | "toml" | "yaml" | "yml" | "html" | "htm" | "css" | "scss"
        | "php" | "rb" | "kt" | "swift" | "lua" | "sh" | "ps1" | "bat" | "cmd" | "sql" | "xml"
        | "vue" | "dart" | "lock" => FileKind::Code,
        "exe" | "dll" | "msi" | "sys" | "ocx" | "scr" | "drv" | "cpl" | "msp" | "msix" | "appx" => {
            FileKind::Executable
        }
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf" | "odt"
        | "ods" | "odp" | "csv" | "epub" | "mobi" => FileKind::Document,
        "tmp" | "temp" | "cache" | "log" | "bak" | "old" | "dmp" | "etl" | "crdownload"
        | "part" => FileKind::Cache,
        "pak" | "ucas" | "utoc" | "vpk" | "bundle" | "assets" | "arc" | "dat" | "bin" | "data"
        | "wem" | "bnk" | "pck" | "forge" | "big" | "rpf" | "cpk" | "wad" | "sav" => {
            FileKind::GameData
        }
        _ => FileKind::Other,
    }
}

/// Todos os tipos, na ordem da declaração de `FileKind` (`kind as usize` indexa aqui).
pub const ALL_KINDS: [FileKind; 11] = [
    FileKind::Folder,
    FileKind::Video,
    FileKind::Image,
    FileKind::Audio,
    FileKind::Archive,
    FileKind::Code,
    FileKind::Executable,
    FileKind::Document,
    FileKind::Cache,
    FileKind::GameData,
    FileKind::Other,
];

/// Bytes por tipo (índice `kind as usize`).
pub type KindHist = [u64; ALL_KINDS.len()];

/// Tipo com mais bytes no histograma; sem bytes, `Folder`.
pub fn dominant_in(h: &KindHist) -> FileKind {
    let mut best = 0;
    for i in 1..h.len() {
        if h[i] > h[best] {
            best = i;
        }
    }
    if h[best] == 0 {
        FileKind::Folder
    } else {
        ALL_KINDS[best]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_of_basic() {
        assert_eq!(kind_of("a.mp4", false), FileKind::Video);
        assert_eq!(kind_of("a.RS", false), FileKind::Code);
        assert_eq!(kind_of("setup.exe", false), FileKind::Executable);
    }

    #[test]
    fn kind_of_misc() {
        assert_eq!(kind_of("whatever.mp4", true), FileKind::Folder);
        assert_eq!(kind_of("Foto.JPEG", false), FileKind::Image);
        assert_eq!(kind_of("x.tar.gz", false), FileKind::Archive);
        assert_eq!(kind_of("app.log", false), FileKind::Cache);
        assert_eq!(kind_of("README", false), FileKind::Other);
        assert_eq!(kind_of("end.", false), FileKind::Other);
        assert_eq!(kind_of("música.mp3", false), FileKind::Audio);
        assert_eq!(kind_of("relatório.PDF", false), FileKind::Document);
    }

    #[test]
    fn kind_of_game_data() {
        for name in [
            "pakchunk0-Windows.pak",
            "global.ucas",
            "global.utoc",
            "pak01_dir.vpk",
            "level1.bundle",
            "sharedassets0.assets",
            "x.arc",
            "save.DAT",
            "blob.bin",
            "game.data",
            "Init.wem",
            "SFX.BNK",
            "res.pck",
            "DataPC.forge",
            "a.big",
            "x64a.rpf",
            "movie.cpk",
            "DOOM.WAD",
            "slot1.sav",
            "data.000",
            "DATA.012",
        ] {
            assert_eq!(kind_of(name, false), FileKind::GameData, "{name}");
        }
        assert_eq!(kind_of("data.000", true), FileKind::Folder);
        assert_eq!(kind_of("data.abc", false), FileKind::Other);
        assert_eq!(kind_of("outro.000", false), FileKind::Other);
        assert_eq!(kind_of("data.", false), FileKind::Other);
    }
}
