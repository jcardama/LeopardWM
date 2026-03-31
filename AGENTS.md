# Agent Guide — LeopardWM

Policies for all AI coding agents working in this repo.
Agent-specific context belongs in the agent's own config file (e.g. `CLAUDE.md`).

## Commands
```
cargo build --release
cargo test --all
```

## Policies
- MSVC toolchain only — configured in `.cargo/config.toml`.
- Plan first for non-trivial changes (3+ files or architectural decisions).
- Prefer reuse over new code — search existing patterns before adding.
- Verify before done — tests, logs, or diffs.
- Prefer minimal, scoped changes; avoid unrelated refactors.
- Do not edit generated files or vendor folders unless explicitly asked.
- Check `git status` before destructive operations.
- See `agent_docs/release.md` for the release process, changelog format, and pre-release checklist.
