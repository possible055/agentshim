# Contributing

## Validation

Run the smallest relevant check while developing. For ordinary changes to the root package, use the same fast path as CI:

```console
cargo fmt --all -- --check
cargo clippy --locked -p agentshim --all-targets --all-features -- -D warnings
cargo test --locked -p agentshim --tests
cargo check --locked -p agentshim --all-features --tests
cargo test --locked -p agentshim --features bench-internals --lib -- profiled
cargo doc --locked -p agentshim --all-features --no-deps
```

Run the documentation command with `RUSTDOCFLAGS=-Dwarnings`, using the assignment syntax for the current shell.

Before handing off a complete change, run the full local validation:

```console
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked
cargo check --locked --workspace --all-features --tests
```

CI always runs the root-package fast path. It adds the derivative-package checks when either derivative source tree, the workspace manifests or lockfile, the Rust toolchain, checkout attributes, or the validation workflows change. Manual validation runs the full scope.

Probe, soak, and other `#[ignore]` integration binaries stay out of the default run. Build one explicitly with `cargo test --locked --test <name> -- --ignored`.

Changes to feature-gated PDF code must also compile the rendering-only profile:

```console
cargo check --locked -p agentshim-pdf-read --all-targets --no-default-features --features rendering
```

## Local hooks

The hooks require Gitleaks 8.30.1 and cargo-deny 0.20.2 as system tools. Install cargo-deny with the pinned Rust-compatible version and install Gitleaks from its release assets, then verify both commands are available:

```console
cargo install --locked cargo-deny --version 0.20.2
cargo deny --version
gitleaks version
```

Hook wrappers live in `scripts/hooks/` and redirect pre-commit output to stderr so editor-driven commits and pushes surface hook failures instead of hiding them in the Git output channel. Enable them per clone:

```console
git config core.hooksPath scripts/hooks
```

The wrappers replace `pre-commit install`; hooks generated into `.git/hooks` are ignored once `core.hooksPath` is set.

Pre-commit blocks formatting, native Clippy, unused dependencies, dependency policy violations, and staged secrets. The structural Clippy audit reports cognitive-complexity findings above 30 without blocking; it covers the root, core, N-API, and gigatoken crates and deliberately excludes the retained upstream PDF source. Pre-push runs Linux-target Clippy and the complete locked test suite, including documentation tests.

The dependency gate rejects vulnerabilities, unsound advisories, wildcard registry requirements, unknown sources, and unapproved licenses. Unmaintained advisories do not block this gate because the retained PDF dependency graph currently has no safe replacement for every such crate.

Stable releases run cargo-semver-checks 0.44.0 against the latest reachable stable tag for the `agentshim` and `agentshim-core` public Rust APIs. Prereleases skip that gate because their APIs may still change before the stable release.

## Test isolation

- Pure tests may run in parallel and must not depend on execution order.
- Filesystem tests must own a separate temporary directory.
- Runtime pools, caches, admission gates, profilers, and other mutable resources must be injected per test. Do not add a shared mutable singleton as a test convenience.
- Tests that change the process environment or current working directory must perform the changed-state assertion in a child process configured through `Command`.
- Timing, throughput, thread scheduling, and repeated race checks belong in the performance or stability workflows. Correctness tests must assert deterministic contracts.
- A test that passes only with `--test-threads 1` is not isolated and must not be merged in that state.

## What to test

Unit tests cover one module's behaviour. Integration tests use only the public API and real stdio. The same contract belongs in one layer, not both.

Assert public parse boundaries, MCP wire (lifecycle, versions, annotations, error envelopes, stdio integrity), path and process safety, resource admission, and observable tool output. Do not assert that a named constant equals a number, that `memory_charge()` equals `BASE + "fixed".len()`, or that a full tool schema JSON matches a snapshot. Do not wrap a fixed parameter in a helper just to check it. Do not freeze README examples, doctor default tables, or internal thresholds that should stay flexible.

## Platform boundaries

Windows and Unix mechanisms live under `crate::platform`. Portable policy and result formatting remain in their owning modules. Add shared contract tests for platform implementations, then exercise each implementation on its native CI runner.

The fully supported platform runs the root package's complete native validation suite on every pull request and every push to `main`; derivative packages receive their complete suite when their validation inputs change:

- Windows x86-64

Compatibility-supported platforms receive native release assets. Every release verifies the binary version, archive manifest, checksum, and two consecutive installer runs on the native runner, but these platforms do not run the full pull-request suite:

- Linux x86-64
- Linux ARM64
- macOS ARM64

The optional platform workflow can run the current Windows-aligned validation suite on those three platforms, as well as Windows ARM64 and macOS x86-64. Windows ARM64, macOS x86-64, and all other Rust targets are possible-support source builds with no release assets, CI guarantee, service level, or support commitment.
