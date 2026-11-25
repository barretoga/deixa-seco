//! Elevação: saber se o processo roda como administrador e se relançar elevado (UAC)
//! para a varredura pela MFT. Também lê `--scan <LETRA>`, que o relançamento passa adiante.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

/// `true` se o token do processo está elevado (administrador com UAC aprovado).
pub fn is_elevated() -> bool {
    let mut token = HANDLE::default();
    // SAFETY: pseudo-handle do processo atual; `token` é um ponteiro de saída válido.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: o buffer é um TOKEN_ELEVATION com o tamanho informado; `token` é válido.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    // SAFETY: `token` foi aberto acima e é fechado uma vez só.
    let _ = unsafe { CloseHandle(token) };
    ok.is_ok() && elevation.TokenIsElevated != 0
}

/// Parâmetros da linha de comando do processo relançado.
fn relaunch_params(scan_letter: Option<char>) -> String {
    match scan_letter {
        Some(c) if c.is_ascii_alphabetic() => format!("--scan {}", c.to_ascii_uppercase()),
        _ => String::new(),
    }
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Relança o executável atual como administrador (`ShellExecuteW` com `runas`), passando
/// `--scan <LETRA>` se houver letra. Se o UAC aceitar, encerra o processo atual e não volta.
/// Se o usuário cancelar o UAC (ou a chamada falhar), devolve `Err` e o processo continua.
pub fn relaunch_elevated(scan_letter: Option<char>) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let file = wide(exe.as_os_str());
    let params = wide(relaunch_params(scan_letter).as_ref());
    let dir = std::env::current_dir().ok().map(|d| wide(d.as_os_str()));
    let dir_ptr = dir.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr()));
    // SAFETY: todas as strings são terminadas em NUL e vivem até o fim da chamada.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            dir_ptr,
            SW_SHOWNORMAL,
        )
    };
    // Valores acima de 32 indicam sucesso.
    if result.0 as isize > 32 {
        std::process::exit(0);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(0) {
        return Err(io::Error::other(format!(
            "não foi possível reabrir como administrador (código {})",
            result.0 as isize
        )));
    }
    Err(err)
}

/// Decide se o app deve pedir admin ao abrir: só quando ainda não está elevado e
/// ninguém passou `--sem-admin` na linha de comando.
pub fn should_ask_elevation<I: IntoIterator<Item = String>>(elevated: bool, args: I) -> bool {
    !elevated && !args.into_iter().any(|a| a == "--sem-admin")
}

/// Letra pedida com `--scan <LETRA>` na linha de comando do processo.
pub fn parse_args() -> Option<char> {
    parse_scan_arg(std::env::args().skip(1))
}

/// Aceita `--scan C`, `--scan C:`, `--scan C:\` e `--scan=C`. Devolve a letra em maiúscula.
pub fn parse_scan_arg<I: IntoIterator<Item = String>>(args: I) -> Option<char> {
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let value = if arg == "--scan" {
            it.next()?
        } else if let Some(v) = arg.strip_prefix("--scan=") {
            v.to_string()
        } else {
            continue;
        };
        let trimmed = value.trim_end_matches(['\\', '/']).trim_end_matches(':');
        let mut chars = trimmed.chars();
        let letter = chars.next()?;
        return (chars.next().is_none() && letter.is_ascii_alphabetic())
            .then(|| letter.to_ascii_uppercase());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn asks_elevation_only_when_needed() {
        assert!(should_ask_elevation(false, args(&[])));
        assert!(should_ask_elevation(false, args(&["--scan", "C"])));
        assert!(!should_ask_elevation(true, args(&[])));
        assert!(!should_ask_elevation(false, args(&["--sem-admin"])));
    }

    #[test]
    fn parses_scan_letter() {
        assert_eq!(parse_scan_arg(args(&["--scan", "C"])), Some('C'));
        assert_eq!(parse_scan_arg(args(&["--scan", "d"])), Some('D'));
        assert_eq!(parse_scan_arg(args(&["--scan", "E:"])), Some('E'));
        assert_eq!(parse_scan_arg(args(&["--scan", "F:\\"])), Some('F'));
        assert_eq!(parse_scan_arg(args(&["--scan=g"])), Some('G'));
        assert_eq!(parse_scan_arg(args(&["outro", "--scan", "C"])), Some('C'));
    }

    #[test]
    fn rejects_bad_scan_args() {
        assert_eq!(parse_scan_arg(args(&[])), None);
        assert_eq!(parse_scan_arg(args(&["--scan"])), None);
        assert_eq!(parse_scan_arg(args(&["--scan", "CD"])), None);
        assert_eq!(parse_scan_arg(args(&["--scan", "1"])), None);
        assert_eq!(parse_scan_arg(args(&["--scan", "ç"])), None);
        assert_eq!(parse_scan_arg(args(&["C"])), None);
    }

    #[test]
    fn relaunch_params_round_trip() {
        assert_eq!(relaunch_params(Some('c')), "--scan C");
        assert_eq!(relaunch_params(None), "");
        assert_eq!(relaunch_params(Some('1')), "");
        let p = relaunch_params(Some('D'));
        assert_eq!(parse_scan_arg(p.split(' ').map(String::from)), Some('D'));
    }

    #[test]
    fn is_elevated_does_not_panic() {
        // Não dá para afirmar o valor (depende do terminal); só que a consulta funciona.
        let _ = is_elevated();
    }
}
