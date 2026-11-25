//! Ações de shell do Windows: abrir no Explorer, terminal e propriedades.
//! Os argumentos são montados por funções puras (testadas); só as funções finais
//! chamam o sistema.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::Win32::UI::Shell::{SHOP_FILEPATH, SHObjectProperties};
use windows::core::PCWSTR;

/// `CREATE_NEW_CONSOLE`: o app (release) não tem console; o PowerShell precisa de um.
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// Argumento bruto do Explorer. Arquivo: `/select,"<caminho>"` (a vírgula e o caminho
/// ficam no mesmo argumento); pasta: o caminho entre aspas.
pub fn explorer_arg(path: &Path, is_dir: bool) -> String {
    if is_dir {
        format!("\"{}\"", path.display())
    } else {
        format!("/select,\"{}\"", path.display())
    }
}

/// Pasta onde abrir o terminal: a própria pasta, ou a pasta que contém o arquivo.
pub fn terminal_dir(path: &Path, is_dir: bool) -> PathBuf {
    if is_dir {
        path.to_path_buf()
    } else {
        path.parent()
            .map_or_else(|| path.to_path_buf(), Path::to_path_buf)
    }
}

/// `wt.exe` na pasta. O caminho vai só em `current_dir`, nunca no texto dos argumentos:
/// nada para escapar (`;` é separador de subcomando do Windows Terminal).
pub fn wt_command(dir: &Path) -> Command {
    let mut c = Command::new("wt.exe");
    c.args(["-d", "."]).current_dir(dir);
    c
}

/// PowerShell na pasta. Também só `current_dir`: aspas tipográficas (U+2018/2019/201A/201B)
/// valem como aspa simples no PowerShell, então o caminho nunca entra num comando.
pub fn powershell_command(dir: &Path) -> Command {
    let mut c = Command::new("powershell.exe");
    c.arg("-NoExit")
        .current_dir(dir)
        .creation_flags(CREATE_NEW_CONSOLE);
    c
}

/// Abre o Explorer com o arquivo selecionado, ou abre a pasta.
pub fn open_in_explorer(path: &Path) -> io::Result<()> {
    let arg = explorer_arg(path, path.is_dir());
    // O explorer.exe devolve código 1 mesmo com sucesso: só o `spawn` importa.
    Command::new("explorer.exe").raw_arg(arg).spawn().map(drop)
}

/// Abre um terminal na pasta: Windows Terminal, ou PowerShell se não estiver instalado.
pub fn open_terminal(dir: &Path) -> io::Result<()> {
    if wt_command(dir).spawn().is_ok() {
        return Ok(());
    }
    powershell_command(dir).spawn().map(drop)
}

/// Abre a janela de propriedades do shell. `false` se o Windows recusou.
pub fn show_properties(path: &Path) -> bool {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` termina em NUL e vive durante a chamada; sem página inicial.
    unsafe { SHObjectProperties(None, SHOP_FILEPATH, PCWSTR(wide.as_ptr()), PCWSTR::null()) }
        .as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_select_keeps_path_in_one_arg() {
        let p = Path::new(r"D:\Fotos de São Paulo\coração.jpg");
        assert_eq!(
            explorer_arg(p, false),
            "/select,\"D:\\Fotos de São Paulo\\coração.jpg\""
        );
    }

    #[test]
    fn explorer_dir_opens_the_folder() {
        let p = Path::new(r"D:\Fotos de São Paulo");
        assert_eq!(explorer_arg(p, true), "\"D:\\Fotos de São Paulo\"");
    }

    #[test]
    fn terminal_dir_of_file_is_parent() {
        assert_eq!(
            terminal_dir(Path::new(r"D:\a b\c.txt"), false),
            PathBuf::from(r"D:\a b")
        );
        assert_eq!(
            terminal_dir(Path::new(r"D:\a b"), true),
            PathBuf::from(r"D:\a b")
        );
    }

    fn args_of(c: &Command) -> Vec<String> {
        c.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn terminal_commands_never_put_the_path_in_arguments() {
        let dir = Path::new("D:\\x\u{2019}; calc; \u{2018}\\a;b 'c'");
        for c in [wt_command(dir), powershell_command(dir)] {
            assert_eq!(c.get_current_dir(), Some(dir));
            let args = args_of(&c);
            assert!(
                args.iter()
                    .all(|a| !a.contains('\u{2019}') && !a.contains("calc")),
                "{args:?}"
            );
        }
        assert_eq!(args_of(&wt_command(dir)), ["-d", "."]);
        assert_eq!(args_of(&powershell_command(dir)), ["-NoExit"]);
    }
}
