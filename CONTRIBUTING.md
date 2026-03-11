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

## Architecture Notes

- **core_layout**: Pure layout logic, no platform dependencies. Should be easily testable.
- **platform_win32**: All Windows API calls go here. Uses `windows-rs` crate.
- **daemon**: Orchestrates everything. Handles events, manages state, applies layouts.
- **cli**: Thin client that sends commands to the daemon via IPC.

## License

By contributing, you agree that your contributions will be licensed under GPL-3.0.
