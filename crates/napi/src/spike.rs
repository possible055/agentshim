use napi::{Error, Result, bindgen_prelude::spawn_blocking};
use napi_derive::napi;

/// Module API version; hosts must exact-match before using any Engine capability.
/// 3: the Engine takes the complete native host configuration, read returns
/// image blocks and artifact byte pages, and background jobs use `JobHooks`
/// directly without a TSFN preview callback.
pub const API_VERSION: u32 = 3;

#[napi]
pub fn api_version() -> u32 {
    API_VERSION
}

/// Spike gate: a Rust panic crossing the exported boundary must surface as a JS
/// error while the host process survives. This is not crash isolation — aborts
/// and segfaults still terminate the host.
#[napi(catch_unwind)]
pub fn spike_panic() -> Result<()> {
    panic!("spike: deliberate engine invariant breach");
}

/// Spike gate: a panic on a background native thread must let the call settle
/// with a typed error instead of hanging or corrupting the host.
#[napi(ts_return_type = "Promise<void>")]
pub async fn spike_background_panic() -> Result<()> {
    spawn_blocking(|| {
        panic!("spike: background worker invariant breach");
    })
    .await
    .map_err(|_| Error::new(napi::Status::GenericFailure, "background worker panicked"))?;
    Ok(())
}
