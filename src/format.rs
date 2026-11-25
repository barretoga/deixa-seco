//! Formatação pt-BR de tamanhos, contagens e datas.

use std::time::SystemTime;
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

/// Insere `.` como separador de milhar.
fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    out
}

/// Tamanho em base 1024, uma casa decimal a partir de KB (`12,4 GB`).
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{} B", group_thousands(bytes));
    }
    let mut unit = 1;
    let mut div: u128 = 1024;
    while unit < UNITS.len() - 1 && (bytes as u128) >= div * 1024 {
        div *= 1024;
        unit += 1;
    }
    // Décimos arredondados; se arredondar para 1024,0 sobe para a próxima unidade.
    let mut tenths = (bytes as u128 * 10 + div / 2) / div;
    if tenths >= 10_240 && unit < UNITS.len() - 1 {
        unit += 1;
        div *= 1024;
        tenths = (bytes as u128 * 10 + div / 2) / div;
    }
    format!("{},{} {}", tenths / 10, tenths % 10, UNITS[unit])
}

/// Contagem com separador de milhar `.` (`312.004`).
pub fn format_count(n: u64) -> String {
    group_thousands(n)
}

/// Data `dd/mm/aaaa` no fuso local.
pub fn format_date(t: SystemTime) -> String {
    const EPOCH_DIFF_SECS: i128 = 11_644_473_600; // 1601 -> 1970
    let secs: i128 = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i128,
        Err(e) => -(e.duration().as_secs() as i128),
    };
    let ticks = (secs + EPOCH_DIFF_SECS).max(0) as u64 * 10_000_000;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: ponteiros válidos para structs locais.
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err()
            || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err()
        {
            local = utc;
        }
    }
    format!("{:02}/{:02}/{:04}", local.wDay, local.wMonth, local.wYear)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1.023 B");
        assert_eq!(format_bytes(1536), "1,5 KB");
        assert_eq!(format_bytes(13_314_398_618), "12,4 GB");
        assert_eq!(format_bytes(1_099_511_627_776), "1,0 TB");
    }

    #[test]
    fn count() {
        assert_eq!(format_count(312_004), "312.004");
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1.000");
        assert_eq!(format_count(1_234_567_890), "1.234.567.890");
    }

    #[test]
    fn date() {
        // 2026-11-23 12:00:00 UTC: mesma data em qualquer fuso do Brasil.
        let t = UNIX_EPOCH + Duration::from_secs(1_795_435_200);
        assert_eq!(format_date(t), "23/11/2026");
    }
}
