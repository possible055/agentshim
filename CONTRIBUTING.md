# Contributing

Issues and pull requests are welcome. For substantial changes, open an issue to discuss the approach first.

## Development

Install the Rust toolchain specified in `rust-toolchain.toml`. Keep changes focused, follow the existing code style, and add or update tests when needed.

Install `cargo-nextest` and run the relevant checks before submitting a pull request:

```console
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo nextest run --locked --retries 2
```

Tests must be isolated and deterministic. A test that passes only after a retry needs to be fixed. CI runs additional checks for affected components.

## Local hooks

To enable the repository's Git hooks, install `pre-commit` (or `uv`), Gitleaks, and cargo-deny, then run:

```console
git config core.hooksPath scripts/hooks
```

Every commit runs Rust formatting, workspace Clippy, and the cognitive-complexity audit.
Every push runs workspace tests with nextest (failing on flaky retries), DSH unit tests and
builds, and Linux-target Clippy on Linux. These checks run regardless of changed paths so
deletions and shared fixtures cannot be skipped by path filters.

## Pull requests

Describe the problem, the change, and how you verified it. Link any related issues and note platform-specific behavior or limitations.
