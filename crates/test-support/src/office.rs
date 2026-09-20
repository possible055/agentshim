use std::path::PathBuf;

/// Locates a file in the shared office fixture corpus
/// (`crates/office-read-core/tests/fixtures`). Single source for the three
/// former per-module `fixture()` copies.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../office-read-core/tests/fixtures")
        .join(name)
}
