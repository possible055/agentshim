# Contributing

## Validation

Run the smallest relevant check while developing. Before handing off a complete change, run:

```console
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked
```

Changes to feature-gated PDF code must also compile the rendering-only profile:

```console
cargo check --locked -p codexshim-pdf-read --all-targets --no-default-features --features rendering
```

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

The fully supported platform runs the complete native validation suite on every pull request and every push to `main`:

- Windows x86-64

Compatibility-supported platforms receive native release assets. Every release verifies the binary version, archive manifest, checksum, and two consecutive installer runs on the native runner, but these platforms do not run the full pull-request suite:

- Linux x86-64
- Linux ARM64
- macOS ARM64

Windows ARM64 and macOS x86-64 may be tested manually through the optional platform workflow. They and all other Rust targets are possible-support source builds with no release assets, CI guarantee, service level, or support commitment.
