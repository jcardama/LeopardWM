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

## Large files (use offset/limit)
daemon: `main.rs` (1740), `config.rs` (2233), `helpers.rs` (1749), `tests.rs` (2472). cli: `main.rs` (2732). core_layout: `tests.rs` (3045).

## When summarizing this conversation
Preserve: (1) which files were modified and why, (2) current git branch and uncommitted state, (3) in-progress work or unfinished steps, (4) user corrections/preferences from this session, (5) specific error messages being debugged.

## Reference docs (read when relevant)
<important if="editing files in crates/daemon/ or crates/core_layout/ or crates/platform_win32/">
Read `.claude/agent_docs/architecture.md` for crate relationships, module map, and data flow.
</important>
<important if="creating a release, tagging, or updating CHANGELOG.md">
Read `agent_docs/release.md` for the release checklist and changelog format.
</important>
<important if="reading files >500 lines, doing large refactors, or 10+ messages into a session">
Read `.claude/agent_docs/context-management.md` for file read caps, compaction behavior, and verification.
</important>
<important if="task touches >5 files or spans multiple crates">
Read `.claude/agent_docs/context-management.md` — sub-agent swarming section. Launch parallel sub-agents for cross-crate or large-scope work.
</important>

## Extended policies
See `AGENTS.md` for agent-agnostic policies that apply across all coding agents.
