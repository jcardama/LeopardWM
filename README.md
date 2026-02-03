# LeopardWM

[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
![Platform: Windows 10/11](https://img.shields.io/badge/Platform-Windows%2010%2F11-0078D4)

A scrollable tiling window manager for Windows.

## What Makes It Different

Most Windows tilers use tree or BSP layouts. LeopardWM is **scroll-first**: windows sit on a horizontal strip, and your monitor acts as a viewport that scrolls over them. Navigation stays spatially consistent as windows are added — you move through context instead of constantly rebuilding split trees.

- **Vsync-aligned animations** — smooth scrolling powered by a `DwmFlush`-driven animation engine
- **Written in Rust** — safe, fast, and easy to hack on

## Features

- Multi-monitor workspaces with monitor-aware focus and move
- Global hotkeys with live config reload
- Smooth scroll animations and touchpad gestures
- Floating and fullscreen toggles
- Width presets and column equalization
- System tray with pause, reload, settings, and diagnostics
- WebView-based settings GUI
- Safe mode for troubleshooting (`--safe-mode`)
- Built-in diagnostics (`leopardwm-cli doctor`)
- Workspace persistence and session recovery
- Autostart via Registry

## Quick Start

Prerequisites: [Rust](https://rustup.rs) with the MSVC toolchain (`stable-x86_64-pc-windows-msvc`)

```bash
git clone https://github.com/jcardama/LeopardWM.git
cd LeopardWM
cargo build --release
```

Binaries land in `target/release/`. Generate a default config and start the daemon:

```bash
cargo run -p leopardwm-cli -- init
cargo run -p leopardwm-cli -- run
```

## Default Hotkeys

| Key | Action |
|---|---|
| `Win+H` / `Win+L` | Focus left / right |
| `Win+J` / `Win+K` | Focus down / up |
| `Win+Shift+H` / `Win+Shift+L` | Move column left / right |
| `Win+Ctrl+H` / `Win+Ctrl+L` | Shrink / grow column |
| `Win+Ctrl+Escape` | Emergency restore + panic-revert |
| `Win+Alt+H` / `Win+Alt+L` | Focus monitor left / right |
| `Win+Alt+Shift+H` / `Win+Alt+Shift+L` | Move window to monitor left / right |
| `Win+Shift+Q` | Close focused window |
| `Win+F` | Toggle floating |
| `Win+Shift+F` | Toggle fullscreen |
| `Win+1` / `Win+2` / `Win+3` | Set width to 1/3, 1/2, 2/3 |
| `Win+0` | Equalize all column widths |
| `Win+R` | Refresh (re-enumerate windows) |

## Autostart

```bash
leopardwm-cli autostart enable   # writes HKCU\...\Run entry
leopardwm-cli autostart disable  # removes it
```

## Config & Runtime Paths

> **Note:** Crate names and on-disk paths still use `leopardwm` internally. A full crate rename is future work.

| Item | Path |
|---|---|
| Config | `%APPDATA%\leopardwm\config\config.toml` |
| State | `%APPDATA%\leopardwm\data\workspace-state.json` |
| Log (stdout) | `%TEMP%\leopardwm-daemon.log` |
| Log (stderr) | `%TEMP%\leopardwm-daemon.err.log` |

## Architecture

LeopardWM is a Rust workspace with five crates:

| Crate | Responsibility |
|---|---|
| `leopardwm-core-layout` | Platform-agnostic scrolling layout engine |
| `leopardwm-platform-win32` | Win32 integration, window operations, DwmFlush animation engine |
| `leopardwm-ipc` | Named-pipe command/response protocol |
| `leopardwm-daemon` | Runtime event loop, state management, dedicated message-pump threads |
| `leopardwm-cli` | User-facing CLI |

## Platform Constraints

LeopardWM is a **window controller**, not a compositor. DWM remains the compositor. Elevated or protected windows may reject placement/styling changes, and behavior can vary across app frameworks (Win32, WPF, Electron, UWP).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[GPL-3.0](LICENSE)
