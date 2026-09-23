# LeopardWM

LeopardWM is a scroll-first tiling window manager for Windows 10/11. It is a Rust workspace built with the MSVC toolchain (`stable-x86_64-pc-windows-msvc`), configured in `.cargo/config.toml`.

## Project map

| Crate | What it does |
|---|---|
| `core_layout` | Platform-agnostic scrolling layout engine |
| `platform_win32` | Win32 APIs, DwmFlush animation engine |
| `ipc` | Named-pipe command/response protocol |
| `daemon` | Event loop, state, message-pump threads, tray, settings WebView |
| `cli` | User-facing CLI |

All crates live under `crates/`; internal names still use `leopardwm`. `tools/` holds the desktop acceptance and diagnostics scripts, `wix/` the MSI installer, and `agent_docs/` the maintainer docs listed below.

<important if="you need to run project commands or verify code changes">

Run commands from the repository root.

- During iteration, run the smallest relevant check, such as `cargo test -p <crate>`.
- Before reporting a completed change, run `pwsh -NoProfile -File tools/check.ps1`.
- The check runs Clippy, the workspace tests, and the tools tests in order. Each passing stage prints one line; the first failing stage prints its first diagnostic and stops the run. Fix that stage and rerun; do not add output pipes or truncation.

| Command | What it does |
|---|---|
| `pwsh -NoProfile -File tools/check.ps1` | Final validation: Clippy, workspace tests, and tools tests |
| `cargo build --release` | Build release binaries |
| `cargo test --workspace` | Run the workspace tests |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint with warnings as errors |
| `python -m unittest discover -s tools -p test_desktop_acceptance.py` | Test the desktop acceptance tooling |

</important>

## Policies

- Plan first for non-trivial changes (3+ files or architectural decisions).
- Prefer reuse over new code; search existing patterns before adding.
- Verify before done: tests, logs, or diffs.
- Prefer minimal, scoped changes; avoid unrelated refactors.
- Do not edit generated files or vendor folders unless explicitly asked.
- Check `git status` before destructive operations.

<important if="editing files in crates/daemon/, crates/core_layout/, or crates/platform_win32/">
Read `agent_docs/architecture.md` for crate relationships, the daemon module map, and data flow.
</important>

<important if="creating a release, tagging, or updating CHANGELOG.md">
Read `agent_docs/release.md` for the release checklist and changelog format.
</important>

<important if="changing IPC commands, events, or the hotkey query">
Read `agent_docs/ipc-events.md` for the wire contract and `agent_docs/shortcut-guide.md` for the hotkey query and Shortcut Guide export.
</important>

<important if="working on window placement or GitHub issues #104 or #112">
Read `agent_docs/placement-report-triage.md` for the evidence gathered so far.
</important>

<important if="setting up winget publishing or code signing">
Read `agent_docs/distribution_setup.md`.
</important>
