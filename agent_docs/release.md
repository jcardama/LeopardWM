# Release Process

Guide for all agents and contributors releasing LeopardWM.

## Version Format

Semantic versioning: `vX.Y.Z` (e.g., `v0.1.5`).
Version is set in the workspace `Cargo.toml` under `[workspace.package]`.

## Release Workflow

The GitHub Actions workflow (`.github/workflows/release.yml`) triggers on tag pushes matching `v*`.

**What it does:**
1. Checks out code on `windows-latest`
2. Builds release: `cargo build --release`
3. Runs tests: `cargo test --all`
4. Packages: `leopardwm.exe` + `leopardwm-cli.exe` + `README.md` + `LICENSE` into `LeopardWM-{version}-x86_64-windows.zip`
5. Extracts release notes from `CHANGELOG.md` (section matching `## X.Y.Z` or `## [X.Y.Z]`)
6. Creates GitHub Release with the zip and notes (falls back to auto-generated notes if no changelog section found)

Binary path: `target/x86_64-pc-windows-msvc/release/` (explicit target in `.cargo/config.toml`).

## Changelog Format

Conventional Commits style in `CHANGELOG.md`:

```markdown
## 0.2.0

### Features
- Add workspace switching via Ctrl+Alt+1-9

### Improvements
- Improve border rendering performance on multi-monitor setups

### Bug Fixes
- Fix transient window suppression for Beeper desktop app
```

Section header: `## X.Y.Z` (no `v` prefix, brackets optional).

## Pre-Release Checklist

1. Update `CHANGELOG.md` with all notable changes
2. Bump `version` in workspace `Cargo.toml`
3. Run full CI locally: `cargo test --all && cargo clippy --all -- -D warnings`
4. Commit: `chore: prepare release X.Y.Z`
5. Tag: `git tag vX.Y.Z`
6. Push tag: `git push origin vX.Y.Z`
7. Monitor the release workflow on GitHub Actions

## Branch Protection (main)

- Required status check: `ci` (strict — branch must be up to date)
- Required approving reviews: 1
- Required linear history (rebase or squash merge only)
- Enforce admins: disabled (admin bypass available)
- Force pushes: not allowed
