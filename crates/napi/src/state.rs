//! Engine-shared state plus the promise/work plumbing every N-API surface builds on.

use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use napi::{Env, Result, Unknown, bindgen_prelude::ToNapiValue};
use tokio_util::sync::CancellationToken;

use crate::background::BackgroundJob;
use crate::budget::NativeOutputLimits;
use crate::capture::ArtifactRecord;
use crate::process::{NativeFailure, NativeResult, PreparedHandles};

pub(crate) struct EngineState {
    pub(crate) root: Arc<agentshim_core::path::RepositoryRoot>,
    pub(crate) access: Arc<agentshim_core::path::FileAccess>,
    pub(crate) tool_engine: agentshim_core::ToolEngine,
    pub(crate) resources: agentshim_core::runtime::RuntimeResources,
    pub(crate) output_limits: NativeOutputLimits,
    pub(crate) timeout_ceiling_ms: u64,
    pub(crate) background_timeout_max_ms: u64,
    pub(crate) shutdown: CancellationToken,
    pub(crate) capture_root: std::path::PathBuf,
    pub(crate) capture_max_bytes: u64,
    pub(crate) capture_cleanup_session_end: bool,
    pub(crate) session_key: String,
    pub(crate) artifacts: Arc<std::sync::Mutex<Vec<ArtifactRecord>>>,
    pub(crate) prepared: PreparedHandles,
    pub(crate) active_calls: Arc<AtomicUsize>,
    pub(crate) native_work: Arc<AtomicUsize>,
    pub(crate) calls: std::sync::Mutex<HashMap<String, CancellationToken>>,
    pub(crate) backgrounds: std::sync::Mutex<Vec<std::sync::Weak<BackgroundJob>>>,
}

impl EngineState {
    pub(crate) fn native_work_count(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.native_work)
    }

    pub(crate) fn start_native_work(&self) -> NativeWorkGuard {
        self.native_work.fetch_add(1, Ordering::SeqCst);
        NativeWorkGuard(Arc::clone(&self.native_work))
    }

    pub(crate) fn begin_call(&self, call_id: &str) -> NativeResult<()> {
        if call_id.is_empty() {
            return NativeResult::failure(NativeFailure::invalid(
                "native call id must not be empty",
            ));
        }
        if self.shutdown.is_cancelled() {
            return NativeResult::failure(NativeFailure::engine_closed());
        }
        let mut calls = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if calls.contains_key(call_id) {
            return NativeResult::failure(NativeFailure::new(
                "AGENTSHIM_CALL_ALREADY_ACTIVE",
                "native call id is already active",
                false,
                Some(serde_json::json!({ "callId": call_id })),
            ));
        }
        calls.insert(call_id.to_owned(), self.shutdown.child_token());
        self.active_calls.fetch_add(1, Ordering::SeqCst);
        NativeResult::success(())
    }

    pub(crate) fn call_token(
        &self,
        call_id: &str,
    ) -> std::result::Result<CancellationToken, NativeFailure> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(call_id)
            .cloned()
            .ok_or_else(|| {
                NativeFailure::new(
                    "AGENTSHIM_CALL_INVALID",
                    "native call id is not active",
                    false,
                    Some(serde_json::json!({ "callId": call_id })),
                )
            })
    }

    pub(crate) fn cancel_call(&self, call_id: &str) -> NativeResult<bool> {
        let token = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(call_id)
            .cloned();
        if let Some(token) = token {
            token.cancel();
            NativeResult::success(true)
        } else {
            NativeResult::success(false)
        }
    }

    pub(crate) fn release_call(&self, call_id: &str) -> NativeResult<bool> {
        let removed = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(call_id)
            .is_some();
        if removed {
            self.active_calls.fetch_sub(1, Ordering::SeqCst);
        }
        NativeResult::success(removed)
    }

    pub(crate) fn cancel_backgrounds(&self) {
        for job in self.background_snapshot() {
            job.cancel_from_engine();
        }
    }

    pub(crate) fn background_snapshot(&self) -> Vec<Arc<BackgroundJob>> {
        self.backgrounds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect()
    }
}

pub(crate) struct NativeWorkGuard(Arc<AtomicUsize>);

impl Drop for NativeWorkGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) fn detached_native_work() -> NativeWorkGuard {
    NativeWorkGuard(Arc::new(AtomicUsize::new(1)))
}

pub(crate) fn native_promise<T, F>(
    env: Env,
    work: NativeWorkGuard,
    future: F,
) -> Result<Unknown<'static>>
where
    T: ToNapiValue + Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    // Keep the pending future independent of the JavaScript class borrow so a
    // Worker teardown can cancel and drain it before releasing the environment.
    let raw_env = env.raw();
    let promise = napi::bindgen_prelude::execute_tokio_future_with_finalize_callback(
        raw_env,
        future,
        |env, value| {
            // Safety: the finalize callback runs on the environment thread with
            // a live `env`, per the `execute_tokio_future` contract.
            unsafe { T::to_napi_value(env, value) }
        },
        Some(Box::new(move |_| drop(work))),
    )?;
    // Safety: `promise` was produced by this same `raw_env` on this thread.
    Ok(unsafe { Unknown::from_raw_unchecked(raw_env, promise) })
}
