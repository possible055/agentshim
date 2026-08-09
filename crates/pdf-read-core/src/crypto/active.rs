use super::rust_provider::RustCryptoProvider;

static ACTIVE: RustCryptoProvider = RustCryptoProvider::new();

pub(crate) fn active() -> &'static RustCryptoProvider {
    &ACTIVE
}
