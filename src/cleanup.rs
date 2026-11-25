//! Limpeza: proteção de caminhos, plano (puro) e execução.
//!
//! Regras de segurança:
//! - `is_protected` compara caminhos normalizados (sem `\\?\`, `.`/`..` resolvidos, sem
//!   separador no fim, sem maiúsculas). Raiz de volume, Windows, `Program Files*` e
//!   `ProgramData` (em qualquer unidade) são protegidos com tudo dentro, exceto o que estiver
//!   dentro de um caminho `allowed`. O perfil e as pastas dele (Desktop, Documents…) são
//!   protegidos inteiros, mas o conteúdo pode sair. Ancestrais de qualquer coisa protegida
//!   também são protegidos. Caminho relativo é sempre protegido.
//! - A execução verifica de novo no caminho real (pasta-pai resolvida, nomes curtos 8.3
//!   expandidos), então uma junção não leva a apagar algo protegido.
//! - Apagar nunca segue reparse points: junção/symlink é removido como link.
//! - Erro num item vai para `skipped` e o lote continua.
//! - Reparse points que não são links (placeholders do OneDrive/nuvem, dedup…) nunca são
//!   apagados nem percorridos: apagar um placeholder pode apagar o arquivo na nuvem.
//!
//! Limitação conhecida: `delete_tree` trabalha por caminho, não por handle. Entre verificar um
//! item (`symlink_metadata`) e apagá-lo, outro processo pode trocar uma pasta por uma junção.
//! O efeito fica restrito (`RemoveDirectoryW` só apaga pasta vazia ou o próprio link, e o
//! conteúdo é listado de novo a cada nível), mas a janela de corrida existe.

use crate::insights::Env;
use crate::insights::known::allowed_system_paths;
use std::fs;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::MetadataExt;
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::Command;
use std::time::{Duration, Instant};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAGS_AND_ATTRIBUTES, FindClose, FindFirstFileW, GetDiskFreeSpaceExW, GetLongPathNameW,
    GetVolumeNameForVolumeMountPointW, SetFileAttributesW, WIN32_FIND_DATAW,
};
use windows::Win32::UI::Shell::{
    SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHEmptyRecycleBinW,
};
use windows::core::PCWSTR;

const ATTR_READONLY: u32 = 0x1;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE_POINT: u32 = 0x400;

const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const PROTECTED_REASON: &str = "caminho protegido";
const CLOUD_REASON: &str = "arquivo na nuvem (OneDrive) — não mexi";
const FOREIGN_REPARSE_REASON: &str = "ponto de reparse desconhecido — não mexi";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    DeletePermanent(PathBuf),
    /// Apaga cada filho; a pasta fica.
    DeleteContents(PathBuf),
    Recycle(PathBuf),
    EmptyRecycleBin,
    /// `command` é o `UninstallString` do registro, já uma linha de comando completa.
    Uninstall {
        name: String,
        command: String,
    },
    /// App da Microsoft Store do usuário atual: `PackageManager.RemovePackageAsync(full_name)`.
    RemoveStorePackage {
        name: String,
        full_name: String,
    },
    /// Compacta um disco virtual do WSL/Docker com o `diskpart` (precisa de admin). Desliga o
    /// WSL antes de cada compactação. Não apaga dados.
    CompactVhdx {
        path: PathBuf,
        /// Distro do WSL dona do disco (`fstrim` antes de compactar); `None` para o Docker.
        distro: Option<String>,
    },
    /// `docker system prune -a -f` (sem `--volumes`).
    DockerPrune,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanupPlan {
    pub ops: Vec<Op>,
    pub bytes_now: u64,
    pub bytes_to_bin: u64,
    /// Itens `Recycle` maiores que a Lixeira: continuam em `ops`, mas o Windows os apagará de
    /// vez; a UI avisa.
    pub too_big_for_bin: Vec<PathBuf>,
    pub rejected: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    /// Bytes realmente removidos (soma dos arquivos apagados, não o tamanho planejado).
    pub freed_now: u64,
    pub sent_to_bin: u64,
    /// Itens que não saíram, com o motivo em pt-BR.
    pub skipped: Vec<(PathBuf, String)>,
    /// Caminhos removidos. Para `EmptyRecycleBin` entra `Lixeira`; para `Uninstall` e
    /// `RemoveStorePackage`, o nome; para `CompactVhdx`, o disco compactado; para
    /// `DockerPrune`, `Docker`.
    pub done: Vec<PathBuf>,
    /// Observações em pt-BR para o resumo (ex.: o Docker não disse quanto liberou).
    pub notes: Vec<String>,
}

/// Nota do resumo quando o prune deu certo mas o Docker não imprimiu o total.
pub const DOCKER_UNKNOWN_NOTE: &str = "Docker limpo (o Docker não informou quanto liberou)";

// ---------------------------------------------------------------------------------------------
// Normalização

/// Caminho absoluto normalizado para comparação: unidade/compartilhamento e componentes em
/// minúsculas, sem `.`/`..`, sem pontos/espaços no fim de cada nome.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Norm {
    prefix: String,
    comps: Vec<String>,
}

impl Norm {
    /// `self` está dentro de `other` ou é igual a ele.
    fn starts_with(&self, other: &Norm) -> bool {
        self.prefix == other.prefix
            && self.comps.len() >= other.comps.len()
            && self.comps[..other.comps.len()] == other.comps[..]
    }

    fn strictly_under(&self, other: &Norm) -> bool {
        self.starts_with(other) && self.comps.len() > other.comps.len()
    }

    fn join(&self, names: &[&str]) -> Norm {
        let mut n = self.clone();
        n.comps.extend(names.iter().map(|s| s.to_lowercase()));
        n
    }
}

struct Parsed {
    norm: Norm,
    /// O mesmo caminho em forma `\\?\` (caixa original), para as chamadas de execução.
    verbatim: PathBuf,
}

/// `None` = não dá para tratar com segurança (relativo, dispositivo, `:` num nome…).
fn parse(path: &Path) -> Option<Parsed> {
    let s = path.as_os_str().to_string_lossy().replace('/', "\\");
    let s = if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{r}")
    } else if let Some(r) = s.strip_prefix(r"\\?\") {
        r.to_string()
    } else {
        s
    };
    let mut prefix: Option<(String, String)> = None; // (comparação, verbatim)
    let mut rooted = false;
    let mut comps: Vec<String> = Vec::new();
    let mut orig: Vec<String> = Vec::new();
    for c in Path::new(&s).components() {
        match c {
            Component::Prefix(px) => {
                prefix = Some(match px.kind() {
                    Prefix::Disk(l) => {
                        let l = (l as char).to_ascii_uppercase();
                        (format!("{}:", l.to_ascii_lowercase()), format!(r"\\?\{l}:"))
                    }
                    Prefix::UNC(server, share) => {
                        let (server, share) = (server.to_string_lossy(), share.to_string_lossy());
                        (
                            format!(r"\\{}\{}", server.to_lowercase(), share.to_lowercase()),
                            format!(r"\\?\UNC\{server}\{share}"),
                        )
                    }
                    _ => return None,
                });
            }
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                comps.pop();
                orig.pop();
            }
            Component::Normal(n) => {
                let t = n.to_string_lossy();
                if t.contains(':') {
                    return None;
                }
                let trimmed = t.trim_end_matches(['.', ' ']);
                if trimmed.is_empty() {
                    return None;
                }
                comps.push(trimmed.to_lowercase());
                orig.push(t.into_owned());
            }
        }
    }
    let (prefix, vprefix) = prefix?;
    if !rooted {
        return None;
    }
    let mut v = format!("{vprefix}\\");
    v.push_str(&orig.join("\\"));
    Some(Parsed {
        norm: Norm { prefix, comps },
        verbatim: PathBuf::from(v),
    })
}

fn norm(path: &Path) -> Option<Norm> {
    parse(path).map(|p| p.norm)
}

// ---------------------------------------------------------------------------------------------
// Proteção

/// O que a proteção precisa saber: caminhos liberados e onde ficam o perfil e o Windows.
#[derive(Clone, Debug)]
pub struct ProtectCtx {
    allowed: Vec<Norm>,
    windows: Option<Norm>,
    profiles: Vec<Norm>,
}

/// Pastas do perfil protegidas inteiras (o conteúdo pode sair).
const PROFILE_FOLDERS: &[&[&str]] = &[
    &["desktop"],
    &["documents"],
    &["downloads"],
    &["pictures"],
    &["music"],
    &["videos"],
    &["appdata"],
    &["appdata", "local"],
    &["appdata", "roaming"],
];

/// Pastas protegidas com tudo dentro, em qualquer unidade.
const SYSTEM_ROOTS: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "$recycle.bin",
    "system volume information",
    "bootmgr",
    "bootnxt",
    "boot",
    "efi",
    "recovery",
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
];

impl ProtectCtx {
    /// `allowed` explícito; perfil e Windows vêm de `env`.
    pub fn from_parts(allowed: &[PathBuf], env: &Env) -> ProtectCtx {
        ProtectCtx {
            allowed: allowed.iter().filter_map(|p| norm(p)).collect(),
            windows: norm(&env.windows),
            profiles: norm(&env.user_profile).into_iter().collect(),
        }
    }

    /// `allowed` = caminhos de sistema das categorias Seguro.
    pub fn from_env(env: &Env) -> ProtectCtx {
        Self::from_parts(&allowed_system_paths(env), env)
    }

    /// Também protege o `USERPROFILE` real, caso o perfil de `Env` tenha sido trocado
    /// (`DEIXA_SECO_PROFILE` em debug).
    pub fn from_system() -> ProtectCtx {
        Self::from_env(&Env::from_system()).with_real_profile()
    }

    fn with_real_profile(mut self) -> ProtectCtx {
        if let Some(real) = std::env::var_os("USERPROFILE").and_then(|p| norm(Path::new(&p)))
            && !self.profiles.contains(&real)
        {
            self.profiles.push(real);
        }
        self
    }

    /// Raízes protegidas com tudo dentro, na unidade de `n`.
    fn hard_roots(&self, n: &Norm) -> Vec<Norm> {
        let root = Norm {
            prefix: n.prefix.clone(),
            comps: Vec::new(),
        };
        let mut v: Vec<Norm> = SYSTEM_ROOTS.iter().map(|r| root.join(&[r])).collect();
        v.extend(self.windows.iter().filter(|w| !w.comps.is_empty()).cloned());
        v
    }

    /// Pastas protegidas só por inteiro.
    fn whole_only(&self) -> Vec<Norm> {
        let mut v = Vec::new();
        for profile in self.profiles.iter().filter(|p| !p.comps.is_empty()) {
            v.push(profile.clone());
            v.extend(PROFILE_FOLDERS.iter().map(|f| profile.join(f)));
        }
        v
    }

    /// `n` está dentro (estritamente, ou igual se `or_equal`) de um `allowed` válido. Um
    /// `allowed` que é igual a uma raiz protegida (ou a contém) não vale.
    fn inside_allowed(&self, n: &Norm, hard: &[Norm], or_equal: bool) -> bool {
        self.allowed.iter().any(|a| {
            let valid = !hard.iter().any(|r| r.starts_with(a));
            valid
                && if or_equal {
                    n.starts_with(a)
                } else {
                    n.strictly_under(a)
                }
        })
    }

    /// `true` = não pode ir para a Lixeira nem ser apagado de vez.
    pub fn is_protected(&self, path: &Path) -> bool {
        let Some(n) = norm(path) else {
            return true;
        };
        if n.comps.is_empty() {
            return true;
        }
        let hard = self.hard_roots(&n);
        // Igual a algo protegido, ou ancestral dele.
        if hard
            .iter()
            .chain(self.whole_only().iter())
            .any(|r| r.starts_with(&n))
        {
            return true;
        }
        hard.iter().any(|r| n.starts_with(r)) && !self.inside_allowed(&n, &hard, false)
    }

    /// `true` = não pode ter o conteúdo apagado (`DeleteContents`). A pasta em si fica, então
    /// pastas do perfil e `allowed` servem; ancestrais de algo protegido, não.
    pub fn is_protected_container(&self, dir: &Path) -> bool {
        let Some(n) = norm(dir) else {
            return true;
        };
        let hard = self.hard_roots(&n);
        if n.comps.is_empty()
            || hard
                .iter()
                .chain(self.whole_only().iter())
                .any(|r| r.strictly_under(&n))
        {
            return true;
        }
        hard.iter().any(|r| n.starts_with(r)) && !self.inside_allowed(&n, &hard, true)
    }
}

/// Atalho com o perfil e o Windows reais deste computador.
pub fn is_protected(path: &Path, allowed: &[PathBuf]) -> bool {
    ProtectCtx::from_parts(allowed, &Env::from_system())
        .with_real_profile()
        .is_protected(path)
}

// ---------------------------------------------------------------------------------------------
// Plano

/// Separa o que pode do que é protegido e soma os bytes. Puro, exceto `CompactVhdx`, que
/// confere se o arquivo existe.
pub fn plan(ops: Vec<(Op, u64)>, allowed: &[PathBuf], bin_max_bytes: u64) -> CleanupPlan {
    plan_with(
        ops,
        &ProtectCtx::from_parts(allowed, &Env::from_system()).with_real_profile(),
        bin_max_bytes,
    )
}

pub fn plan_with(ops: Vec<(Op, u64)>, ctx: &ProtectCtx, bin_max_bytes: u64) -> CleanupPlan {
    let mut out = CleanupPlan::default();
    for (op, size) in ops {
        let ok = match &op {
            Op::DeletePermanent(p) | Op::Recycle(p) => {
                if ctx.is_protected(p) {
                    out.rejected.push(p.clone());
                    false
                } else {
                    true
                }
            }
            Op::DeleteContents(p) => {
                if ctx.is_protected_container(p) {
                    out.rejected.push(p.clone());
                    false
                } else {
                    true
                }
            }
            Op::CompactVhdx { path, .. } => {
                if check_vhdx_path(ctx, path).is_err() || !path.is_file() {
                    out.rejected.push(path.clone());
                    false
                } else {
                    true
                }
            }
            // Sem caminho a proteger: o Windows (ou o Docker) decide o que sai.
            Op::EmptyRecycleBin
            | Op::Uninstall { .. }
            | Op::RemoveStorePackage { .. }
            | Op::DockerPrune => true,
        };
        if !ok {
            continue;
        }
        match &op {
            Op::Recycle(p) => {
                out.bytes_to_bin += size;
                if size > bin_max_bytes {
                    out.too_big_for_bin.push(p.clone());
                }
            }
            // Quanto a compactação libera só se sabe no fim (a UI mostra "estimado").
            Op::CompactVhdx { .. } => {}
            _ => out.bytes_now += size,
        }
        out.ops.push(op);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Execução

/// Executa o plano (chame fora da thread da UI). `on_progress(feitas, total)` a cada operação.
/// A proteção é verificada de novo com o ambiente real, no caminho resolvido.
pub fn execute(plan: &CleanupPlan, on_progress: &mut dyn FnMut(usize, usize)) -> CleanupReport {
    execute_with(plan, &ProtectCtx::from_system(), on_progress)
}

pub fn execute_with(
    plan: &CleanupPlan,
    ctx: &ProtectCtx,
    on_progress: &mut dyn FnMut(usize, usize),
) -> CleanupReport {
    let mut r = CleanupReport::default();
    let total = plan.ops.len();
    on_progress(0, total);
    for (i, op) in plan.ops.iter().enumerate() {
        run_op(op, ctx, &mut r);
        on_progress(i + 1, total);
    }
    r
}

fn run_op(op: &Op, ctx: &ProtectCtx, r: &mut CleanupReport) {
    match op {
        Op::DeletePermanent(p) => match resolve_item(ctx, p) {
            Err(why) => r.skipped.push((p.clone(), why)),
            Ok(real) => {
                let mut fails = Vec::new();
                r.freed_now += delete_tree(&real, p, &mut fails);
                if fails.is_empty() {
                    r.done.push(p.clone());
                } else {
                    r.skipped.extend(fails);
                }
            }
        },
        Op::DeleteContents(dir) => match resolve_container(ctx, dir) {
            Err(why) => r.skipped.push((dir.clone(), why)),
            Ok(real) => match fs::read_dir(&real) {
                Err(e) => r.skipped.push((dir.clone(), reason(&e))),
                Ok(entries) => {
                    for entry in entries {
                        let entry = match entry {
                            Ok(e) => e,
                            Err(e) => {
                                r.skipped.push((dir.clone(), reason(&e)));
                                continue;
                            }
                        };
                        let shown = dir.join(entry.file_name());
                        let mut fails = Vec::new();
                        r.freed_now += delete_tree(&entry.path(), &shown, &mut fails);
                        if fails.is_empty() {
                            r.done.push(shown);
                        } else {
                            r.skipped.extend(fails);
                        }
                    }
                }
            },
        },
        Op::Recycle(p) => match resolve_item(ctx, p) {
            Err(why) => r.skipped.push((p.clone(), why)),
            Ok(real) => {
                let (size, foreign) = measure(&real);
                if let Some(why) = foreign {
                    // O shell levaria junto placeholders da nuvem: não mexe no item.
                    r.skipped.push((p.clone(), why.to_string()));
                    return;
                }
                // Sem plano B: se a Lixeira falhar, o item fica (nunca apaga de vez).
                match trash::delete(&real) {
                    Ok(()) => {
                        r.sent_to_bin += size;
                        r.done.push(p.clone());
                    }
                    Err(e) => r.skipped.push((p.clone(), trash_reason(&e))),
                }
            }
        },
        Op::EmptyRecycleBin => {
            let before = recycle_bin_size();
            match empty_recycle_bin() {
                Ok(()) => {
                    r.freed_now += before;
                    r.done.push(PathBuf::from("Lixeira"));
                }
                // Lixeira já vazia: o shell às vezes devolve erro; não é falha.
                Err(_) if before == 0 => r.done.push(PathBuf::from("Lixeira")),
                Err(e) => r.skipped.push((PathBuf::from("Lixeira"), reason(&e))),
            }
        }
        Op::Uninstall { name, command } => match run_uninstaller(command) {
            Ok(()) => r.done.push(PathBuf::from(name)),
            Err(why) => r.skipped.push((PathBuf::from(name), why)),
        },
        Op::RemoveStorePackage { name, full_name } => {
            match crate::insights::store::remove_package(full_name) {
                Ok(()) => r.done.push(PathBuf::from(name)),
                Err(why) => r.skipped.push((PathBuf::from(name), why)),
            }
        }
        Op::CompactVhdx { path, distro } => match compact_vhdx(ctx, path, distro.as_deref()) {
            Ok((freed, note)) => {
                r.freed_now += freed;
                match note {
                    Some(why) => r.skipped.push((path.clone(), why)),
                    None => r.done.push(path.clone()),
                }
            }
            Err(why) => r.skipped.push((path.clone(), why)),
        },
        Op::DockerPrune => record_prune(r, docker_prune()),
    }
}

/// `cmd /S /C "<UninstallString>"`: com `/S`, o cmd tira só as aspas de fora que nós pusemos,
/// e a linha do registro chega intacta mesmo com várias aspas. Sem janela de console (a
/// interface do próprio desinstalador continua aparecendo).
fn uninstall_command(command: &str) -> Command {
    let mut c = Command::new("cmd");
    c.raw_arg(format!("/S /C \"{command}\""))
        .creation_flags(CREATE_NO_WINDOW);
    c
}

/// Roda exatamente o `UninstallString` e espera.
fn run_uninstaller(command: &str) -> Result<(), String> {
    let status = uninstall_command(command)
        .status()
        .map_err(|e| reason(&e))?;
    match status.code() {
        Some(0) => Ok(()),
        Some(n) => Err(format!("desinstalador retornou {n}")),
        None => Err("desinstalador terminou sem código".to_string()),
    }
}

/// Caminho real de um item a remover inteiro: pasta-pai resolvida (junções seguidas, como o
/// sistema faria), o próprio item não (se for link, sai o link), nome curto expandido.
fn resolve_item(ctx: &ProtectCtx, p: &Path) -> Result<PathBuf, String> {
    if ctx.is_protected(p) {
        return Err(PROTECTED_REASON.into());
    }
    let parsed = parse(p).ok_or(PROTECTED_REASON)?;
    let name = parsed.verbatim.file_name().ok_or(PROTECTED_REASON)?;
    let parent = parsed.verbatim.parent().ok_or(PROTECTED_REASON)?;
    let real_parent = fs::canonicalize(parent).map_err(|e| reason(&e))?;
    let real = real_parent.join(name);
    fs::symlink_metadata(&real).map_err(|e| reason(&e))?;
    let real = long_path(&real).ok_or(PROTECTED_REASON)?;
    if ctx.is_protected(&real) {
        return Err(PROTECTED_REASON.into());
    }
    Ok(real)
}

/// Caminho real de uma pasta cujo conteúdo sai (resolvido inteiro: é por ele que se lista).
fn resolve_container(ctx: &ProtectCtx, dir: &Path) -> Result<PathBuf, String> {
    if ctx.is_protected_container(dir) {
        return Err(PROTECTED_REASON.into());
    }
    let parsed = parse(dir).ok_or(PROTECTED_REASON)?;
    let real = fs::canonicalize(&parsed.verbatim).map_err(|e| reason(&e))?;
    if ctx.is_protected_container(&real) {
        return Err(PROTECTED_REASON.into());
    }
    if !fs::metadata(&real).map_err(|e| reason(&e))?.is_dir() {
        return Err("não é uma pasta".into());
    }
    Ok(real)
}

/// Expande nomes curtos 8.3 (`PROGRA~1`) sem seguir reparse points. Se a API falhar e algum
/// nome tiver `~` (pode ser nome curto), devolve `None`: tratar como protegido.
fn long_path(p: &Path) -> Option<PathBuf> {
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: `wide` termina em 0; `buf` é um buffer válido do tamanho informado.
        let n = unsafe { GetLongPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf)) } as usize;
        if n == 0 {
            let tilde = p.components().any(|c| match c {
                Component::Normal(n) => n.to_string_lossy().contains('~'),
                _ => false,
            });
            return (!tilde).then(|| p.to_path_buf());
        }
        if n < buf.len() {
            return Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])));
        }
        buf.resize(n + 1, 0);
    }
}

/// Apaga `real` (forma `\\?\`) sem seguir reparse points. Devolve os bytes removidos; falhas
/// vão para `fails` com o caminho mostrado (`shown`).
fn delete_tree(real: &Path, shown: &Path, fails: &mut Vec<(PathBuf, String)>) -> u64 {
    let md = match fs::symlink_metadata(real) {
        Ok(m) => m,
        Err(e) => {
            fails.push((shown.to_path_buf(), reason(&e)));
            return 0;
        }
    };
    let attrs = md.file_attributes();
    let is_dir = attrs & ATTR_DIRECTORY != 0;
    if attrs & ATTR_REPARSE_POINT != 0 {
        if let ReparseKind::Foreign(why) = classify_reparse(attrs, reparse_tag(real)) {
            fails.push((shown.to_path_buf(), why.to_string()));
            return 0;
        }
        // Junção ou symlink: remove o link, nunca o alvo.
        let res = if is_dir {
            fs::remove_dir(real)
        } else {
            fs::remove_file(real)
        };
        if let Err(e) = res {
            fails.push((shown.to_path_buf(), reason(&e)));
        }
        return 0;
    }
    if attrs & ATTR_READONLY != 0 {
        let wide: Vec<u16> = real.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: `wide` termina em 0. Falha aqui aparece depois, ao apagar.
        let _ = unsafe {
            SetFileAttributesW(
                PCWSTR(wide.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(attrs & !ATTR_READONLY),
            )
        };
    }
    if !is_dir {
        return match fs::remove_file(real) {
            Ok(()) => md.len(),
            Err(e) => {
                fails.push((shown.to_path_buf(), reason(&e)));
                0
            }
        };
    }
    let before = fails.len();
    let mut freed = 0;
    match fs::read_dir(real) {
        Err(e) => {
            fails.push((shown.to_path_buf(), reason(&e)));
            return 0;
        }
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(e) => freed += delete_tree(&e.path(), &shown.join(e.file_name()), fails),
                    Err(e) => fails.push((shown.to_path_buf(), reason(&e))),
                }
            }
        }
    }
    if fails.len() == before
        && let Err(e) = fs::remove_dir(real)
    {
        fails.push((shown.to_path_buf(), reason(&e)));
    }
    freed
}

/// Soma dos tamanhos dos arquivos, sem seguir reparse points. O segundo valor diz se há um
/// reparse point que não é link (placeholder da nuvem…) no item, com o motivo.
fn measure(real: &Path) -> (u64, Option<&'static str>) {
    let Ok(md) = fs::symlink_metadata(real) else {
        return (0, None);
    };
    let attrs = md.file_attributes();
    if attrs & ATTR_REPARSE_POINT != 0 {
        return match classify_reparse(attrs, reparse_tag(real)) {
            ReparseKind::Foreign(why) => (0, Some(why)),
            _ => (0, None),
        };
    }
    if attrs & ATTR_DIRECTORY == 0 {
        return (md.len(), None);
    }
    let mut total = 0;
    let mut foreign = None;
    if let Ok(rd) = fs::read_dir(real) {
        for e in rd.flatten() {
            let (s, f) = measure(&e.path());
            total += s;
            foreign = foreign.or(f);
        }
    }
    (total, foreign)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReparseKind {
    NotReparse,
    /// Junção (mount point) ou symlink: remove-se o link.
    Link,
    /// Qualquer outro reparse point: não mexe; o texto é o motivo para `skipped`.
    Foreign(&'static str),
}

/// `tag` = `None` se não deu para ler (trata como estranho: não mexe).
fn classify_reparse(attrs: u32, tag: Option<u32>) -> ReparseKind {
    if attrs & ATTR_REPARSE_POINT == 0 {
        return ReparseKind::NotReparse;
    }
    match tag {
        Some(IO_REPARSE_TAG_MOUNT_POINT | IO_REPARSE_TAG_SYMLINK) => ReparseKind::Link,
        // IO_REPARSE_TAG_CLOUD e variantes: 0x9000_X01A.
        Some(t) if t & 0xFFFF_0FFF == 0x9000_001A => ReparseKind::Foreign(CLOUD_REASON),
        _ => ReparseKind::Foreign(FOREIGN_REPARSE_REASON),
    }
}

/// Tag do reparse point de `p` (sem seguir), via `FindFirstFileW` (`dwReserved0`).
fn reparse_tag(p: &Path) -> Option<u32> {
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `wide` termina em 0; `data` é válido; o handle é fechado logo depois.
    let h = unsafe { FindFirstFileW(PCWSTR(wide.as_ptr()), &mut data) }.ok()?;
    // SAFETY: `h` veio de FindFirstFileW com sucesso.
    let _ = unsafe { FindClose(h) };
    (data.dwFileAttributes & ATTR_REPARSE_POINT != 0).then_some(data.dwReserved0)
}

/// Motivo em pt-BR para um erro de E/S.
fn reason(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(32) | Some(33) => return "arquivo em uso".into(),
        Some(5) => return "sem permissão".into(),
        Some(2) | Some(3) => return "caminho não encontrado".into(),
        _ => {}
    }
    match e.kind() {
        io::ErrorKind::NotFound => "caminho não encontrado".into(),
        io::ErrorKind::PermissionDenied => "sem permissão".into(),
        _ => e.to_string(),
    }
}

/// Motivo em pt-BR de um item que não foi para a Lixeira (nunca o `Debug` em inglês).
fn trash_reason(e: &trash::Error) -> String {
    let detail = match e {
        trash::Error::Os { description, .. } | trash::Error::Unknown { description } => {
            description.trim().to_string()
        }
        trash::Error::TargetedRoot => "é a raiz de uma unidade".to_string(),
        trash::Error::CouldNotAccess { .. } => "não existe ou está sem permissão".to_string(),
        trash::Error::CanonicalizePath { .. } | trash::Error::ConvertOsString { .. } => {
            "caminho inválido".to_string()
        }
        _ => "erro do sistema".to_string(),
    };
    format!("não consegui mandar para a Lixeira: {detail}")
}

// ---------------------------------------------------------------------------------------------
// Discos virtuais (WSL/Docker) e Docker prune
//
// Nada disto roda nos testes: eles só conferem os comandos, os scripts e as funções puras.

const WSL_SHUTDOWN_LIMIT: Duration = Duration::from_secs(60);
const DISKPART_LIMIT: Duration = Duration::from_secs(30 * 60);
const DETACH_LIMIT: Duration = Duration::from_secs(120);
const DOCKER_PRUNE_LIMIT: Duration = Duration::from_secs(10 * 60);
const FSTRIM_LIMIT: Duration = Duration::from_secs(5 * 60);
/// Abaixo disto, "quase nada" saiu da compactação.
const NEAR_ZERO: u64 = 16 * 1024 * 1024;

const DOCKER_DESKTOP_REASON: &str = "Feche o Docker Desktop antes de compactar";
const ADMIN_REASON: &str = "precisa de admin";
const IN_USE_REASON: &str = "o disco está em uso";
const NOT_VHDX_REASON: &str = "não é um disco .vhdx/.vhd";
const BAD_PATH_REASON: &str = "caminho inválido";
const NO_SHORT_NAME_REASON: &str =
    "o caminho tem acentos e não tem nome curto; o diskpart não aceita";
const NOT_CONFIRMED_REASON: &str = "o diskpart não confirmou a compactação";

/// Confere o caminho de um `CompactVhdx` sem tocar o disco: termina em `.vhdx`/`.vhd`, é
/// absoluto numa unidade com letra, não tem `"` nem quebra de linha (iria quebrar o script do
/// diskpart) e não é protegido.
fn check_vhdx_path(ctx: &ProtectCtx, p: &Path) -> Result<(), &'static str> {
    let s = p.as_os_str().to_string_lossy();
    if s.contains(['"', '\n', '\r']) {
        return Err(BAD_PATH_REASON);
    }
    let lower = s.to_lowercase();
    if !(lower.ends_with(".vhdx") || lower.ends_with(".vhd")) {
        return Err(NOT_VHDX_REASON);
    }
    let parsed = parse(p).ok_or(BAD_PATH_REASON)?;
    // Só `X:` (sem compartilhamento de rede).
    if !(parsed.norm.prefix.len() == 2 && parsed.norm.prefix.ends_with(':')) {
        return Err(BAD_PATH_REASON);
    }
    if ctx.is_protected(p) {
        return Err(PROTECTED_REASON);
    }
    Ok(())
}

/// Caminho sem o prefixo `\\?\` (o diskpart não o entende).
fn plain_path(p: &Path) -> String {
    let s = p.as_os_str().to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(r) => r.to_string(),
        None => s.into_owned(),
    }
}

/// Script do diskpart que compacta `path` (já validado por `check_vhdx_path`).
pub fn diskpart_script(path: &Path) -> String {
    format!(
        "select vdisk file=\"{}\"\r\nattach vdisk readonly\r\ncompact vdisk\r\ndetach vdisk\r\n",
        plain_path(path)
    )
}

/// Script de socorro: solta o disco se a compactação falhou no meio.
fn detach_script(path: &Path) -> String {
    format!(
        "select vdisk file=\"{}\"\r\ndetach vdisk\r\n",
        plain_path(path)
    )
}

/// `%SystemRoot%\System32\<exe>`: caminho completo, nada de procurar no PATH.
fn system32(exe: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join(exe)
}

/// Caminho para o script do diskpart, que lê o arquivo em 8 bits: só ASCII. Com acento, usa
/// o nome curto 8.3 (`short` é injetável nos testes); sem nome curto ASCII, recusa.
fn diskpart_path(
    real: &Path,
    short: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Result<String, &'static str> {
    let plain = plain_path(real);
    if plain.is_ascii() {
        return Ok(plain);
    }
    let s = short(real).map(|p| plain_path(&p));
    match s {
        Some(s) if s.is_ascii() && !s.contains(['"', '\n', '\r']) => Ok(s),
        _ => Err(NO_SHORT_NAME_REASON),
    }
}

/// Nome curto 8.3 (`GetShortPathNameW`). `None` se a API falhar.
fn short_path(p: &Path) -> Option<PathBuf> {
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: `wide` termina em 0; `buf` é um buffer válido do tamanho informado.
        let n = unsafe { GetShortPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf)) } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])));
        }
        buf.resize(n + 1, 0);
    }
}

/// Nome de distro seguro para pôr na linha de comando: `[A-Za-z0-9._-]`, sem começar com `-`.
fn valid_distro(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Comandos de uma compactação, todos sem janela.
struct CompactSteps {
    /// `wsl -d <distro> -u root -- fstrim -av` (só distro do WSL com nome válido).
    fstrim: Option<Command>,
    /// `wsl --shutdown`, antes de cada compactação.
    shutdown: Command,
    /// `diskpart /s <script>`.
    diskpart: Command,
}

fn compact_steps(script: &Path, distro: Option<&str>) -> CompactSteps {
    let fstrim = distro.filter(|d| valid_distro(d)).map(|d| {
        let mut c = Command::new(system32("wsl.exe"));
        c.args(["-d", d, "-u", "root", "--", "fstrim", "-av"])
            .creation_flags(CREATE_NO_WINDOW);
        c
    });
    let mut shutdown = Command::new(system32("wsl.exe"));
    shutdown.arg("--shutdown").creation_flags(CREATE_NO_WINDOW);
    CompactSteps {
        fstrim,
        shutdown,
        diskpart: diskpart_command(script),
    }
}

fn diskpart_command(script: &Path) -> Command {
    let mut c = Command::new(system32("diskpart.exe"));
    c.arg("/s").arg(script).creation_flags(CREATE_NO_WINDOW);
    c
}

/// `docker system prune -a -f`, sem `--volumes` (volumes têm dados).
fn docker_prune_command() -> Command {
    let exe = crate::insights::virt::docker_exe().unwrap_or_else(|| PathBuf::from("docker.exe"));
    let mut c = Command::new(exe);
    c.args(["system", "prune", "-a", "-f"])
        .creation_flags(CREATE_NO_WINDOW);
    c
}

/// Bytes de `Total reclaimed space: 1.234GB` (unidades decimais, como o Docker imprime).
pub fn parse_reclaimed(stdout: &str) -> Option<u64> {
    const KEY: &str = "Total reclaimed space:";
    let line = stdout.lines().rev().find(|l| l.contains(KEY))?;
    let value = &line[line.find(KEY)? + KEY.len()..];
    crate::insights::virt::parse_size(value)
}

/// `true` se o Docker Desktop está aberto, dada a lista de nomes de processos.
fn docker_desktop_running(processes: &[String]) -> bool {
    processes
        .iter()
        .any(|p| p.eq_ignore_ascii_case("Docker Desktop.exe"))
}

/// Nomes dos executáveis em execução (Toolhelp32). `None` se não der para listar.
fn process_names() -> Option<Vec<String>> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: snapshot de processos do sistema; o handle é fechado no fim.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let mut e = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    // SAFETY: `snap` é válido; `e` tem `dwSize` preenchido.
    let mut ok = unsafe { Process32FirstW(snap, &mut e) }.is_ok();
    while ok {
        let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
        out.push(String::from_utf16_lossy(&e.szExeFile[..len]));
        // SAFETY: idem.
        ok = unsafe { Process32NextW(snap, &mut e) }.is_ok();
    }
    // SAFETY: `snap` veio de CreateToolhelp32Snapshot com sucesso.
    let _ = unsafe { CloseHandle(snap) };
    (!out.is_empty()).then_some(out)
}

/// Tamanho alocado no disco (`GetCompressedFileSizeW`; tamanho lógico se a API falhar).
/// `None` se o arquivo não existir.
pub fn allocated_size(p: &Path) -> Option<u64> {
    use windows::Win32::Storage::FileSystem::GetCompressedFileSizeW;
    let len = fs::metadata(p).ok()?.len();
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut high = 0u32;
    // SAFETY: `wide` termina em 0; `high` é um u32 válido.
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(wide.as_ptr()), Some(&mut high)) };
    if low == u32::MAX {
        // INVALID_FILE_SIZE (ou um tamanho que coincide): fica com o lógico.
        return Some(len);
    }
    Some(((high as u64) << 32) | low as u64)
}

/// Script temporário em ASCII puro, sem BOM (o diskpart lê 8 bits), apagado no `Drop`.
struct TempScript {
    path: PathBuf,
}

impl TempScript {
    fn create_in(dir: &Path, text: &str) -> io::Result<TempScript> {
        use std::io::Write;
        if !text.is_ascii() {
            return Err(io::Error::other(
                "script do diskpart com caractere fora do ASCII",
            ));
        }
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut last_err = None;
        for _ in 0..16 {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!(
                "deixa-seco-diskpart-{}-{nanos}-{n}.txt",
                std::process::id()
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    let me = TempScript { path };
                    f.write_all(text.as_bytes())?; // em erro, `me` sai de cena e apaga o arquivo
                    return Ok(me);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_err = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| io::Error::other("sem nome livre")))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempScript {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

enum RunError {
    Spawn(io::Error),
    /// Passou do prazo e foi morto; leva o que saiu até ali.
    Timeout(String),
}

struct RunOutput {
    code: Option<i32>,
    /// stdout seguido de stderr, já decodificados.
    text: String,
}

/// Saída de console: UTF-8 se for válida, senão a página OEM (diskpart em pt-BR).
fn decode_console(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    use windows::Win32::Globalization::{
        CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar,
    };
    // SAFETY: só lê `bytes`; sem buffer de saída, devolve o tamanho necessário.
    let n = unsafe { MultiByteToWideChar(CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None) };
    if n <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut buf = vec![0u16; n as usize];
    // SAFETY: `buf` tem o tamanho pedido pela chamada anterior.
    let n = unsafe {
        MultiByteToWideChar(
            CP_OEMCP,
            MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
            bytes,
            Some(&mut buf),
        )
    };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Roda sem janela, captura stdout e stderr e espera até `limit`; passou disso, mata o processo.
/// Depois que o processo termina, espera a saída no máximo o que sobra do prazo (mín. 500 ms):
/// um neto pode segurar o pipe aberto.
fn run_capture(mut cmd: Command, limit: Duration) -> Result<RunOutput, RunError> {
    use std::io::Read;
    use std::process::Stdio;
    const TICK: Duration = Duration::from_millis(100);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(RunError::Spawn)?;
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(mut p) = pipe {
            std::thread::spawn(move || {
                let mut b = Vec::new();
                let _ = p.read_to_end(&mut b);
                let _ = tx.send(b);
            });
        }
        rx
    };
    let out_rx = reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err_rx = reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(TICK),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                // Morto o processo, o pipe fecha (a menos que um neto o segure).
                let wait = Duration::from_secs(1);
                let mut bytes = out_rx.recv_timeout(wait).unwrap_or_default();
                bytes.extend(err_rx.recv_timeout(wait).unwrap_or_default());
                return Err(RunError::Timeout(decode_console(&bytes)));
            }
        }
    };
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(500));
    let mut bytes = out_rx.recv_timeout(left).unwrap_or_default();
    bytes.extend(
        err_rx
            .recv_timeout(Duration::from_millis(500))
            .unwrap_or_default(),
    );
    Ok(RunOutput {
        code: status.code(),
        text: decode_console(&bytes),
    })
}

/// Primeira linha que parece erro (senão a última não vazia), cortada em 120 caracteres.
fn relevant_line(text: &str) -> Option<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let pick = lines
        .iter()
        .find(|l| {
            let l = l.to_lowercase();
            ["erro", "error", "falh", "fail"]
                .iter()
                .any(|k| l.contains(k))
        })
        .or(lines.last())?;
    let mut s: String = pick.chars().take(120).collect();
    if pick.chars().count() > 120 {
        s.push('…');
    }
    Some(s)
}

/// Motivo curto em pt-BR para uma falha do diskpart, a partir da saída dele.
fn diskpart_reason(output: &str) -> String {
    let lower = output.to_lowercase();
    if ["em uso", "sendo usado", "in use", "being used"]
        .iter()
        .any(|k| lower.contains(k))
    {
        return IN_USE_REASON.into();
    }
    if [
        "acesso negado",
        "access is denied",
        "privilégio",
        "privilege",
        "administrador",
        "administrator",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        return ADMIN_REASON.into();
    }
    format!(
        "diskpart falhou: {}",
        relevant_line(output).unwrap_or_else(|| "sem saída".into())
    )
}

/// O diskpart terminou bem: código 0 **e** a linha de compactação concluída na saída.
fn diskpart_outcome(code: Option<i32>, output: &str) -> Result<(), String> {
    let lower = output.to_lowercase();
    let compacted = [
        "concluiu com êxito a compactação",
        "compactou com êxito",
        "successfully compacted",
    ]
    .iter()
    .any(|k| lower.contains(k));
    if code == Some(0) && compacted {
        return Ok(());
    }
    let why = diskpart_reason(output);
    if code == Some(0) && why.starts_with("diskpart falhou:") {
        // Saiu "bem", mas sem dizer que compactou (comando não reconhecido…).
        return Err(NOT_CONFIRMED_REASON.into());
    }
    Err(why)
}

/// Soltar o disco depois de uma falha só se foi ESTA execução que o anexou (nunca um disco que
/// o usuário montou).
fn should_detach(succeeded: bool, output: &str) -> bool {
    // Linha a linha: "desanexou com êxito" contém "anexou com êxito" e não é um attach.
    !succeeded
        && output.lines().any(|line| {
            let l = line.to_lowercase();
            ["anexou com êxito", "successfully attached"]
                .iter()
                .any(|k| l.contains(k))
                && !l.contains("desanexou")
                && !l.contains("detached")
        })
}

/// O `fstrim` falhar não impede a compactação; só vira motivo se quase nada saiu.
fn fstrim_note(fstrim_ok: bool, freed: u64) -> Option<String> {
    (!fstrim_ok && freed < NEAR_ZERO)
        .then(|| "compactei, mas quase nada saiu: o fstrim do WSL falhou".to_string())
}

/// Compacta um disco: Docker Desktop fechado, admin, mede, `fstrim` (distro do WSL),
/// `wsl --shutdown`, diskpart, mede de novo. Devolve os bytes liberados e, se o `fstrim` falhou
/// e quase nada saiu, o motivo.
fn compact_vhdx(
    ctx: &ProtectCtx,
    path: &Path,
    distro: Option<&str>,
) -> Result<(u64, Option<String>), String> {
    check_vhdx_path(ctx, path).map_err(str::to_string)?;
    match process_names() {
        None => return Err("não consegui ver se o Docker Desktop está aberto".into()),
        Some(list) if docker_desktop_running(&list) => return Err(DOCKER_DESKTOP_REASON.into()),
        Some(_) => {}
    }
    if !crate::elevation::is_elevated() {
        return Err(ADMIN_REASON.into());
    }
    // Caminho real (pasta-pai resolvida, nomes curtos expandidos) e verificado de novo.
    let real = resolve_item(ctx, path)?;
    check_vhdx_path(ctx, &real).map_err(str::to_string)?;
    let md = fs::symlink_metadata(&real).map_err(|e| reason(&e))?;
    if md.file_attributes() & ATTR_REPARSE_POINT != 0 {
        // O diskpart seguiria o link até outro arquivo.
        return Err("é um link ou ponto de reparse — não mexi".into());
    }
    if !md.is_file() {
        return Err("não é um arquivo".into());
    }
    let ascii = diskpart_path(&real, &short_path).map_err(str::to_string)?;
    let before = allocated_size(&real).ok_or("caminho não encontrado")?;

    let script = TempScript::create_in(&std::env::temp_dir(), &diskpart_script(Path::new(&ascii)))
        .map_err(|e| format!("não consegui criar o script do diskpart: {}", reason(&e)))?;
    let steps = compact_steps(script.path(), distro);
    // `fstrim` devolve ao disco virtual os blocos livres do Linux; falhar não é fatal.
    let fstrim_ok = match steps.fstrim {
        Some(c) => matches!(run_capture(c, FSTRIM_LIMIT), Ok(o) if o.code == Some(0)),
        None => true,
    };
    match run_capture(steps.shutdown, WSL_SHUTDOWN_LIMIT) {
        Ok(o) if o.code == Some(0) => {}
        Ok(_) | Err(RunError::Spawn(_)) => return Err("não consegui desligar o WSL".into()),
        Err(RunError::Timeout(_)) => return Err("o WSL não desligou em 60 s".into()),
    }
    let (result, output) = match run_capture(steps.diskpart, DISKPART_LIMIT) {
        Ok(o) => (diskpart_outcome(o.code, &o.text), o.text),
        Err(RunError::Timeout(text)) => (
            Err("o diskpart passou de 30 min e foi interrompido".into()),
            text,
        ),
        Err(RunError::Spawn(e)) => (
            Err(format!("diskpart falhou: {}", reason(&e))),
            String::new(),
        ),
    };
    drop(script);
    if let Err(why) = result {
        if should_detach(false, &output) {
            detach_after_failure(&ascii);
        }
        return Err(why);
    }
    let after = allocated_size(&real).unwrap_or(before);
    let freed = before.saturating_sub(after);
    Ok((freed, fstrim_note(fstrim_ok, freed)))
}

/// Tenta uma vez soltar o disco que esta execução anexou (o resultado não importa).
fn detach_after_failure(ascii_path: &str) {
    if let Ok(script) =
        TempScript::create_in(&std::env::temp_dir(), &detach_script(Path::new(ascii_path)))
    {
        let _ = run_capture(diskpart_command(script.path()), DETACH_LIMIT);
    }
}

/// Põe no relatório o resultado do prune: `Ok(None)` = deu certo, sem o total do Docker.
fn record_prune(r: &mut CleanupReport, res: Result<Option<u64>, String>) {
    let shown = PathBuf::from("Docker");
    match res {
        Ok(freed) => {
            match freed {
                Some(f) => r.freed_now += f,
                None => r.notes.push(DOCKER_UNKNOWN_NOTE.to_string()),
            }
            r.done.push(shown);
        }
        Err(why) => r.skipped.push((shown, why)),
    }
}

/// `docker system prune -a -f`; devolve o liberado segundo o próprio Docker (`None` se ele
/// não imprimiu `Total reclaimed space`).
fn docker_prune() -> Result<Option<u64>, String> {
    match run_capture(docker_prune_command(), DOCKER_PRUNE_LIMIT) {
        Ok(o) if o.code == Some(0) => Ok(parse_reclaimed(&o.text)),
        Ok(o) => {
            let lower = o.text.to_lowercase();
            if ["daemon", "cannot connect", "pipe"]
                .iter()
                .any(|k| lower.contains(k))
            {
                Err("o Docker não está rodando".into())
            } else {
                Err(format!(
                    "docker falhou: {}",
                    relevant_line(&o.text).unwrap_or_else(|| "sem saída".into())
                ))
            }
        }
        Err(RunError::Timeout(_)) => Err("o Docker passou de 10 min e foi interrompido".into()),
        Err(RunError::Spawn(_)) => Err("Docker não encontrado".into()),
    }
}

// ---------------------------------------------------------------------------------------------
// Lixeira

/// Tamanho total da Lixeira de todas as unidades (0 se não der para ler).
pub fn recycle_bin_size() -> u64 {
    crate::insights::known::recycle_bin_size().unwrap_or(0)
}

/// Esvazia a Lixeira de todas as unidades sem a confirmação do shell (a nossa já aconteceu).
pub fn empty_recycle_bin() -> io::Result<()> {
    // SAFETY: raiz nula = todas as unidades; sem janela-pai.
    unsafe {
        SHEmptyRecycleBinW(
            None,
            PCWSTR::null(),
            SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND,
        )
    }
    .map_err(|e| io::Error::other(e.message()))
}

/// Limite da Lixeira na unidade `letter`, em bytes: `MaxCapacity` (MB) do registro do volume;
/// 0 se a Lixeira estiver desligada nele; se não der para ler, 10% do volume.
pub fn bin_max_bytes(letter: char) -> u64 {
    let root = format!("{}:\\", letter.to_ascii_uppercase());
    if let Some(guid) = volume_guid(&root) {
        let key =
            format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{guid}");
        if let Ok(k) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).open_subkey(key) {
            if k.get_value::<u32, _>("NukeOnDelete").ok() == Some(1) {
                return 0;
            }
            if let Ok(mb) = k.get_value::<u32, _>("MaxCapacity") {
                return mb as u64 * 1024 * 1024;
            }
        }
    }
    volume_total(&root) / 10
}

/// `{GUID}` do volume montado em `root` (`C:\`).
fn volume_guid(root: &str) -> Option<String> {
    let wide: Vec<u16> = root.encode_utf16().chain([0]).collect();
    let mut buf = [0u16; 64];
    // SAFETY: `wide` termina em 0; `buf` tem o tamanho informado (≥ 50 que a API pede).
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(wide.as_ptr()), &mut buf) }.ok()?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let name = String::from_utf16_lossy(&buf[..len]); // \\?\Volume{GUID}\
    let start = name.find('{')?;
    let end = name.find('}')?;
    (end > start).then(|| name[start..=end].to_string())
}

fn volume_total(root: &str) -> u64 {
    let wide: Vec<u16> = root.encode_utf16().chain([0]).collect();
    let mut total = 0u64;
    // SAFETY: `wide` termina em 0; `total` é um u64 válido.
    match unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), None, Some(&mut total), None) } {
        Ok(()) => total,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_builds_verbatim_and_resolves_dots() {
        let p = parse(Path::new(r"c:\Users\A\..\B\.\x.txt")).unwrap();
        assert_eq!(p.verbatim, PathBuf::from(r"\\?\C:\Users\B\x.txt"));
        assert_eq!(p.norm.comps, vec!["users", "b", "x.txt"]);
        let u = parse(Path::new(r"\\?\UNC\srv\Share\d")).unwrap();
        assert_eq!(u.verbatim, PathBuf::from(r"\\?\UNC\srv\Share\d"));
        assert_eq!(u.norm.prefix, r"\\srv\share");
        assert!(parse(Path::new(r"\\.\PhysicalDrive0")).is_none());
        assert!(parse(Path::new(r"\\?\Volume{x}\a")).is_none());
    }

    #[test]
    fn reason_maps_common_errors() {
        assert_eq!(reason(&io::Error::from_raw_os_error(32)), "arquivo em uso");
        assert_eq!(reason(&io::Error::from_raw_os_error(5)), "sem permissão");
        assert_eq!(
            reason(&io::Error::from_raw_os_error(3)),
            "caminho não encontrado"
        );
    }

    #[test]
    fn unc_share_root_is_protected() {
        let c = ProtectCtx::from_parts(&[], &Env::for_test("C:\\"));
        assert!(c.is_protected(Path::new(r"\\srv\share")));
        assert!(c.is_protected(Path::new(r"\\srv\share\Windows\x")));
        assert!(!c.is_protected(Path::new(r"\\srv\share\dados\x")));
    }

    #[test]
    fn uninstall_command_wraps_line_in_quotes_with_s() {
        let line = r#""C:\Program Files\App\unins000.exe" /SILENT "/LOG=C:\a b.txt""#;
        let c = uninstall_command(line);
        assert_eq!(c.get_program(), "cmd");
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec![format!("/S /C \"{line}\"")]);
    }

    #[test]
    fn plan_accepts_store_removal_without_path_check() {
        // Nunca executado aqui: só o plano.
        let c = ProtectCtx::from_parts(&[], &Env::for_test("C:\\"));
        let op = Op::RemoveStorePackage {
            name: "Bloco".into(),
            full_name: "Acme.Bloco_1.0.0.0_x64__abc".into(),
        };
        let pl = plan_with(vec![(op.clone(), 42)], &c, 0);
        assert_eq!(pl.ops, vec![op]);
        assert_eq!(pl.bytes_now, 42);
        assert!(pl.rejected.is_empty() && pl.too_big_for_bin.is_empty());
    }

    #[test]
    fn classify_reparse_by_tag() {
        use ReparseKind::*;
        assert_eq!(classify_reparse(0x20, None), NotReparse);
        let rp = ATTR_REPARSE_POINT | ATTR_DIRECTORY;
        assert_eq!(classify_reparse(rp, Some(IO_REPARSE_TAG_MOUNT_POINT)), Link);
        assert_eq!(
            classify_reparse(ATTR_REPARSE_POINT, Some(IO_REPARSE_TAG_SYMLINK)),
            Link
        );
        // IO_REPARSE_TAG_CLOUD, CLOUD_1, CLOUD_F (OneDrive).
        for t in [0x9000_001A, 0x9000_101A, 0x9000_F01A] {
            assert_eq!(
                classify_reparse(ATTR_REPARSE_POINT, Some(t)),
                Foreign(CLOUD_REASON),
                "{t:#x}"
            );
        }
        // Dedup, AppExecLink, tag ilegível: não mexe.
        for t in [Some(0x8000_0013), Some(0x8000_001B), None] {
            assert_eq!(
                classify_reparse(ATTR_REPARSE_POINT, t),
                Foreign(FOREIGN_REPARSE_REASON)
            );
        }
    }

    #[test]
    fn reparse_tag_reads_junction() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("alvo");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        let ok = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok);
        let v = parse(&link).unwrap().verbatim;
        assert_eq!(reparse_tag(&v), Some(IO_REPARSE_TAG_MOUNT_POINT));
        assert_eq!(reparse_tag(&parse(&target).unwrap().verbatim), None);
    }

    #[test]
    fn long_path_fails_closed_on_unresolvable_tilde() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("PROGRA~9");
        assert_eq!(long_path(&fake), None);
        let plain = dir.path().join("nao_existe");
        assert_eq!(long_path(&plain), Some(plain.clone()));
        let real = dir.path().join("existe~1.txt");
        fs::write(&real, b"x").unwrap();
        assert!(long_path(&real).is_some());
    }

    #[test]
    fn trash_reason_is_pt_br() {
        let os = trash::Error::Os {
            code: 5,
            description: "Acesso negado.".into(),
        };
        assert_eq!(
            trash_reason(&os),
            "não consegui mandar para a Lixeira: Acesso negado."
        );
        let unknown = trash::Error::Unknown {
            description: "falhou".into(),
        };
        assert_eq!(
            trash_reason(&unknown),
            "não consegui mandar para a Lixeira: falhou"
        );
        for e in [
            trash::Error::TargetedRoot,
            trash::Error::CouldNotAccess { target: "x".into() },
            trash::Error::CanonicalizePath {
                original: PathBuf::from("x"),
            },
        ] {
            let r = trash_reason(&e);
            assert!(r.starts_with("não consegui mandar para a Lixeira: "), "{r}");
            for english in ["TargetedRoot", "CouldNotAccess", "Canonicalize", "{"] {
                assert!(!r.contains(english), "{r}");
            }
        }
    }

    // --- Task 16: compactar VHDX e Docker prune. Nada aqui executa wsl, diskpart ou docker:
    // só monta comandos/scripts e testa funções puras.

    fn args_of(c: &Command) -> Vec<String> {
        c.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn program_of(c: &Command) -> String {
        c.get_program().to_string_lossy().to_lowercase()
    }

    #[test]
    fn diskpart_script_is_exact_with_spaces_and_accents() {
        let p = Path::new(r"C:\Users\t\Dados de São Paulo\ext4.vhdx");
        assert_eq!(
            diskpart_script(p),
            "select vdisk file=\"C:\\Users\\t\\Dados de São Paulo\\ext4.vhdx\"\r\n\
             attach vdisk readonly\r\n\
             compact vdisk\r\n\
             detach vdisk\r\n"
        );
        // Forma `\\?\` vira caminho comum (o diskpart não entende o prefixo).
        assert_eq!(
            diskpart_script(Path::new(r"\\?\D:\wsl\ext4.vhdx")),
            "select vdisk file=\"D:\\wsl\\ext4.vhdx\"\r\n\
             attach vdisk readonly\r\n\
             compact vdisk\r\n\
             detach vdisk\r\n"
        );
        assert_eq!(
            detach_script(Path::new(r"\\?\D:\a b\x.vhdx")),
            "select vdisk file=\"D:\\a b\\x.vhdx\"\r\ndetach vdisk\r\n"
        );
    }

    #[test]
    fn diskpart_script_file_is_ascii_without_bom_and_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path;
        let text = "select vdisk file=\"C:\\DADOSD~1\\x.vhdx\"\r\ncompact vdisk\r\n";
        {
            let f = TempScript::create_in(dir.path(), text).unwrap();
            path = f.path().to_path_buf();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            assert!(name.starts_with("deixa-seco-diskpart-") && name.ends_with(".txt"));
            let bytes = fs::read(&path).unwrap();
            assert_eq!(bytes, text.as_bytes());
            assert!(bytes.is_ascii());
            assert_ne!(&bytes[..2], &[0xFF, 0xFE]);
            assert_ne!(&bytes[..3], &[0xEF, 0xBB, 0xBF]);
            // Dois scripts seguidos têm nomes diferentes.
            let g = TempScript::create_in(dir.path(), "x").unwrap();
            assert_ne!(g.path(), f.path());
        }
        assert!(!path.exists());
        // Texto com acento nunca vira script (o diskpart lê 8 bits).
        assert!(TempScript::create_in(dir.path(), "file=\"C:\\São\"").is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn diskpart_path_is_ascii_or_short_name() {
        let none = |_: &Path| -> Option<PathBuf> { None };
        assert_eq!(
            diskpart_path(Path::new(r"\\?\C:\a b\x.vhdx"), &none),
            Ok(r"C:\a b\x.vhdx".to_string())
        );
        let accented = Path::new(r"\\?\C:\Dados São\ext4.vhdx");
        let short = |p: &Path| -> Option<PathBuf> {
            assert_eq!(p, Path::new(r"\\?\C:\Dados São\ext4.vhdx"));
            Some(PathBuf::from(r"\\?\C:\DADOSS~1\ext4.vhdx"))
        };
        assert_eq!(
            diskpart_path(accented, &short),
            Ok(r"C:\DADOSS~1\ext4.vhdx".to_string())
        );
        assert_eq!(diskpart_path(accented, &none), Err(NO_SHORT_NAME_REASON));
        let still =
            |_: &Path| -> Option<PathBuf> { Some(PathBuf::from(r"C:\Dados São\EXT4~1.VHD")) };
        assert_eq!(diskpart_path(accented, &still), Err(NO_SHORT_NAME_REASON));
        assert_eq!(
            NO_SHORT_NAME_REASON,
            "o caminho tem acentos e não tem nome curto; o diskpart não aceita"
        );
    }

    #[test]
    fn compact_steps_shape() {
        let script = Path::new(r"C:\Temp\s.txt");
        let s = compact_steps(script, Some("Ubuntu-22.04"));
        let trim = s.fstrim.as_ref().expect("fstrim para distro do WSL");
        assert!(program_of(trim).ends_with(r"\system32\wsl.exe"));
        assert_eq!(
            args_of(trim),
            vec!["-d", "Ubuntu-22.04", "-u", "root", "--", "fstrim", "-av"]
        );
        assert!(program_of(&s.shutdown).ends_with(r"\system32\wsl.exe"));
        assert_eq!(args_of(&s.shutdown), vec!["--shutdown"]);
        assert!(program_of(&s.diskpart).ends_with(r"\system32\diskpart.exe"));
        assert_eq!(args_of(&s.diskpart), vec!["/s", r"C:\Temp\s.txt"]);
        // Docker: sem fstrim. Nome estranho: sem fstrim.
        assert!(compact_steps(script, None).fstrim.is_none());
        for bad in ["", "Ubu ntu", "a;b", "a\"b", "-u", "São"] {
            assert!(compact_steps(script, Some(bad)).fstrim.is_none(), "{bad}");
        }
    }

    #[test]
    fn distro_names() {
        for ok in ["Ubuntu", "Ubuntu-22.04", "docker_desktop", "Debian.1"] {
            assert!(valid_distro(ok), "{ok}");
        }
        for bad in ["", "-d", "a b", "a&b", "a|b", "Ubuntu\n", "São", "a/b"] {
            assert!(!valid_distro(bad), "{bad}");
        }
    }

    const DP_BANNER: &str = "\r\nMicrosoft DiskPart version 10.0.22621.1\r\n\r\nCopyright (C) Microsoft Corporation.\r\nOn computer: PC\r\n\r\n";

    #[test]
    fn diskpart_outcome_needs_exit_zero_and_marker() {
        let en_ok = format!(
            "{DP_BANNER}DiskPart successfully selected the virtual disk file.\r\n\r\n  100 percent completed\r\n\r\nDiskPart successfully attached the virtual disk file.\r\n\r\n  100 percent completed\r\n\r\nDiskPart successfully compacted the virtual disk file.\r\n\r\nDiskPart successfully detached the virtual disk file.\r\n"
        );
        assert_eq!(diskpart_outcome(Some(0), &en_ok), Ok(()));
        let pt_ok = "\r\nMicrosoft DiskPart versão 10.0.22621.1\r\n\r\nO DiskPart selecionou o arquivo de disco virtual com êxito.\r\n\r\nO DiskPart anexou com êxito o arquivo de disco virtual.\r\n\r\n  100 por cento concluídos\r\n\r\nO DiskPart concluiu com êxito a compactação do arquivo de disco virtual.\r\n\r\nO DiskPart desanexou com êxito o arquivo de disco virtual.\r\n";
        assert_eq!(diskpart_outcome(Some(0), pt_ok), Ok(()));
        // Código 0 sem a linha de compactação: não conta como feito.
        let no_marker =
            format!("{DP_BANNER}DiskPart successfully selected the virtual disk file.\r\n");
        assert_eq!(
            diskpart_outcome(Some(0), &no_marker),
            Err("o diskpart não confirmou a compactação".to_string())
        );
        // Marcador, mas código de erro (ex.: o detach do fim falhou).
        assert!(diskpart_outcome(Some(1), &en_ok).is_err());
        assert!(diskpart_outcome(None, &en_ok).is_err());
        // Comando não reconhecido.
        let bad_cmd = format!(
            "{DP_BANNER}The arguments specified for this command are not valid.\r\nFor more information on the command type: HELP COMPACT VDISK\r\n"
        );
        assert_eq!(
            diskpart_outcome(Some(0), &bad_cmd),
            Err("o diskpart não confirmou a compactação".to_string())
        );
        assert_eq!(
            diskpart_outcome(Some(87), &bad_cmd),
            Err(
                "diskpart falhou: For more information on the command type: HELP COMPACT VDISK"
                    .to_string()
            )
        );
        // Em uso.
        let busy = format!(
            "{DP_BANNER}DiskPart successfully selected the virtual disk file.\r\n\r\nDiskPart has encountered an error: The process cannot access the file because it is being used by another process.\r\nSee the System Event Log for more information.\r\n"
        );
        assert_eq!(
            diskpart_outcome(Some(-2147024864), &busy),
            Err("o disco está em uso".to_string())
        );
        assert_eq!(
            diskpart_outcome(Some(0), &busy),
            Err("o disco está em uso".to_string())
        );
    }

    #[test]
    fn detach_only_what_this_run_attached() {
        let attached_en = "DiskPart successfully selected the virtual disk file.\r\nDiskPart successfully attached the virtual disk file.\r\nDiskPart has encountered an error: The device is not ready.\r\n";
        let attached_pt = "O DiskPart anexou com êxito o arquivo de disco virtual.\r\n";
        assert!(should_detach(false, attached_en));
        assert!(should_detach(false, attached_pt));
        // Deu certo: o próprio script já soltou.
        assert!(!should_detach(true, attached_en));
        // O attach falhou (ex.: o usuário já tinha montado): nunca solta.
        let user_mounted = "DiskPart successfully selected the virtual disk file.\r\nDiskPart has encountered an error: The virtual disk is already attached.\r\n";
        assert!(!should_detach(false, user_mounted));
        assert!(!should_detach(false, ""));
        // Só o desanexar do fim saiu (o attach foi de outra execução): não é um attach.
        let only_detached_pt = "O DiskPart selecionou o arquivo de disco virtual com êxito.\r\nO DiskPart desanexou com êxito o arquivo de disco virtual.\r\n";
        assert!(!should_detach(false, only_detached_pt));
        let only_detached_en = "DiskPart successfully detached the virtual disk file.\r\n";
        assert!(!should_detach(false, only_detached_en));
        // Attach e detach na mesma saída: o attach conta.
        let both = "O DiskPart anexou com êxito o arquivo de disco virtual.\r\nO DiskPart desanexou com êxito o arquivo de disco virtual.\r\n";
        assert!(should_detach(false, both));
    }

    #[test]
    fn fstrim_failure_only_noted_when_nothing_freed() {
        assert_eq!(fstrim_note(true, 0), None);
        assert_eq!(fstrim_note(false, 5 * 1024 * 1024 * 1024), None);
        assert_eq!(
            fstrim_note(false, 1024),
            Some("compactei, mas quase nada saiu: o fstrim do WSL falhou".to_string())
        );
    }

    #[test]
    fn docker_prune_command_has_no_volumes() {
        let c = docker_prune_command();
        assert!(program_of(&c).ends_with("docker.exe"), "{}", program_of(&c));
        assert_eq!(args_of(&c), vec!["system", "prune", "-a", "-f"]);
    }

    #[test]
    fn prune_without_total_is_noted_not_zero() {
        let mut r = CleanupReport::default();
        record_prune(&mut r, Ok(None));
        assert_eq!(r.done, vec![PathBuf::from("Docker")]);
        assert_eq!(r.freed_now, 0);
        assert_eq!(r.notes, vec![DOCKER_UNKNOWN_NOTE.to_string()]);
        let mut r = CleanupReport::default();
        record_prune(&mut r, Ok(Some(5)));
        assert_eq!(r.freed_now, 5);
        assert!(r.notes.is_empty());
        let mut r = CleanupReport::default();
        record_prune(&mut r, Err("o Docker não está rodando".into()));
        assert!(r.done.is_empty());
        assert_eq!(r.skipped.len(), 1);
    }

    #[test]
    fn parse_reclaimed_units() {
        let out = "Deleted Images:\nuntagged: x\n\nTotal reclaimed space: 1.234GB\n";
        assert_eq!(parse_reclaimed(out), Some(1_234_000_000));
        assert_eq!(parse_reclaimed("Total reclaimed space: 0B"), Some(0));
        assert_eq!(
            parse_reclaimed("Total reclaimed space: 512kB"),
            Some(512_000)
        );
        assert_eq!(
            parse_reclaimed("Total reclaimed space: 512KB"),
            Some(512_000)
        );
        assert_eq!(
            parse_reclaimed("Total reclaimed space: 3.5MB\r\n"),
            Some(3_500_000)
        );
        assert_eq!(
            parse_reclaimed("Total reclaimed space: 2TB"),
            Some(2_000_000_000_000)
        );
        assert_eq!(parse_reclaimed("nada aqui"), None);
        assert_eq!(parse_reclaimed("Total reclaimed space: muito"), None);
    }

    #[test]
    fn docker_desktop_check_is_injectable() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(docker_desktop_running(&l(&[
            "explorer.exe",
            "Docker Desktop.exe"
        ])));
        assert!(docker_desktop_running(&l(&["docker desktop.EXE"])));
        assert!(!docker_desktop_running(&l(&[
            "com.docker.backend.exe",
            "docker.exe"
        ])));
        assert!(!docker_desktop_running(&[]));
    }

    fn vhdx_env(root: &Path) -> Env {
        Env {
            user_profile: root.join("perfil"),
            local_app_data: root.join(r"perfil\AppData\Local"),
            app_data: root.join(r"perfil\AppData\Roaming"),
            windows: root.join("Win"),
            system_drive: PathBuf::from("C:\\"),
        }
    }

    #[test]
    fn plan_compact_vhdx_checks_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ctx = ProtectCtx::from_parts(&[], &vhdx_env(root));
        let wsl = root.join(r"perfil\AppData\Local\Pacote São\LocalState");
        fs::create_dir_all(&wsl).unwrap();
        fs::create_dir_all(root.join("Win")).unwrap();
        let ok = wsl.join("ext4.VHDX");
        let vhd = root.join("velho.vhd");
        let vmdk = root.join("disco.vmdk");
        let prot = root.join(r"Win\x.vhdx");
        for f in [&ok, &vhd, &vmdk, &prot] {
            fs::write(f, b"").unwrap();
        }
        let missing = root.join("sumiu.vhdx");
        let ops = vec![
            (
                Op::CompactVhdx {
                    path: ok.clone(),
                    distro: Some("Ubuntu".into()),
                },
                999,
            ),
            (
                Op::CompactVhdx {
                    path: vhd.clone(),
                    distro: None,
                },
                5,
            ),
            (
                Op::CompactVhdx {
                    path: vmdk.clone(),
                    distro: None,
                },
                5,
            ),
            (
                Op::CompactVhdx {
                    path: prot.clone(),
                    distro: None,
                },
                5,
            ),
            (
                Op::CompactVhdx {
                    path: missing.clone(),
                    distro: None,
                },
                5,
            ),
            (
                Op::CompactVhdx {
                    path: PathBuf::from(r"rel\x.vhdx"),
                    distro: None,
                },
                5,
            ),
            (Op::DockerPrune, 70),
        ];
        let pl = plan_with(ops, &ctx, u64::MAX);
        assert_eq!(
            pl.ops,
            vec![
                Op::CompactVhdx {
                    path: ok,
                    distro: Some("Ubuntu".into())
                },
                Op::CompactVhdx {
                    path: vhd,
                    distro: None
                },
                Op::DockerPrune
            ]
        );
        // Compactar: liberado desconhecido até terminar; prune: a estimativa do `df`.
        assert_eq!(pl.bytes_now, 70);
        assert_eq!(
            pl.rejected,
            vec![vmdk, prot, missing, PathBuf::from(r"rel\x.vhdx")]
        );
    }

    #[test]
    fn vhdx_check_reasons() {
        let ctx = ProtectCtx::from_parts(&[], &Env::for_test("C:\\"));
        let bad = |s: &str| check_vhdx_path(&ctx, Path::new(s));
        assert_eq!(bad(r"C:\d\x.vmdk"), Err("não é um disco .vhdx/.vhd"));
        assert_eq!(bad(r"C:\d\a.vhdx.bak"), Err("não é um disco .vhdx/.vhd"));
        assert_eq!(bad("C:\\d\\a\"b.vhdx"), Err("caminho inválido"));
        assert_eq!(bad("C:\\d\\a\nb.vhdx"), Err("caminho inválido"));
        assert_eq!(bad(r"x.vhdx"), Err("caminho inválido"));
        assert_eq!(bad(r"\\srv\share\x.vhdx"), Err("caminho inválido"));
        assert_eq!(bad(r"C:\Windows\x.vhdx"), Err(PROTECTED_REASON));
        assert_eq!(
            bad(r"C:\Program Files\Docker\x.vhdx"),
            Err(PROTECTED_REASON)
        );
        assert_eq!(
            bad(r"C:\Users\t\AppData\Local\Docker\wsl\disk\docker_data.vhdx"),
            Ok(())
        );
        assert_eq!(bad(r"D:\WSL\Ubuntu\ext4.vhd"), Ok(()));
    }

    #[test]
    fn diskpart_failure_reasons_pt_br() {
        let banner = "\r\nMicrosoft DiskPart versão 10.0.22621.1\r\n\r\nCopyright (C) Microsoft Corporation.\r\nNo computador: PC\r\n\r\n";
        assert_eq!(
            diskpart_reason(&format!(
                "{banner}O DiskPart encontrou um erro: O processo não pode acessar o arquivo porque ele está sendo usado por outro processo.\r\n"
            )),
            "o disco está em uso"
        );
        assert_eq!(
            diskpart_reason(
                "DiskPart has encountered an error: The process cannot access the file because it is being used by another process."
            ),
            "o disco está em uso"
        );
        assert_eq!(
            diskpart_reason("O DiskPart encontrou um erro: Acesso negado."),
            "precisa de admin"
        );
        assert_eq!(
            diskpart_reason("DiskPart has encountered an error: Access is denied."),
            "precisa de admin"
        );
        assert_eq!(
            diskpart_reason(&format!(
                "{banner}O DiskPart conseguiu selecionar o arquivo de disco virtual.\r\n\r\nO DiskPart encontrou um erro: O sistema não pode encontrar o arquivo especificado.\r\nConsulte o log de eventos.\r\n"
            )),
            "diskpart falhou: O DiskPart encontrou um erro: O sistema não pode encontrar o arquivo especificado."
        );
        assert_eq!(diskpart_reason(""), "diskpart falhou: sem saída");
        let long = "x".repeat(500);
        assert!(diskpart_reason(&long).chars().count() <= "diskpart falhou: ".len() + 121);
    }

    #[test]
    fn allocated_size_of_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.vhdx");
        fs::write(&f, vec![1u8; 10_000]).unwrap();
        let s = allocated_size(&f).unwrap();
        assert!(s >= 10_000, "{s}");
        assert_eq!(allocated_size(&dir.path().join("nao.vhdx")), None);
    }

    #[test]
    fn boot_entries_at_volume_root_are_protected() {
        let c = ProtectCtx::from_parts(&[], &Env::for_test("C:\\"));
        for s in [
            r"C:\bootmgr",
            r"D:\BOOTNXT",
            r"C:\Boot",
            r"C:\Boot\BCD",
            r"E:\EFI\Microsoft",
            r"C:\Recovery\x",
            r"C:\pagefile.sys",
            r"D:\hiberfil.sys",
            r"C:\swapfile.sys",
            r"C:\$Recycle.Bin\S-1-5",
            r"D:\System Volume Information",
        ] {
            assert!(c.is_protected(Path::new(s)), "{s}");
        }
        assert!(c.is_protected_container(Path::new(r"C:\Boot")));
        assert!(!c.is_protected(Path::new(r"C:\Users\t\Boot")));
        assert!(!c.is_protected(Path::new(r"D:\Jogos\pagefile.sys")));
    }
}
