//! Virtualização: discos do WSL e do Docker (compactar), lixo do Docker (prune), emuladores
//! Android (snapshots, imagens de sistema sem uso, AVDs parados) e VMs/imagens de disco paradas.
//!
//! `virt_insights` é pura (só a `Tree` e os `SysFacts`). O que toca o sistema (registro do WSL,
//! `docker system df`, `config.ini` dos AVDs) fica nas funções `gather_*`, chamadas só por
//! `SysFacts::gather` no worker. Nada aqui apaga, compacta ou executa limpeza.

use super::{Action, Env, Insight, Risk, SysFacts, known};
use crate::cleanup::ProtectCtx;
use crate::tree::{NodeId, Tree};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

const STALE: Duration = Duration::from_secs(90 * 86_400);
/// Tamanho mínimo de um arquivo de VM/imagem para entrar em `stale_vms`.
const MIN_VM_FILE: u64 = 500 * 1024 * 1024;
const VM_EXTS: [&str; 7] = ["vhdx", "vhd", "vmdk", "vdi", "qcow2", "iso", "img"];
/// Pastas que nunca entram em `stale_vms` (apps da Store).
const SKIP_DIRS: [&str; 1] = ["windowsapps"];
/// Discos de dados do WSL/Docker, reconhecidos pelo nome mesmo fora do lugar padrão. Nunca vão
/// para a Lixeira (o mtime de um disco montado fica atrasado); os `.vhdx` vão para compactar.
const LIVE_DISK_FILES: [&str; 3] = ["docker_data.vhdx", "ext4.vhdx", "swap.vhdx"];
/// Pastas cujo conteúdo nunca entra em `stale_vms` (dados do Docker/WSL).
const LIVE_DISK_DIRS: [&str; 3] = ["docker", "dockerdesktopwsl", "wsl"];

/// Distro do WSL registrada em `HKCU\...\Lxss`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WslDisk {
    pub name: String,
    pub vhdx: PathBuf,
}

/// Disco virtual do WSL/Docker que dá para compactar (um item de `vhdx_compact`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtDisk {
    /// O arquivo na árvore.
    pub node: NodeId,
    pub path: PathBuf,
    /// Nome mostrado ("Ubuntu", "Docker", "WSL").
    pub label: String,
    /// Distro do WSL registrada dona do disco; `None` para o Docker e discos sem registro.
    pub distro: Option<String>,
}

/// AVD conhecido pelo emulador (lido do `.ini` e do `config.ini`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvdConfig {
    /// A pasta `.avd`.
    pub dir: PathBuf,
    /// O `<nome>.ini` que aponta para a pasta, quando existe.
    pub ini: Option<PathBuf>,
    /// Valores de `image.sysdir.N` (ex.: `system-images\android-36\google_apis\x86_64\`).
    pub sysdirs: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Análise (pura)

fn older_than(now: SystemTime, m: SystemTime) -> bool {
    now.duration_since(m).is_ok_and(|d| d > STALE)
}

/// Mtime mais recente do nó e de tudo abaixo dele.
fn newest_any(tree: &Tree, id: NodeId) -> Option<SystemTime> {
    let mut best: Option<SystemTime> = None;
    let mut stack = vec![id];
    while let Some(cur) = stack.pop() {
        if let Some(m) = tree.node(cur).mtime {
            best = Some(best.map_or(m, |b| b.max(m)));
        }
        stack.extend(tree.children(cur).iter().copied());
    }
    best
}

fn stale_node(tree: &Tree, id: NodeId, now: SystemTime) -> bool {
    newest_any(tree, id).is_some_and(|m| older_than(now, m))
}

fn find_abs(tree: &Tree, p: &Path) -> Option<NodeId> {
    if p.is_absolute() { tree.find(p) } else { None }
}

/// Componentes normais do caminho, em minúsculas (para comparar sufixos).
fn lower_comps(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect()
}

fn ext_of(name: &str) -> Option<String> {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())
}

fn has_file(tree: &Tree, dir: NodeId, name: &str) -> bool {
    tree.children(dir).iter().any(|&k| {
        let n = tree.node(k);
        !n.is_dir && n.name.eq_ignore_ascii_case(name)
    })
}

fn dir_children(tree: &Tree, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    tree.children(id)
        .iter()
        .copied()
        .filter(move |&k| tree.node(k).is_dir)
}

/// O que uma passada pela árvore encontra.
#[derive(Default)]
struct Found {
    avds: Vec<NodeId>,
    /// Pastas `system-images\<api>\<tag>\<abi>`.
    images: Vec<NodeId>,
    /// Pastas de VM conhecidas (Genymotion `deployed\*`, `VirtualBox VMs\*`).
    vm_dirs: Vec<NodeId>,
    /// Arquivos com extensão de disco/imagem e tamanho suficiente.
    vm_files: Vec<NodeId>,
}

fn scan(tree: &Tree) -> Found {
    let mut f = Found::default();
    for i in 1..tree.len() {
        let id = NodeId(i as u32);
        let n = tree.node(id);
        if !n.alive {
            continue;
        }
        if !n.is_dir {
            if n.size >= MIN_VM_FILE
                && ext_of(&n.name).is_some_and(|e| VM_EXTS.contains(&e.as_str()))
            {
                f.vm_files.push(id);
            }
            continue;
        }
        // Sem alocar por pasta: a passada cobre a árvore inteira.
        let name = &*n.name;
        let b = name.as_bytes();
        if b.len() > 4
            && b[b.len() - 4..].eq_ignore_ascii_case(b".avd")
            && has_file(tree, id, "config.ini")
        {
            f.avds.push(id);
        } else if name.eq_ignore_ascii_case("system-images") {
            for api in dir_children(tree, id) {
                for tag in dir_children(tree, api) {
                    f.images.extend(dir_children(tree, tag));
                }
            }
        } else if name.eq_ignore_ascii_case("virtualbox vms")
            || (name.eq_ignore_ascii_case("deployed")
                && n.parent
                    .is_some_and(|p| tree.node(p).name.eq_ignore_ascii_case("genymotion")))
        {
            f.vm_dirs.extend(dir_children(tree, id));
        }
    }
    f
}

/// Algum ancestral (ou o próprio nó) está no conjunto?
fn under_any(tree: &Tree, id: NodeId, set: &HashSet<NodeId>) -> bool {
    tree.ancestors(id).iter().any(|a| set.contains(a))
}

fn sorted_by_size(tree: &Tree, mut nodes: Vec<NodeId>) -> Vec<NodeId> {
    nodes.sort_by_key(|&n| std::cmp::Reverse(tree.node(n).size));
    nodes
}

struct Spec {
    id: &'static str,
    title: &'static str,
    explanation: &'static str,
    risk: Risk,
    action: Action,
    needs_admin: bool,
    hint: Option<String>,
}

/// Insight com os nós da árvore (tamanho = soma); `None` se não sobrar nada.
fn from_nodes(tree: &Tree, s: Spec, nodes: Vec<NodeId>) -> Option<Insight> {
    let nodes = sorted_by_size(tree, known::dedup_nested(tree, nodes));
    let size: u64 = nodes.iter().map(|&n| tree.node(n).size).sum();
    (size > 0).then(|| Insight {
        id: s.id,
        title: s.title.into(),
        explanation: s.explanation.into(),
        risk: s.risk,
        size,
        nodes,
        action: s.action,
        needs_admin: s.needs_admin,
        hint: s.hint,
        programs: Vec::new(),
        disks: Vec::new(),
        companions: Vec::new(),
    })
}

fn has_ancestor_named(tree: &Tree, id: NodeId, names: &[&str]) -> bool {
    tree.ancestors(id).iter().any(|&a| {
        let n = tree.node(a).name.to_lowercase();
        names.contains(&n.as_str())
    })
}

/// Arquivo que parece disco de dados do WSL/Docker (pelo nome ou por estar numa pasta deles).
fn is_live_disk(tree: &Tree, id: NodeId) -> bool {
    let name = tree.node(id).name.to_lowercase();
    LIVE_DISK_FILES.contains(&name.as_str()) || has_ancestor_named(tree, id, &LIVE_DISK_DIRS)
}

/// Discos do WSL e do Docker que estão na árvore (e fora de pastas protegidas), com o nome de
/// cada um: os registrados e os `docker_data.vhdx`/`ext4.vhdx` achados em outro lugar.
fn virtual_disks(
    tree: &Tree,
    facts: &SysFacts,
    found: &Found,
    protect: &ProtectCtx,
) -> Vec<VirtDisk> {
    let mut out: Vec<VirtDisk> = Vec::new();
    let disk = |node: NodeId, label: &str, distro: Option<&str>| VirtDisk {
        node,
        path: tree.path(node),
        label: label.to_string(),
        distro: distro.map(str::to_string),
    };
    let all = facts
        .wsl_disks
        .iter()
        .map(|d| (d.vhdx.as_path(), d.name.as_str()))
        .chain(facts.docker_vhdx.as_deref().map(|p| (p, "Docker")));
    for (path, name) in all {
        let Some(id) = find_abs(tree, path) else {
            continue;
        };
        if tree.node(id).is_dir || protect.is_protected(path) {
            continue;
        }
        // As distros internas do Docker Desktop (`docker-desktop`, `docker-desktop-data`)
        // aparecem como "Docker", sem distro (o `fstrim` é só para distro do usuário).
        let (label, distro) = if name.to_lowercase().starts_with("docker-desktop") {
            ("Docker", None)
        } else if Some(path) == facts.docker_vhdx.as_deref() {
            (name, None)
        } else {
            (name, Some(name))
        };
        if !out.iter().any(|d| d.node == id) {
            out.push(disk(id, label, distro));
        }
    }
    for &f in &found.vm_files {
        let name = tree.node(f).name.to_lowercase();
        let label = match name.as_str() {
            "docker_data.vhdx" => "Docker",
            "ext4.vhdx" if has_ancestor_named(tree, f, &["docker", "dockerdesktopwsl"]) => "Docker",
            "ext4.vhdx" => "WSL",
            _ => continue,
        };
        if !out.iter().any(|d| d.node == f) && !protect.is_protected(&tree.path(f)) {
            out.push(disk(f, label, None));
        }
    }
    out
}

fn vhdx_compact(tree: &Tree, disks: &[VirtDisk]) -> Option<Insight> {
    let mut names: Vec<&str> = Vec::new();
    for d in disks {
        if !names.contains(&d.label.as_str()) {
            names.push(&d.label);
        }
    }
    let mut ins = from_nodes(
        tree,
        Spec {
            id: "vhdx_compact",
            title: "Discos virtuais do WSL e Docker",
            explanation: "Esses discos crescem e não encolhem sozinhos; compactar devolve ao Windows o espaço que ficou livre dentro deles, sem apagar nada.",
            risk: Risk::Seguro,
            action: Action::CompactVhdx,
            needs_admin: true,
            hint: Some(format!(
                "Discos: {}. Para apagar uma distro inteira, use `wsl --unregister <nome>` (perde tudo o que há nela).",
                names.join(", ")
            )),
        },
        disks.iter().map(|d| d.node).collect(),
    )?;
    // Mesma ordem dos nós (maior primeiro).
    ins.disks = ins
        .nodes
        .iter()
        .filter_map(|&n| disks.iter().find(|d| d.node == n).cloned())
        .collect();
    Some(ins)
}

/// Só aparece se o disco do Docker está na árvore varrida. O tamanho vem de `docker system df`
/// (por isso `nodes` fica vazio: não é o tamanho de nenhum nó).
fn docker_prune(tree: &Tree, facts: &SysFacts) -> Option<Insight> {
    let vhdx = facts.docker_vhdx.as_deref()?;
    find_abs(tree, vhdx)?;
    let (risk, action, size, explanation) = match facts.docker_reclaimable {
        Some(0) => return None,
        Some(n) => (
            Risk::Revisar,
            Action::DockerPrune,
            n,
            "Imagens, contêineres parados e cache de build que o Docker não está usando; os volumes ficam.",
        ),
        None => (
            Risk::Informativo,
            Action::None,
            0,
            "Abra o Docker Desktop para medir o que dá para limpar.",
        ),
    };
    Some(Insight {
        id: "docker_prune",
        title: "Lixo do Docker".into(),
        explanation: explanation.into(),
        risk,
        size,
        nodes: Vec::new(),
        action,
        needs_admin: false,
        hint: None,
        programs: Vec::new(),
        disks: Vec::new(),
        companions: Vec::new(),
    })
}

/// `system-images\android-36\google_apis\x86_64\` → componentes em minúsculas.
fn sysdir_comps(s: &str) -> Vec<String> {
    lower_comps(Path::new(&s.replace('/', "\\")))
}

const AVDS_EXPLANATION: &str =
    "Emuladores que você não abre há mais de 90 dias, com todos os dados deles.";

fn android(
    tree: &Tree,
    facts: &SysFacts,
    found: &Found,
    protect: &ProtectCtx,
    now: SystemTime,
) -> Vec<Insight> {
    let mut out = Vec::new();
    let ok = |id: NodeId| !protect.is_protected(&tree.path(id));

    // AVDs parados: a pasta `.avd` e o `.ini` que aponta para ela.
    let stale: Vec<NodeId> = found
        .avds
        .iter()
        .copied()
        .filter(|&a| ok(a) && stale_node(tree, a, now))
        .collect();
    let mut avd_nodes = stale.clone();
    let mut pairs: Vec<(NodeId, NodeId)> = Vec::new();
    for &a in &stale {
        let dir = tree.path(a);
        let known_ini = facts
            .avds
            .iter()
            .find(|c| lower_comps(&c.dir) == lower_comps(&dir))
            .and_then(|c| c.ini.as_deref())
            .and_then(|p| find_abs(tree, p));
        let sibling = || {
            let stem = tree.node(a).name.rsplit_once('.').map(|(s, _)| s)?;
            let parent = tree.node(a).parent?;
            let want = format!("{stem}.ini");
            tree.children(parent).iter().copied().find(|&k| {
                let n = tree.node(k);
                !n.is_dir && n.name.eq_ignore_ascii_case(&want)
            })
        };
        if let Some(ini) = known_ini.or_else(sibling).filter(|&i| ok(i)) {
            avd_nodes.push(ini);
            pairs.push((a, ini));
        }
    }

    // Snapshots dos AVDs que ficam (os parados já saem inteiros).
    let snaps: Vec<NodeId> = found
        .avds
        .iter()
        .filter(|a| !stale.contains(a))
        .flat_map(|&a| {
            tree.children(a).iter().copied().filter(|&k| {
                let n = tree.node(k);
                n.is_dir && n.name.eq_ignore_ascii_case("snapshots")
            })
        })
        .filter(|&s| !protect.is_protected_container(&tree.path(s)))
        .collect();
    out.extend(from_nodes(
        tree,
        Spec {
            id: "android_snapshots",
            title: "Snapshots de emuladores Android",
            explanation: "Estados salvos para o emulador abrir rápido; sem eles, ele só faz uma inicialização completa da próxima vez.",
            risk: Risk::Seguro,
            action: Action::DeleteContents,
            needs_admin: false,
            hint: None,
        },
        snaps,
    ));

    // Imagens de sistema que nenhum AVD usa. Se algum AVD da árvore não tem `config.ini`
    // conhecido, não dá para saber o que ele usa: nada sai.
    let known_dirs: Vec<Vec<String>> = facts.avds.iter().map(|c| lower_comps(&c.dir)).collect();
    let all_known = found
        .avds
        .iter()
        .all(|&a| known_dirs.contains(&lower_comps(&tree.path(a))));
    if all_known {
        let refs: Vec<Vec<String>> = facts
            .avds
            .iter()
            .flat_map(|c| c.sysdirs.iter().map(|s| sysdir_comps(s)))
            .filter(|r| !r.is_empty())
            .collect();
        let unused: Vec<NodeId> = found
            .images
            .iter()
            .copied()
            .filter(|&i| {
                let p = lower_comps(&tree.path(i));
                ok(i) && !refs.iter().any(|r| p.ends_with(r))
            })
            .collect();
        out.extend(from_nodes(
            tree,
            Spec {
                id: "android_images",
                title: "Imagens de sistema Android sem uso",
                explanation: "Imagens do SDK que nenhum emulador usa; dá para baixar de novo pelo SDK Manager.",
                risk: Risk::Revisar,
                action: Action::Recycle,
                needs_admin: false,
                hint: None,
            },
            unused,
        ));
    }

    out.extend(
        from_nodes(
            tree,
            Spec {
                id: "android_avds",
                title: "Emuladores Android parados",
                explanation: AVDS_EXPLANATION,
                risk: Risk::Revisar,
                action: Action::Recycle,
                needs_admin: false,
                hint: None,
            },
            avd_nodes,
        )
        .map(|mut ins| {
            ins.companions = pairs
                .into_iter()
                .filter(|(a, i)| ins.nodes.contains(a) && ins.nodes.contains(i))
                .collect();
            ins
        }),
    );
    out
}

fn stale_vms(
    tree: &Tree,
    env: &Env,
    found: &Found,
    disks: &[VirtDisk],
    facts: &SysFacts,
    protect: &ProtectCtx,
    now: SystemTime,
) -> Vec<Insight> {
    // O que já é coberto por outra sugestão (ou nunca entra).
    let mut covered: HashSet<NodeId> = found.avds.iter().copied().collect();
    covered.extend(disks.iter().map(|d| d.node));
    covered.extend(
        facts
            .wsl_disks
            .iter()
            .filter_map(|d| find_abs(tree, &d.vhdx)),
    );
    covered.extend(find_abs(tree, &env.local_app_data.join("Docker")));
    for &i in &found.images {
        covered.extend(tree.node(i).parent.and_then(|tag| tree.node(tag).parent));
    }
    let skip_name = |id: NodeId| {
        tree.ancestors(id).iter().any(|&a| {
            let n = tree.node(a).name.to_lowercase();
            SKIP_DIRS.contains(&n.as_str())
        })
    };

    let vm_dirs: HashSet<NodeId> = found.vm_dirs.iter().copied().collect();
    let mut items: Vec<NodeId> = found
        .vm_dirs
        .iter()
        .copied()
        .filter(|&d| {
            tree.node(d).size >= MIN_VM_FILE
                && !under_any(tree, d, &covered)
                && !skip_name(d)
                && !has_ancestor_named(tree, d, &LIVE_DISK_DIRS)
                && stale_node(tree, d, now)
        })
        .collect();
    // Arquivo solto: a pasta inteira tem que estar parada. Um disco-base só leitura (o
    // `system.vmdk` do LDPlayer, o pai de discos de diferenças, o backing file de um qcow2)
    // não muda, mas os vizinhos dele mudam todo dia enquanto a VM é usada.
    items.extend(found.vm_files.iter().copied().filter(|&f| {
        tree.node(f).mtime.is_some_and(|m| older_than(now, m))
            && tree
                .node(f)
                .parent
                .is_some_and(|p| stale_node(tree, p, now))
            && !under_any(tree, f, &covered)
            && !under_any(tree, f, &vm_dirs)
            && !skip_name(f)
            && !is_live_disk(tree, f)
    }));

    let (locked, free): (Vec<NodeId>, Vec<NodeId>) = items
        .into_iter()
        .partition(|&n| protect.is_protected(&tree.path(n)));
    let mut out = Vec::new();
    out.extend(from_nodes(
        tree,
        Spec {
            id: "stale_vms",
            title: "VMs e imagens de disco paradas",
            explanation: "Máquinas virtuais e imagens de disco (.iso, .vhdx, .vdi…) sem modificação há mais de 90 dias. Confira que a VM não está em uso antes de apagar.",
            risk: Risk::Revisar,
            action: Action::Recycle,
            needs_admin: false,
            hint: None,
        },
        free,
    ));
    out.extend(from_nodes(
        tree,
        Spec {
            id: "stale_vms_programs",
            title: "VMs paradas em pastas protegidas",
            explanation: "Discos de máquinas virtuais sem modificação há mais de 90 dias, numa pasta que o deixa-seco não apaga.",
            risk: Risk::Informativo,
            action: Action::None,
            needs_admin: false,
            hint: Some("Em pasta do sistema ou de programa; remova pelo próprio programa ou gerenciador de VMs.".into()),
        },
        locked,
    ));
    out
}

/// Sugestões de virtualização. Só entra o que está dentro da árvore varrida.
pub fn virt_insights(tree: &Tree, env: &Env, facts: &SysFacts, now: SystemTime) -> Vec<Insight> {
    let protect = ProtectCtx::from_env(env);
    let found = scan(tree);
    let disks = virtual_disks(tree, facts, &found, &protect);
    let mut out = Vec::new();
    out.extend(vhdx_compact(tree, &disks));
    out.extend(docker_prune(tree, facts));
    out.extend(android(tree, facts, &found, &protect, now));
    out.extend(stale_vms(tree, env, &found, &disks, facts, &protect, now));
    out
}

// ---------------------------------------------------------------------------------------------
// Coleta de fatos (só em `SysFacts::gather`, no worker)

/// `\\?\C:\x` → `C:\x` (a árvore usa caminhos sem o prefixo).
fn strip_verbatim(s: &str) -> PathBuf {
    let s = s.trim().trim_matches('"');
    if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{r}"))
    } else if let Some(r) = s.strip_prefix(r"\\?\") {
        PathBuf::from(r)
    } else {
        PathBuf::from(s)
    }
}

/// Distros do WSL lidas do registro (`HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss`).
/// Nunca chama `wsl.exe` (ele pode travar ou ligar o WSL).
pub fn gather_wsl_disks() -> Vec<WslDisk> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(lxss) =
        hkcu.open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Lxss", KEY_READ)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for sub in lxss.enum_keys().flatten() {
        let Ok(k) = lxss.open_subkey_with_flags(&sub, KEY_READ) else {
            continue;
        };
        let (Ok(name), Ok(base)) = (
            k.get_value::<String, _>("DistributionName"),
            k.get_value::<String, _>("BasePath"),
        ) else {
            continue;
        };
        let file = k
            .get_value::<String, _>("VhdFileName")
            .ok()
            .filter(|f| !f.trim().is_empty())
            .unwrap_or_else(|| "ext4.vhdx".into());
        out.push(WslDisk {
            name,
            vhdx: strip_verbatim(&base).join(file),
        });
    }
    out
}

/// Disco de dados do Docker Desktop, se existir.
pub fn gather_docker_vhdx(local_app_data: &Path) -> Option<PathBuf> {
    let wsl = local_app_data.join(r"Docker\wsl");
    [
        wsl.join(r"disk\docker_data.vhdx"),
        wsl.join(r"data\ext4.vhdx"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// `1.2GB`, `512.3kB`, `0B` (unidades decimais do Docker) → bytes.
pub(crate) fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "b" | "" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        _ => return None,
    };
    Some((n * mult).round() as u64)
}

/// Soma do `Reclaimable` das linhas de `docker system df --format "{{json .}}"`.
/// `None` se nenhuma linha tiver o campo.
pub fn parse_docker_df(stdout: &str) -> Option<u64> {
    const KEY: &str = "\"Reclaimable\":\"";
    let mut total: Option<u64> = None;
    for line in stdout.lines() {
        let Some(start) = line.find(KEY) else {
            continue;
        };
        let rest = &line[start + KEY.len()..];
        let value = rest.split('"').next().unwrap_or_default();
        let size = value.split_whitespace().next().and_then(parse_size)?;
        total = Some(total.unwrap_or(0) + size);
    }
    total
}

/// `docker.exe` no PATH.
pub(crate) fn docker_exe() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("docker.exe"))
        .find(|p| p.is_file())
}

/// Roda o comando sem janela e devolve a saída se ele terminar bem dentro do prazo; senão
/// mata o processo e devolve `None`. O prazo é duro: depois de matar, espera no máximo 1 s pelo
/// fim do processo e não espera o leitor da saída (um neto pode segurar o pipe aberto; a thread
/// fica solta e termina sozinha quando o pipe fechar).
fn run_with_timeout(mut cmd: std::process::Command, limit: Duration) -> Option<String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::time::Instant;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const TICK: Duration = Duration::from_millis(50);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = tx.send(stdout.read_to_string(&mut s).ok().map(|_| s));
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(TICK),
            _ => {
                let _ = child.kill();
                let give_up = Instant::now() + Duration::from_secs(1);
                while Instant::now() < give_up && !matches!(child.try_wait(), Ok(Some(_))) {
                    std::thread::sleep(TICK);
                }
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    // Terminou bem: a saída chega logo, a menos que um neto segure o pipe.
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(500));
    rx.recv_timeout(left).ok().flatten()
}

/// Quanto o `docker system prune` liberaria. `None` se o Docker não estiver instalado, não
/// responder em 5 s ou falhar (daemon parado).
pub fn gather_docker_reclaimable() -> Option<u64> {
    let exe = docker_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["system", "df", "--format", "{{json .}}"]);
    parse_docker_df(&run_with_timeout(cmd, Duration::from_secs(5))?)
}

/// Valor de `chave=valor` num `.ini` do emulador.
fn ini_values<'a>(text: &'a str, pred: impl Fn(&str) -> bool + 'a) -> Vec<&'a str> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| pred(k.trim()))
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
        .collect()
}

/// `image.sysdir.1`, `image.sysdir.2`… do `config.ini`.
pub fn parse_sysdirs(config_ini: &str) -> Vec<String> {
    ini_values(config_ini, |k| {
        k.to_ascii_lowercase().starts_with("image.sysdir.")
    })
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Pastas onde o emulador procura AVDs, das variáveis de ambiente.
fn avd_homes() -> Vec<PathBuf> {
    let var = |k: &str| {
        std::env::var_os(k)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let mut v: Vec<PathBuf> = Vec::new();
    v.extend(var("ANDROID_AVD_HOME"));
    v.extend(var("ANDROID_USER_HOME").map(|p| p.join("avd")));
    v.extend(var("ANDROID_SDK_HOME").map(|p| p.join(r".android\avd")));
    v.extend(var("ANDROID_PREFS_ROOT").map(|p| p.join(r".android\avd")));
    v.extend(var("USERPROFILE").map(|p| p.join(r".android\avd")));
    let mut seen = HashSet::new();
    v.retain(|p| seen.insert(lower_comps(p)));
    v
}

/// AVDs conhecidos: `.ini` (com `path=`) e pastas `.avd` nas pastas de AVD. Lê só arquivos
/// pequenos; erros só pulam o item.
pub fn gather_avds() -> Vec<AvdConfig> {
    let mut out: Vec<AvdConfig> = Vec::new();
    let mut add = |dir: PathBuf, ini: Option<PathBuf>| {
        if let Some(c) = out
            .iter_mut()
            .find(|c| lower_comps(&c.dir) == lower_comps(&dir))
        {
            if c.ini.is_none() {
                c.ini = ini;
            }
            return;
        }
        let Ok(text) = std::fs::read_to_string(dir.join("config.ini")) else {
            return;
        };
        out.push(AvdConfig {
            sysdirs: parse_sysdirs(&text),
            dir,
            ini,
        });
    };
    for home in avd_homes() {
        let Ok(entries) = std::fs::read_dir(&home) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name.ends_with(".ini") && p.is_file() {
                let Ok(text) = std::fs::read_to_string(&p) else {
                    continue;
                };
                let target = ini_values(&text, |k| k.eq_ignore_ascii_case("path"))
                    .first()
                    .map(|s| PathBuf::from(*s))
                    .filter(|p| p.is_absolute())
                    .unwrap_or_else(|| p.with_extension("avd"));
                add(target, Some(p));
            } else if name.ends_with(".avd") && p.is_dir() {
                add(p, None);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    const DAY: Duration = Duration::from_secs(86_400);
    const LOCAL: &str = r"C:\Users\t\AppData\Local";
    const UBUNTU: &str = r"C:\Users\t\AppData\Local\Packages\Ubuntu\LocalState\ext4.vhdx";
    const DOCKER: &str = r"C:\Users\t\AppData\Local\Docker\wsl\disk\docker_data.vhdx";

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + DAY * 1000
    }

    fn ago(days: u32) -> Option<SystemTime> {
        Some(now() - DAY * days)
    }

    fn env() -> Env {
        Env::for_test("C:\\")
    }

    /// Cria as pastas que faltarem e o arquivo no fim do caminho absoluto.
    fn put(t: &mut Tree, path: &str, size: u64, days: u32) -> NodeId {
        let p = Path::new(path);
        let rel = p.strip_prefix(t.path(t.root())).expect("dentro da raiz");
        let comps: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let mut cur = t.root();
        for (i, c) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let found = t
                .children(cur)
                .iter()
                .copied()
                .find(|&k| &*t.node(k).name == c.as_str());
            cur = match found {
                Some(k) => k,
                None if last => t.add(cur, c, false, size, ago(days)),
                None => t.add(cur, c, true, 0, None),
            };
        }
        cur
    }

    fn facts() -> SysFacts {
        SysFacts {
            wsl_disks: vec![WslDisk {
                name: "Ubuntu".into(),
                vhdx: UBUNTU.into(),
            }],
            docker_vhdx: Some(DOCKER.into()),
            ..SysFacts::default()
        }
    }

    fn get<'a>(v: &'a [Insight], id: &str) -> Option<&'a Insight> {
        v.iter().find(|i| i.id == id)
    }

    fn paths(t: &Tree, i: &Insight) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = i.nodes.iter().map(|&n| t.path(n)).collect();
        v.sort();
        v
    }

    #[test]
    fn wsl_and_docker_disks_become_vhdx_compact() {
        let mut t = Tree::new("C:\\".into());
        put(&mut t, UBUNTU, 6 * GB, 200);
        put(&mut t, DOCKER, 5 * GB, 200);
        put(
            &mut t,
            &format!(r"{LOCAL}\Docker\wsl\main\ext4.vhdx"),
            GB,
            200,
        );
        t.finalize();
        let mut f = facts();
        f.wsl_disks.push(WslDisk {
            name: "docker-desktop".into(),
            vhdx: format!(r"{LOCAL}\Docker\wsl\main\ext4.vhdx").into(),
        });
        let v = virt_insights(&t, &env(), &f, now());
        let i = get(&v, "vhdx_compact").expect("vhdx_compact");
        assert_eq!(i.size, 12 * GB);
        assert_eq!(i.nodes.len(), 3);
        assert_eq!(i.risk, Risk::Seguro);
        assert_eq!(i.action, Action::CompactVhdx);
        assert!(i.needs_admin);
        let hint = i.hint.as_deref().unwrap();
        assert!(hint.starts_with("Discos: Ubuntu, Docker."), "{hint}");
        assert!(hint.contains("wsl --unregister"));
        // Um disco por item, maior primeiro: caminho, rótulo e distro (só a do WSL de verdade).
        let main = format!(r"{LOCAL}\Docker\wsl\main\ext4.vhdx");
        let got: Vec<(PathBuf, &str, Option<&str>)> = i
            .disks
            .iter()
            .map(|d| (d.path.clone(), d.label.as_str(), d.distro.as_deref()))
            .collect();
        assert_eq!(
            got,
            vec![
                (PathBuf::from(UBUNTU), "Ubuntu", Some("Ubuntu")),
                (PathBuf::from(DOCKER), "Docker", None),
                (PathBuf::from(main), "Docker", None),
            ]
        );
        for d in &i.disks {
            assert_eq!(t.path(d.node), d.path);
            assert!(i.nodes.contains(&d.node));
        }
        // Discos do WSL e do Docker (mesmo velhos e grandes) nunca em `stale_vms`.
        assert!(get(&v, "stale_vms").is_none(), "{v:#?}");
    }

    #[test]
    fn disks_outside_the_scan_are_ignored() {
        let mut t = Tree::new("D:\\".into());
        put(&mut t, r"D:\x\a.txt", 10, 1);
        t.finalize();
        let mut f = facts();
        f.docker_reclaimable = Some(3 * GB);
        assert!(virt_insights(&t, &env(), &f, now()).is_empty());
    }

    fn docker_tree() -> Tree {
        let mut t = Tree::new("C:\\".into());
        put(&mut t, DOCKER, 5 * GB, 1);
        t.finalize();
        t
    }

    #[test]
    fn docker_prune_uses_reclaimable() {
        let t = docker_tree();
        let mut f = facts();
        f.docker_reclaimable = Some(3 * GB);
        let v = virt_insights(&t, &env(), &f, now());
        let i = get(&v, "docker_prune").expect("docker_prune");
        assert_eq!(i.size, 3 * GB);
        assert_eq!(i.risk, Risk::Revisar);
        assert_eq!(i.action, Action::DockerPrune);
        assert_eq!(i.title, "Lixo do Docker");
        // Nada para limpar: some.
        f.docker_reclaimable = Some(0);
        assert!(get(&virt_insights(&t, &env(), &f, now()), "docker_prune").is_none());
    }

    #[test]
    fn docker_not_running_is_informative() {
        let t = docker_tree();
        let v = virt_insights(&t, &env(), &facts(), now());
        let i = get(&v, "docker_prune").expect("docker_prune");
        assert_eq!(i.risk, Risk::Informativo);
        assert_eq!(i.action, Action::None);
        assert!(i.explanation.contains("Abra o Docker Desktop"));
    }

    const SDK: &str = r"D:\Android\Sdk\system-images";
    const AVD: &str = r"D:\Android\avd";

    fn android_tree(pixel_days: u32) -> Tree {
        let mut t = Tree::new("D:\\".into());
        put(
            &mut t,
            &format!(r"{SDK}\android-36\google_apis\x86_64\system.img"),
            4 * GB,
            300,
        );
        put(
            &mut t,
            &format!(r"{SDK}\android-30\google_apis\x86\system.img"),
            GB,
            300,
        );
        put(
            &mut t,
            &format!(r"{AVD}\Pixel.avd\config.ini"),
            1,
            pixel_days,
        );
        put(
            &mut t,
            &format!(r"{AVD}\Pixel.avd\userdata-qemu.img.qcow2"),
            2 * GB,
            pixel_days,
        );
        put(
            &mut t,
            &format!(r"{AVD}\Pixel.avd\snapshots\default_boot\ram.img"),
            5 * GB / 2,
            pixel_days,
        );
        put(&mut t, &format!(r"{AVD}\Pixel.ini"), 1, pixel_days);
        t.finalize();
        t
    }

    fn android_facts() -> SysFacts {
        SysFacts {
            avds: vec![AvdConfig {
                dir: format!(r"{AVD}\Pixel.avd").into(),
                ini: Some(format!(r"{AVD}\Pixel.ini").into()),
                sysdirs: vec![r"system-images\android-36\google_apis\x86_64\".into()],
            }],
            ..SysFacts::default()
        }
    }

    #[test]
    fn unreferenced_system_images() {
        let t = android_tree(5);
        let v = virt_insights(&t, &env(), &android_facts(), now());
        let i = get(&v, "android_images").expect("android_images");
        assert_eq!(
            paths(&t, i),
            [PathBuf::from(format!(r"{SDK}\android-30\google_apis\x86"))]
        );
        assert_eq!(i.size, GB);
        assert_eq!(i.risk, Risk::Revisar);
        assert_eq!(i.action, Action::Recycle);
        // Imagens e AVDs nunca em `stale_vms` (o .qcow2 e o system.img são grandes e velhos).
        assert!(get(&v, "stale_vms").is_none(), "{v:#?}");
    }

    #[test]
    fn unknown_avd_config_keeps_all_images() {
        let t = android_tree(5);
        assert!(
            get(
                &virt_insights(&t, &env(), &SysFacts::default(), now()),
                "android_images"
            )
            .is_none()
        );
    }

    #[test]
    fn snapshots_are_safe() {
        let t = android_tree(5);
        let v = virt_insights(&t, &env(), &android_facts(), now());
        let i = get(&v, "android_snapshots").expect("android_snapshots");
        assert_eq!(i.risk, Risk::Seguro);
        assert_eq!(i.action, Action::DeleteContents);
        assert_eq!(i.size, 5 * GB / 2);
        assert_eq!(
            paths(&t, i),
            [PathBuf::from(format!(r"{AVD}\Pixel.avd\snapshots"))]
        );
        assert!(get(&v, "android_avds").is_none(), "AVD recente fica");
    }

    #[test]
    fn stale_avd_with_its_ini() {
        let t = android_tree(200);
        let v = virt_insights(&t, &env(), &android_facts(), now());
        let i = get(&v, "android_avds").expect("android_avds");
        assert_eq!(i.risk, Risk::Revisar);
        assert_eq!(i.action, Action::Recycle);
        assert_eq!(
            paths(&t, i),
            [
                PathBuf::from(format!(r"{AVD}\Pixel.avd")),
                PathBuf::from(format!(r"{AVD}\Pixel.ini")),
            ]
        );
        // A pasta e o `.ini` são um item só: o `.ini` acompanha a pasta.
        let avd = t.find(Path::new(&format!(r"{AVD}\Pixel.avd"))).unwrap();
        let ini = t.find(Path::new(&format!(r"{AVD}\Pixel.ini"))).unwrap();
        assert_eq!(i.companions, vec![(avd, ini)]);
        assert!(
            get(&v, "android_snapshots").is_none(),
            "snapshot de AVD parado já sai com ele"
        );
    }

    #[test]
    fn base_disk_next_to_a_live_disk_is_not_stale() {
        // Disco-base só leitura de 200 dias ao lado do disco de dados usado há 5 dias.
        let mut t = Tree::new("C:\\".into());
        put(&mut t, r"C:\LDPlayer\vms\leidian0\base.vmdk", GB, 200);
        put(&mut t, r"C:\LDPlayer\vms\leidian0\data.vmdk", GB, 5);
        // Pasta inteira parada: entra.
        put(&mut t, r"C:\VMs\velha\base.vmdk", GB, 200);
        put(&mut t, r"C:\VMs\velha\data.vmdk", GB, 150);
        t.finalize();
        let v = virt_insights(&t, &env(), &SysFacts::default(), now());
        let i = get(&v, "stale_vms").expect("stale_vms");
        assert_eq!(
            paths(&t, i),
            [
                PathBuf::from(r"C:\VMs\velha\base.vmdk"),
                PathBuf::from(r"C:\VMs\velha\data.vmdk"),
            ]
        );
    }

    #[test]
    fn stale_vdi_outside_program_files() {
        let mut t = Tree::new("C:\\".into());
        put(&mut t, r"C:\VMs\win.vdi", GB, 200);
        put(&mut t, r"C:\VMs\small.iso", 100 * MB, 200);
        put(&mut t, r"C:\Outro\new.iso", GB, 10);
        put(&mut t, r"C:\VMs\notes.txt", GB, 200);
        t.finalize();
        let v = virt_insights(&t, &env(), &SysFacts::default(), now());
        let i = get(&v, "stale_vms").expect("stale_vms");
        assert_eq!(paths(&t, i), [PathBuf::from(r"C:\VMs\win.vdi")]);
        assert_eq!(i.size, GB);
        assert_eq!(i.risk, Risk::Revisar);
        assert_eq!(i.action, Action::Recycle);
        assert_eq!(i.title, "VMs e imagens de disco paradas");
    }

    #[test]
    fn stale_vdi_inside_program_files_is_informative() {
        let mut t = Tree::new("D:\\".into());
        put(&mut t, r"D:\Program Files\MuMu\vms\a\disk.vdi", GB, 200);
        t.finalize();
        let v = virt_insights(&t, &env(), &SysFacts::default(), now());
        assert!(get(&v, "stale_vms").is_none());
        let i = get(&v, "stale_vms_programs").expect("informativo");
        assert_eq!(i.risk, Risk::Informativo);
        assert_eq!(i.action, Action::None);
        assert_eq!(i.size, GB);
        assert!(
            i.hint
                .as_deref()
                .unwrap()
                .contains("próprio programa ou gerenciador de VMs")
        );
    }

    #[test]
    fn known_vm_folders_grouped_and_skipped_dirs() {
        let mut t = Tree::new("C:\\".into());
        put(
            &mut t,
            r"C:\Users\t\VirtualBox VMs\Win10\Win10.vdi",
            2 * GB,
            200,
        );
        put(
            &mut t,
            r"C:\Users\t\VirtualBox VMs\Win10\Win10.vbox",
            1,
            200,
        );
        let geny = format!(r"{LOCAL}\Genymobile\Genymotion\deployed\Pixel");
        put(&mut t, &format!(r"{geny}\disk.vmdk"), 3 * GB, 400);
        put(&mut t, &format!(r"{geny}\ram.img"), GB, 300);
        put(
            &mut t,
            r"C:\Program Files\WindowsApps\x\disk.vhdx",
            2 * GB,
            400,
        );
        t.finalize();
        let v = virt_insights(&t, &env(), &SysFacts::default(), now());
        let i = get(&v, "stale_vms").expect("stale_vms");
        assert_eq!(
            paths(&t, i),
            [
                PathBuf::from(format!(r"{LOCAL}\Genymobile\Genymotion\deployed\Pixel")),
                PathBuf::from(r"C:\Users\t\VirtualBox VMs\Win10"),
            ]
        );
        assert_eq!(i.size, 6 * GB + 1);
        assert!(get(&v, "stale_vms_programs").is_none(), "{v:#?}");
    }

    #[test]
    fn docker_disk_in_custom_place_goes_to_compact_not_stale() {
        let mut t = Tree::new("D:\\".into());
        put(
            &mut t,
            r"D:\DockerDesktopWSL\disk\docker_data.vhdx",
            20 * GB,
            300,
        );
        put(&mut t, r"D:\distros\deb\ext4.vhdx", 3 * GB, 300);
        put(&mut t, r"D:\tmp\swap.vhdx", GB, 300);
        put(&mut t, r"D:\wsl\backup\old.vhdx", GB, 300);
        put(&mut t, r"D:\VMs\old.vhdx", GB, 300);
        t.finalize();
        let v = virt_insights(&t, &env(), &SysFacts::default(), now());
        let c = get(&v, "vhdx_compact").expect("vhdx_compact");
        assert_eq!(
            paths(&t, c),
            [
                PathBuf::from(r"D:\DockerDesktopWSL\disk\docker_data.vhdx"),
                PathBuf::from(r"D:\distros\deb\ext4.vhdx"),
            ]
        );
        let hint = c.hint.as_deref().unwrap();
        assert!(hint.starts_with("Discos: Docker, WSL."), "{hint}");
        let s = get(&v, "stale_vms").expect("stale_vms");
        assert_eq!(paths(&t, s), [PathBuf::from(r"D:\VMs\old.vhdx")]);
        assert!(s.explanation.contains("Confira que a VM não está em uso"));
    }

    #[test]
    fn timeout_is_hard() {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", "ping -n 20 127.0.0.1 >nul"]);
        let t0 = std::time::Instant::now();
        assert_eq!(run_with_timeout(cmd, Duration::from_millis(300)), None);
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        let mut ok = std::process::Command::new("cmd");
        ok.args(["/c", "echo oi"]);
        assert_eq!(
            run_with_timeout(ok, Duration::from_secs(5))
                .as_deref()
                .map(str::trim),
            Some("oi")
        );
    }

    #[test]
    fn docker_df_parsing() {
        let out = r#"{"Active":"2","Reclaimable":"1.2GB (50%)","Size":"2.4GB","TotalCount":"5","Type":"Images"}
{"Active":"0","Reclaimable":"512.3kB (100%)","Size":"512.3kB","TotalCount":"1","Type":"Containers"}
{"Active":"0","Reclaimable":"0B","Size":"0B","TotalCount":"0","Type":"Local Volumes"}
{"Active":"0","Reclaimable":"300MB","Size":"300MB","TotalCount":"3","Type":"Build Cache"}
"#;
        assert_eq!(
            parse_docker_df(out),
            Some(1_200_000_000 + 512_300 + 300_000_000)
        );
        assert_eq!(parse_docker_df(""), None);
        assert_eq!(parse_docker_df("error during connect"), None);
        assert_eq!(parse_size("2.297GB"), Some(2_297_000_000));
        assert_eq!(parse_size("1TB"), Some(1_000_000_000_000));
        assert_eq!(parse_size("abc"), None);
    }

    #[test]
    fn ini_parsing() {
        let cfg = "avd.ini.encoding=UTF-8\nimage.sysdir.1=system-images\\android-36\\google_apis\\x86_64\\\nhw.ramSize=2048\n";
        assert_eq!(
            parse_sysdirs(cfg),
            [r"system-images\android-36\google_apis\x86_64\"]
        );
        assert_eq!(
            strip_verbatim(r"\\?\C:\Users\x\Docker\wsl\main"),
            PathBuf::from(r"C:\Users\x\Docker\wsl\main")
        );
        assert_eq!(strip_verbatim(r"C:\a"), PathBuf::from(r"C:\a"));
    }

    #[test]
    #[ignore = "lê o registro, roda `docker system df` e varre %LOCALAPPDATA%\\Docker e o AVD home"]
    fn virt_smoke() {
        let t0 = std::time::Instant::now();
        let facts = SysFacts::gather();
        println!("gather em {:?}", t0.elapsed());
        println!("wsl_disks: {:#?}", facts.wsl_disks);
        println!("docker_vhdx: {:?}", facts.docker_vhdx);
        println!("docker_reclaimable: {:?}", facts.docker_reclaimable);
        println!("avds: {:#?}", facts.avds);
        let env = Env::from_system();
        let mut roots = vec![env.local_app_data.join("Docker")];
        // Pasta acima de `avd` (ex.: `D:\Android`, com o SDK junto).
        roots.extend(
            facts
                .avds
                .iter()
                .filter_map(|a| a.dir.parent()?.parent().map(Path::to_path_buf)),
        );
        for root in roots.into_iter().filter(|r| r.is_dir()) {
            let tree = crate::scan::walk::walk_scan(&root, &mut |_, _| {}).unwrap();
            println!("--- {} ({} nós)", root.display(), tree.len());
            for i in virt_insights(&tree, &env, &facts, SystemTime::now()) {
                println!(
                    "{} [{:?}] {} — {} B, {} nós, hint: {:?}",
                    i.id,
                    i.risk,
                    i.title,
                    i.size,
                    i.nodes.len(),
                    i.hint
                );
                for &n in &i.nodes {
                    println!("    {} ({} B)", tree.path(n).display(), tree.node(n).size);
                }
            }
        }
    }
}
