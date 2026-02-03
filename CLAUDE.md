# CLAUDE.md

## What
LeopardWM — a scroll-first tiling window manager for Windows 10/11.
Rust workspace, MSVC toolchain (`stable-x86_64-pc-windows-msvc`).

## Crates
| Crate | What it does |
|---|---|
| `core_layout` | Platform-agnostic scrolling layout engine |
| `platform_win32` | Win32 APIs, DwmFlush animation engine |
| `ipc` | Named-pipe command/response protocol |
| `daemon` | Event loop, state, message-pump threads, tray, settings WebView |
| `cli` | User-facing CLI |

All crates live under `crates/`. Internal names still use `leopardwm` (rename is future work).

## Commands
```
cargo build --release
cargo test --all
```

## Workflow
- Plan before editing if the change touches 3+ files or involves architectural decisions.
- Prefer reuse — search for existing functions/patterns before adding new code.
- Verify with tests or logs before declaring done.
- Keep changes minimal and scoped; avoid unrelated refactors.

## Extended policies
See `AGENTS.md` for agent-agnostic policies that apply across all coding agents.
