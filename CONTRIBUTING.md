# Contributing to LeopardWM

Thank you for your interest in contributing to LeopardWM!

## Code of Conduct

Be respectful and constructive in all interactions.

## How to Contribute

### Reporting Issues

- Check existing issues before creating a new one
- Use clear, descriptive titles
- Include steps to reproduce bugs
- Include system information (Windows version, monitor setup)

### Pull Requests

1. Fork the repository
2. Create a feature branch (`git checkout -b feat/amazing-feature`)
3. Make your changes
4. Run tests (`cargo test --all`)
5. Run formatting (`cargo fmt --all`)
6. Run linting (`cargo clippy --all -- -D warnings`)
7. Commit with conventional commits (`feat:`, `fix:`, `docs:`, `chore:`)
8. Push and open a PR

PRs must pass CI and receive at least one approving review before merging.
Changes to `.github/` or `SECURITY.md` require owner review.

### Commit Messages

We use [Conventional Commits](https://www.conventionalcommits.org/):

- `feat:` - New features
- `fix:` - Bug fixes
- `docs:` - Documentation changes
- `chore:` - Maintenance tasks
- `refactor:` - Code refactoring
- `test:` - Test additions/changes

### Code Style

- Follow Rust idioms and best practices
- Use `cargo fmt` for formatting
- Address all `cargo clippy` warnings
- Add tests for new functionality
- Document public APIs

## Development Setup

Building requires Rust with the MSVC toolchain (`stable-x86_64-pc-windows-msvc`).
The final `pwsh -NoProfile -File tools/check.ps1` check requires PowerShell 7
and Python for the tools tests. Node.js is optional, for the extra Settings
JavaScript check described below.

```bash
# Install Rust (if not already installed)
# https://rustup.rs/

# Clone and build
git clone https://github.com/jcardama/LeopardWM.git
cd LeopardWM
cargo build

# Run tests
cargo test --all

# Check formatting
cargo fmt --all -- --check

# Run linter
cargo clippy --all -- -D warnings
```

## Translating LeopardWM

Edit an existing UTF-8 catalog in `crates/daemon/locales/`, or copy `en.toml` to
`<identifier>.toml` to add a language. Adding that one file is enough: the build
embeds it, config accepts its filename stem, and Settings lists its `language.name`
autonym. Use a language identifier such as `fr` or `pt-BR`: 2–8 ASCII letters,
optionally followed by hyphen-separated groups of 1–8 ASCII letters or digits.
Identifiers are case-sensitive; `en.toml` remains the English source of truth.

Translate values only, including `language.name` into the language's own name.
Preserve every quoted key and `{placeholder}`, and keep values plain text rather
than HTML. From the repository root, run:

```powershell
cargo test -p leopardwm-daemon locale::tests
```

Tests discover every catalog and check flat string TOML, identifiers, a nonempty
autonym, complete English keys, no orphan keys, and matching placeholder names and
counts. A translation PR should include the catalog, language identifier, test
result, and whether a native speaker reviewed it. Do not claim human review for
machine drafts; the bundled `zh-CN` translation is human-reviewed.
Rebuild to try translations. See [Localization](docs/localization.md) for details
and the limits of automated checks.

With Node.js on `PATH`, this optional extra check exercises Settings rendering,
escaping, and live language changes without a browser:

```powershell
cargo test -p leopardwm-daemon translations_render_literally_in_settings_and_switch_live -- --ignored
```

Run only that named ignored test; other ignored tests can drive the desktop.

## Architecture Notes

- **core_layout**: Pure layout logic, no platform dependencies. Should be easily testable.
- **platform_win32**: All Windows API calls go here. Uses `windows-rs` crate.
- **daemon**: Orchestrates everything. Handles events, manages state, applies layouts.
- **cli**: Thin client that sends commands to the daemon via IPC.

## License

By contributing, you agree that your contributions will be licensed under GPL-3.0.
