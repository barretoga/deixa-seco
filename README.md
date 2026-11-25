# deixa-seco

**A fast disk space analyzer for Windows that shows where your space went — and helps you get it back.**

`deixa-seco` (Brazilian Portuguese slang, roughly *"leave it dry"*) scans a drive, draws it as an interactive treemap, explains what is taking up space in plain categories, and runs the cleanup for you — always behind a confirmation dialog. It is written in Rust with [egui](https://github.com/emilk/egui) and ships as a single executable.

> The user interface is in Brazilian Portuguese.

![Treemap of a drive, colored by the kind of content each folder holds](docs/images/map.png)

*Folder names and paths in the screenshots are pixelated on purpose.*

## Features

- **Fast scanning.** With admin rights it reads the NTFS Master File Table directly; otherwise it falls back to a parallel folder walk. The map starts drawing while the scan is still running.
- **Treemap that tells you what is inside.** Each folder is colored by the kind of content that dominates it (video, images, code, games/data, caches…), with a legend. Sizes are the space actually **allocated on disk**, which is what you get back when you delete.
- **Easy navigation.** Double-click to zoom into a folder, clickable breadcrumb, back/forward (Backspace, mouse side buttons), adjustable depth, and `Ctrl+F` search that jumps to the result.
- **Right-click actions on any block.** Open in Explorer, copy path, open a terminal there, Windows properties, or send to the Recycle Bin.
- **Cleanup suggestions**, each with a risk level (*Seguro* = safe, *Revisar* = review, *Info* = informational):
  - temporary files, browser caches, Windows Update download cache, developer caches (npm, pip, cargo, gradle, NuGet);
  - `node_modules`, `target`, `build`/`dist`, `.venv` of projects untouched for 90+ days;
  - Downloads older than 180 days;
  - installed programs (registry and Microsoft Store), only those that live on the scanned drive, with their official uninstallers;
  - WSL and Docker virtual disks (compaction), unused Docker images and build cache, Android emulator snapshots, unused system images and idle emulators, and idle VM disks/ISOs;
  - Recycle Bin, `pagefile.sys`, `hiberfil.sys` (informational, with tips).
- **Safe by design.** Nothing runs without confirmation. Regenerable caches are deleted for good; anything you pick yourself goes to the Recycle Bin. System folders (`Windows`, `Program Files`, `ProgramData`, boot files) and OneDrive placeholders are never touched; junctions and symlinks are never followed.

| Live map while scanning | Right-click menu |
|---|---|
| ![Live map while scanning](docs/images/live-scan.png) | ![Context menu on a tile](docs/images/context-menu.png) |

| Suggestions panel | Confirmation before anything runs |
|---|---|
| ![Suggestions panel](docs/images/suggestions.png) | ![Confirmation dialog](docs/images/confirm.png) |

## How it works

### Architecture

```mermaid
flowchart LR
    subgraph Scan["Scanning (background thread)"]
        MFT["MFT reader<br/>(NTFS + admin)"]
        WALK["Parallel folder walk<br/>(jwalk, fallback)"]
    end
    TREE[("Arena tree<br/>NodeId → name, size, mtime")]
    subgraph Analysis["Analysis (worker threads)"]
        LAYOUT["Squarified treemap<br/>layout"]
        KINDS["Dominant kind<br/>per folder"]
        INSIGHTS["Suggestion rules<br/>(known paths, dev, programs,<br/>Store, virtualization)"]
        INDEX["Search index"]
    end
    subgraph UI["UI thread (egui)"]
        MAP["Treemap view<br/>+ navigation + search"]
        SIDE["Suggestions panel"]
        CONFIRM["Confirmation dialog"]
    end
    CLEAN["Cleanup engine<br/>(protection checks, Recycle Bin,<br/>delete, uninstall, compact, prune)"]

    MFT --> TREE
    WALK --> TREE
    WALK -. "previews every ~1 s" .-> MAP
    TREE --> LAYOUT --> MAP
    TREE --> KINDS --> MAP
    TREE --> INSIGHTS --> SIDE
    TREE --> INDEX --> MAP
    MAP -- "right-click → Recycle" --> CONFIRM
    SIDE -- "selected items" --> CONFIRM
    CONFIRM -- "user confirms" --> CLEAN
    CLEAN -- "done paths / new sizes" --> TREE
```

The UI thread never does heavy work: scans, analysis, the search index and every cleanup run on background threads and hand results back through channels.

### From launch to cleanup

```mermaid
sequenceDiagram
    actor U as You
    participant A as deixa-seco
    participant S as Scanner
    participant W as Workers
    participant C as Cleanup engine

    A->>U: UAC prompt (optional, enables MFT scan)
    U->>A: pick a drive or folder
    A->>S: start scan
    loop every ~1 s (folder walk)
        S-->>A: partial tree preview
    end
    S-->>A: full tree
    A->>W: build map, colors, search index, suggestions
    W-->>A: suggestions (sizes, risk, actions)
    U->>A: tick items, click "Limpar selecionados"
    A->>U: confirmation: what goes for good, what goes to the Recycle Bin, what is skipped
    U->>A: confirm
    A->>C: run plan
    C-->>A: freed bytes, skipped items with reasons
    A->>A: update map and suggestions without rescanning
    A->>U: summary + offer to empty the Recycle Bin
```

### Safety layers

```mermaid
flowchart TD
    PICK["Selected item"] --> PLAN{"Plan:<br/>is the path protected?"}
    PLAN -- "Windows, Program Files, ProgramData,<br/>volume root, boot files, profile folders" --> REJECT["Listed as<br/>'Não vou mexer nisto'"]
    PLAN -- ok --> DIALOG["Confirmation dialog<br/>(Cancel is the default)"]
    DIALOG -- cancel --> STOP["Nothing happens"]
    DIALOG -- confirm --> RECHECK{"Execution:<br/>re-check the real path<br/>(junctions, 8.3 names)"}
    RECHECK -- protected --> SKIP["Skipped with reason"]
    RECHECK -- ok --> KIND{"Kind of item"}
    KIND -- "cache / temp" --> DEL["Delete permanently<br/>(regenerated by its owner)"]
    KIND -- "your files" --> BIN["Recycle Bin"]
    KIND -- "program" --> UNINST["Official uninstaller"]
    KIND -- "WSL/Docker disk" --> COMPACT["fstrim → wsl --shutdown<br/>→ diskpart compact"]
    KIND -- "reparse point (OneDrive, junction)" --> NOFOLLOW["Never followed / never deleted"]
```

## Requirements

- Windows 10 or 11 (x64)
- [Rust](https://rustup.rs/) 1.85+ (edition 2024) to build
- Admin rights are optional: they enable the fast MFT scan and a few system cleanups (Windows Update cache, VHDX compaction)

## Build and run

```powershell
git clone https://github.com/barretoga/deixa-seco.git
cd deixa-seco
cargo build --release
.\target\release\deixa-seco.exe
```

The app asks for admin rights through UAC when it opens. Decline to run with the regular folder scan, or skip the prompt with:

```powershell
.\target\release\deixa-seco.exe --sem-admin
```

Start scanning a drive right away with `--scan C`.

## Tests

```powershell
cargo test
```

Tests never touch real user files: deletion tests run inside temporary folders, and commands that would stop WSL, run `diskpart`, prune Docker, empty the Recycle Bin or uninstall programs are only built and inspected, never executed.

Some tests need admin rights and are ignored by default. To compare the MFT scan against the folder walk on your C: drive (read-only), run in an elevated terminal:

```powershell
cargo test --release --test mft -- --ignored --nocapture
```

## Project layout

| Path | What it does |
|---|---|
| `src/scan/` | MFT reader, parallel folder walk, scan thread and live previews |
| `src/tree.rs` | Arena tree of files and folders (sizes, search, removal, resize) |
| `src/treemap.rs` | Squarified treemap layout |
| `src/kind.rs` | File kinds and per-folder dominant kind |
| `src/insights/` | Suggestion rules: known caches, dev artifacts, programs, Store apps, virtualization |
| `src/cleanup.rs` | Protection checks and execution of every cleanup action |
| `src/ui/` | egui screens: drive list, map, navigation, search, suggestions, confirmation |
| `docs/images/` | Screenshots used in this README (folder names and paths are pixelated) |

## Inspiration

The treemap idea is inspired by [SpaceSniffer](http://www.uderzo.it/main_products/space_sniffer/). deixa-seco is an independent implementation and shares no code or assets with it.

## License

[MIT](LICENSE)
