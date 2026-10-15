# Release Process

Guide for all agents and contributors releasing LeopardWM.

## Version Format

Semantic versioning: `vX.Y.Z` (for example, `v0.2.6`).
Version is set in the workspace `Cargo.toml` under `[workspace.package]`.

## Release Workflow

`.github/workflows/release.yml` triggers when a tag matching `v*` is pushed.
The tag push is the publication boundary; do not build or publish release assets manually.

The workflow:

1. Checks out the tagged commit on `windows-latest` with the stable MSVC toolchain.
2. Builds release binaries for both `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`.
3. Verifies that `leopardwm.exe` and `leopardwm-watchdog.exe` use the Windows GUI subsystem on all built targets.
4. Runs `cargo test --all --target x86_64-pc-windows-msvc` on the x64 GitHub-hosted runner.
5. Packages `leopardwm.exe`, `leopardwm-cli.exe`, `lwm.exe`, `leopardwm-watchdog.exe`, `README.md`, and `LICENSE` into `LeopardWM-{version}-x86_64-windows.zip` and `LeopardWM-{version}-arm64-windows.zip`.
6. Builds per-machine MSIs for both architectures with `cargo wix` and `wix/main.wxs`.
7. Extracts the matching version section from `CHANGELOG.md` for release notes.
8. Generates `checksums.txt` with SHA-256 hashes for all ZIPs and MSIs.
9. Creates the GitHub Release with the ZIPs, MSIs, checksums, and release notes.
10. Runs the dependent Winget job, which opens a public `microsoft/winget-pkgs` PR for `jcardama.LeopardWM` using the released MSIs.

Binary paths: `target/x86_64-pc-windows-msvc/release/` and `target/aarch64-pc-windows-msvc/release/`.

A successful GitHub Release does not imply the Winget job succeeded. Verify both jobs independently.

## Changelog Format

Use Conventional Commits-style sections in `CHANGELOG.md`:

```markdown
## 0.2.0

### Features
- Add workspace switching via Ctrl+Alt+1-9

### Improvements
- Improve border rendering performance on multi-monitor setups

### Fixes
- Fix transient window suppression for Beeper desktop app
```

The section header is `## X.Y.Z` without a `v` prefix; brackets are also accepted by the extraction script.

## Pre-Release Checklist

1. Update `CHANGELOG.md` with all notable user-facing changes.
2. Bump `[workspace.package].version` in `Cargo.toml` and update `Cargo.lock`.
3. Update the stable MSVC toolchain so local clippy matches current CI.
4. Run the local release gate and inspect every result:
   - `cargo build --release`
   - `pwsh ./.github/verify-gui-subsystems.ps1`
   - `pwsh ./.github/verify-executable-versions.ps1`
   - `cargo test --all`
   - `cargo clippy --all -- -D warnings`
   - `cargo fmt --all -- --check`
5. Commit the release preparation without co-author trailers.
6. Freeze the candidate commit and independently review the exact diff from the previous release.
7. Push the candidate to `main` without force and require the `check` workflow to pass for that exact SHA.
8. Confirm `origin/main` still matches the candidate and the release tag does not already exist.
9. Create the lightweight tag: `git tag vX.Y.Z <candidate-sha>`.
10. Push only the tag: `git push origin refs/tags/vX.Y.Z`.
11. Monitor the Release workflow, then verify the GitHub Release, ZIP/MSI checksums, ZIP contents, release notes, and Winget PR.

Any candidate change after verification or review requires rerunning the complete gate and review before publication.

## Scoop Extras Contract

Scoop package metadata is maintained outside this repository. The Scoop Extras manifest depends on the tag-triggered GitHub Release preserving:

- URL template: `https://github.com/jcardama/LeopardWM/releases/download/v$version/LeopardWM-$version-x86_64-windows.zip`
- Archive root: `LeopardWM-$version-x86_64-windows`
- Shim targets: `leopardwm.exe`, `leopardwm-cli.exe`, and `lwm.exe`

Coordinate any change to this release contract with the Scoop Extras manifest before publishing a release.

## Branch Protection (`main`)

- Required status check: `check` (strict; the branch must be up to date).
- Required approving reviews: 1.
- Linear history is required.
- Admin enforcement is disabled, so repository administrators can bypass these requirements without changing protection settings.
- Force pushes are not allowed.
