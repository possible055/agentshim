use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

use agentshim_gigatoken::{CountUpTo, LoadError, O200kCounter, O200kPrototype};
use rmcp::model::CallToolResult;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub(crate) const MODEL_OUTPUT_TOKEN_LIMIT: usize = 10_000;
pub(crate) const CLIENT_WRAPPER_TOKEN_RESERVE: usize = 128;
pub(crate) const IMAGE_MODEL_TOKENS: usize = 1_844;
pub(crate) const IMAGE_ITEM_TOKEN_RESERVE: usize = 32;
const BYTE_FAST_PATH_LIMIT: usize = 512;
pub(crate) const TOOL_CONTENT_TOKEN_LIMIT: usize =
    MODEL_OUTPUT_TOKEN_LIMIT - CLIENT_WRAPPER_TOKEN_RESERVE;
const COUNTER_WORKERS: usize = 2;
const POOL_CANCELLATION_POLL: Duration = Duration::from_millis(5);
// The shared gate contains only the immutable tokenizer prototype and its reusable
// counters. Timeout, byte limit, read scope, client profile, and catalog state stay
// on each AgentShim instance.
static SHARED_GATE: OnceLock<Arc<OutputTokenGate>> = OnceLock::new();
static SHARED_GATE_INIT: Mutex<()> = Mutex::new(());

pub(crate) use agentshim_core::output::{ProjectedTokenCost, ProjectionDecision};

pub(crate) struct OutputTokenGate {
    prototype: O200kPrototype,
    counters: Mutex<Vec<O200kCounter>>,
    available: Condvar,
}

impl OutputTokenGate {
    pub(crate) fn load() -> Result<Self, LoadError> {
        let prototype = O200kPrototype::load_embedded()?;
        let counters = (0..COUNTER_WORKERS)
            .map(|_| prototype.fork_counter())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            prototype,
            counters: Mutex::new(counters),
            available: Condvar::new(),
        })
    }

    pub(crate) fn load_shared() -> Result<Arc<Self>, LoadError> {
        if let Some(gate) = SHARED_GATE.get() {
            return Ok(Arc::clone(gate));
        }
        let _initialization = SHARED_GATE_INIT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(gate) = SHARED_GATE.get() {
            return Ok(Arc::clone(gate));
        }
        let gate = Arc::new(Self::load()?);
        assert!(
            SHARED_GATE.set(Arc::clone(&gate)).is_ok(),
            "shared gate initialization is serialized"
        );
        Ok(gate)
    }

    pub(crate) fn project_tool_output(
        &self,
        text: &str,
        image_count: usize,
        ceiling: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        let mut content = Vec::with_capacity(image_count.saturating_add(1));
        content.push(serde_json::json!({ "type": "text", "text": text }));
        content.extend(
            (0..image_count).map(
                |_| serde_json::json!({ "type": "image", "data": "", "mimeType": "image/png" }),
            ),
        );
        let payload = serde_json::to_string(&content).expect("tool content projection serializes");
        self.project_payload(
            &payload,
            CLIENT_WRAPPER_TOKEN_RESERVE.saturating_add(
                image_count.saturating_mul(IMAGE_MODEL_TOKENS + IMAGE_ITEM_TOKEN_RESERVE),
            ),
            ceiling,
            cancellation,
        )
    }

    pub(crate) fn project_result(
        &self,
        result: &CallToolResult,
        ceiling: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        let (payload, image_count) = if let Some(structured) = &result.structured_content
            && !structured.is_null()
        {
            (
                serde_json::to_string(structured).expect("MCP structured content is serializable"),
                0,
            )
        } else {
            let mut content = serde_json::to_value(&result.content)
                .expect("MCP content projection is serializable");
            let image_count = strip_image_payloads(&mut content);
            (
                serde_json::to_string(&content).expect("MCP content projection is serializable"),
                image_count,
            )
        };
        self.project_payload(
            &payload,
            CLIENT_WRAPPER_TOKEN_RESERVE.saturating_add(
                image_count.saturating_mul(IMAGE_MODEL_TOKENS + IMAGE_ITEM_TOKEN_RESERVE),
            ),
            ceiling,
            cancellation,
        )
    }

    pub(crate) fn project_payload(
        &self,
        payload: &str,
        fixed_tokens: usize,
        ceiling: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        let Some(payload_ceiling) = ceiling.checked_sub(fixed_tokens) else {
            return if cancellation.is_cancelled() {
                ProjectionDecision::Cancelled
            } else {
                ProjectionDecision::Exceeded
            };
        };
        match self.evaluate_up_to(payload, payload_ceiling, cancellation) {
            ProjectionDecision::Fits(cost) => ProjectionDecision::Fits(ProjectedTokenCost {
                tokens: fixed_tokens.saturating_add(cost.tokens),
                exact: cost.exact,
            }),
            ProjectionDecision::Exceeded => ProjectionDecision::Exceeded,
            ProjectionDecision::Cancelled => ProjectionDecision::Cancelled,
        }
    }

    fn evaluate_up_to(
        &self,
        payload: &str,
        limit: usize,
        cancellation: &CancellationToken,
    ) -> ProjectionDecision {
        if cancellation.is_cancelled() {
            return ProjectionDecision::Cancelled;
        }
        if payload.len() <= limit && payload.len() <= BYTE_FAST_PATH_LIMIT {
            tracing::trace!(target: "agentshim", token_gate_path = "byte_fast", tokens_upper_bound = payload.len());
            return ProjectionDecision::Fits(ProjectedTokenCost {
                tokens: payload.len(),
                exact: false,
            });
        }
        let trace_enabled = tracing::enabled!(target: "agentshim", tracing::Level::TRACE);
        let wait_started = trace_enabled.then(Instant::now);
        let Some(mut counter) = self.acquire(cancellation) else {
            return ProjectionDecision::Cancelled;
        };
        let wait_ns = wait_started.map(|started| duration_ns(started.elapsed()));
        let before = trace_enabled.then(|| counter.counter_mut().metrics());
        let count_started = trace_enabled.then(Instant::now);
        let result = counter
            .counter_mut()
            .count_ordinary_up_to(payload, limit, || cancellation.is_cancelled());
        let count_ns = count_started.map(|started| duration_ns(started.elapsed()));
        let after = trace_enabled.then(|| counter.counter_mut().metrics());
        counter.mark_healthy();
        if let (Some(wait_ns), Some(count_ns), Some(before), Some(after)) =
            (wait_ns, count_ns, before, after)
        {
            tracing::trace!(
                target: "agentshim",
                token_gate_path = "exact",
                token_counter_pool_wait_ns = wait_ns,
                token_count_ns = count_ns,
                token_cache_hits = after
                    .short_hits
                    .saturating_add(after.long_hits)
                    .saturating_sub(before.short_hits.saturating_add(before.long_hits)),
                token_cache_misses = after.misses.saturating_sub(before.misses),
                token_cache_uncached = after.uncached.saturating_sub(before.uncached),
                token_cache_resets = after.resets.saturating_sub(before.resets),
                token_cache_resident_bytes = after.resident_bytes,
            );
        }
        match result {
            CountUpTo::Exact(count) => ProjectionDecision::Fits(ProjectedTokenCost {
                tokens: count,
                exact: true,
            }),
            CountUpTo::Exceeded => ProjectionDecision::Exceeded,
            CountUpTo::Cancelled => ProjectionDecision::Cancelled,
        }
    }

    fn acquire(&self, cancellation: &CancellationToken) -> Option<CounterLease<'_>> {
        let mut counters = self.lock_counters();
        loop {
            if let Some(counter) = counters.pop() {
                return Some(CounterLease {
                    gate: self,
                    counter: Some(counter),
                    healthy: false,
                });
            }
            if cancellation.is_cancelled() {
                return None;
            }
            let waited = self
                .available
                .wait_timeout(counters, POOL_CANCELLATION_POLL)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            counters = waited.0;
        }
    }

    fn release(&self, counter: O200kCounter) {
        self.lock_counters().push(counter);
        self.available.notify_one();
    }

    fn lock_counters(&self) -> MutexGuard<'_, Vec<O200kCounter>> {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn strip_image_payloads(value: &mut Value) -> usize {
    let Value::Array(content) = value else {
        return 0;
    };
    let mut images = 0_usize;
    for block in content {
        let Value::Object(block) = block else {
            continue;
        };
        if block.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }
        images = images.saturating_add(1);
        if let Some(data) = block.get_mut("data") {
            *data = Value::String(String::new());
        }
    }
    images
}

struct CounterLease<'a> {
    gate: &'a OutputTokenGate,
    counter: Option<O200kCounter>,
    healthy: bool,
}

impl CounterLease<'_> {
    fn counter_mut(&mut self) -> &mut O200kCounter {
        self.counter.as_mut().expect("counter lease is occupied")
    }

    fn mark_healthy(&mut self) {
        self.healthy = true;
    }
}

impl Drop for CounterLease<'_> {
    fn drop(&mut self) {
        let counter = if self.healthy {
            self.counter.take()
        } else {
            self.gate.prototype.fork_counter().ok()
        };
        if let Some(counter) = counter {
            self.gate.release(counter);
        }
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

pub(crate) fn structured_result_fits_model_budget(
    structured: &Value,
    cancellation: &CancellationToken,
) -> bool {
    let Ok(gate) = OutputTokenGate::load_shared() else {
        return false;
    };
    matches!(
        gate.project_payload(
            &serde_json::to_string(structured).expect("structured content is serializable"),
            0,
            TOOL_CONTENT_TOKEN_LIMIT,
            cancellation,
        ),
        ProjectionDecision::Fits(_)
    )
}

#[cfg(test)]
mod tests {
    use rmcp::model::CallToolResult;
    use tokio_util::sync::CancellationToken;

    use rmcp::model::ContentBlock;

    use super::{
        CLIENT_WRAPPER_TOKEN_RESERVE, COUNTER_WORKERS, IMAGE_ITEM_TOKEN_RESERVE,
        IMAGE_MODEL_TOKENS, OutputTokenGate, POOL_CANCELLATION_POLL, ProjectionDecision,
        TOOL_CONTENT_TOKEN_LIMIT,
    };

    fn projected_text_payload(text: &str) -> String {
        serde_json::to_string(&[serde_json::json!({ "type": "text", "text": text })])
            .expect("MCP text content is serializable")
    }

    #[test]
    fn fast_path_and_exact_boundary_are_distinct() {
        let gate = OutputTokenGate::load().expect("embedded ranks");
        let cancellation = CancellationToken::new();
        let gate_limit = TOOL_CONTENT_TOKEN_LIMIT + CLIENT_WRAPPER_TOKEN_RESERVE;
        assert_eq!(
            gate.project_payload(
                &"a".repeat(512),
                CLIENT_WRAPPER_TOKEN_RESERVE,
                gate_limit,
                &cancellation
            ),
            ProjectionDecision::Fits(agentshim_core::output::ProjectedTokenCost {
                tokens: CLIENT_WRAPPER_TOKEN_RESERVE + 512,
                exact: false
            })
        );
        let ProjectionDecision::Fits(exact) = gate.project_payload(
            &"a".repeat(TOOL_CONTENT_TOKEN_LIMIT),
            CLIENT_WRAPPER_TOKEN_RESERVE,
            gate_limit,
            &cancellation,
        ) else {
            panic!("exact boundary must fit");
        };
        assert!(exact.exact);
        assert_eq!(
            gate.project_payload(
                &" x".repeat(TOOL_CONTENT_TOKEN_LIMIT),
                CLIENT_WRAPPER_TOKEN_RESERVE,
                gate_limit,
                &cancellation,
            ),
            ProjectionDecision::Fits(agentshim_core::output::ProjectedTokenCost {
                tokens: gate_limit,
                exact: true
            })
        );
        assert_eq!(
            gate.project_payload(
                &" x".repeat(TOOL_CONTENT_TOKEN_LIMIT + 1),
                CLIENT_WRAPPER_TOKEN_RESERVE,
                gate_limit,
                &cancellation,
            ),
            ProjectionDecision::Exceeded
        );
    }

    #[test]
    fn image_projection_counts_caption_text_but_not_base64() {
        let gate = OutputTokenGate::load().expect("embedded ranks");
        let result = CallToolResult::success(vec![
            ContentBlock::text("page 1 caption"),
            ContentBlock::image("A".repeat(200_000), "image/png"),
        ]);
        assert!(matches!(
            gate.project_result(
                &result,
                TOOL_CONTENT_TOKEN_LIMIT + CLIENT_WRAPPER_TOKEN_RESERVE,
                &CancellationToken::new()
            ),
            ProjectionDecision::Fits(_)
        ));
    }

    #[test]
    fn image_only_projection_observes_cancellation() {
        let gate = OutputTokenGate::load().expect("embedded ranks");
        let result =
            CallToolResult::success(vec![ContentBlock::image("A".repeat(200_000), "image/png")]);
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert_eq!(
            gate.project_result(
                &result,
                TOOL_CONTENT_TOKEN_LIMIT + CLIENT_WRAPPER_TOKEN_RESERVE,
                &cancellation
            ),
            ProjectionDecision::Cancelled
        );
    }

    #[test]
    fn result_projection_matches_codex_structured_content_precedence() {
        let gate = OutputTokenGate::load().expect("embedded ranks");
        let cancellation = CancellationToken::new();
        let mut structured = CallToolResult::success(vec![ContentBlock::text(" x".repeat(8_000))]);
        structured.structured_content = Some(serde_json::json!({ "ok": true }));
        let ProjectionDecision::Fits(structured_cost) =
            gate.project_result(&structured, 1_000, &cancellation)
        else {
            panic!("small structured content must take precedence");
        };
        assert!(structured_cost.tokens >= CLIENT_WRAPPER_TOKEN_RESERVE);

        structured.structured_content = Some(serde_json::Value::Null);
        assert_eq!(
            gate.project_result(&structured, 1_000, &cancellation),
            ProjectionDecision::Exceeded
        );
    }

    #[test]
    fn image_projection_ignores_base64_and_charges_fixed_model_cost() {
        let gate = OutputTokenGate::load().expect("embedded ranks");
        let cancellation = CancellationToken::new();
        let small = CallToolResult::success(vec![
            ContentBlock::text("caption"),
            ContentBlock::image("A".repeat(16), "image/png"),
        ]);
        let large = CallToolResult::success(vec![
            ContentBlock::text("caption"),
            ContentBlock::image("A".repeat(1_000_000), "image/png"),
        ]);
        let ProjectionDecision::Fits(small) =
            gate.project_result(&small, usize::MAX, &cancellation)
        else {
            panic!("small image projection");
        };
        let ProjectionDecision::Fits(large) =
            gate.project_result(&large, usize::MAX, &cancellation)
        else {
            panic!("large image projection");
        };
        assert_eq!(small, large);
        assert!(
            small.tokens
                >= CLIENT_WRAPPER_TOKEN_RESERVE + IMAGE_MODEL_TOKENS + IMAGE_ITEM_TOKEN_RESERVE
        );
    }

    #[test]
    fn client_json_wrapper_crosses_the_exact_content_boundary() {
        let gate = OutputTokenGate::load().expect("embedded ranks");
        let cancellation = CancellationToken::new();
        // One projected text block counted against the content limit pins the
        // wrapper-reserve boundary the retired evaluate_tool_text API tested.
        let fits = projected_text_payload(&" x".repeat(TOOL_CONTENT_TOKEN_LIMIT - 10));
        assert_eq!(
            gate.project_payload(&fits, 0, TOOL_CONTENT_TOKEN_LIMIT, &cancellation),
            ProjectionDecision::Fits(agentshim_core::output::ProjectedTokenCost {
                tokens: TOOL_CONTENT_TOKEN_LIMIT,
                exact: true
            })
        );
        let over = projected_text_payload(&" x".repeat(TOOL_CONTENT_TOKEN_LIMIT - 9));
        assert_eq!(
            gate.project_payload(&over, 0, TOOL_CONTENT_TOKEN_LIMIT, &cancellation),
            ProjectionDecision::Exceeded
        );
    }

    #[test]
    fn saturated_counter_pool_keeps_the_fast_path_available_and_honors_cancellation() {
        let gate = std::sync::Arc::new(OutputTokenGate::load().expect("embedded ranks"));
        let counters = (0..COUNTER_WORKERS)
            .map(|_| gate.acquire(&CancellationToken::new()).expect("counter"))
            .collect::<Vec<_>>();
        assert!(
            matches!(
                gate.project_payload(
                    "small output",
                    0,
                    TOOL_CONTENT_TOKEN_LIMIT,
                    &CancellationToken::new()
                ),
                ProjectionDecision::Fits(_)
            ),
            "small output must not wait for a counter slot"
        );
        let cancellation = CancellationToken::new();
        let worker_gate = std::sync::Arc::clone(&gate);
        let worker_cancellation = cancellation.clone();
        let waiter = std::thread::spawn(move || {
            worker_gate.project_payload(
                &" x".repeat(10_000),
                0,
                TOOL_CONTENT_TOKEN_LIMIT,
                &worker_cancellation,
            )
        });

        std::thread::sleep(POOL_CANCELLATION_POLL * 2);
        cancellation.cancel();
        assert_eq!(
            waiter.join().expect("waiter"),
            ProjectionDecision::Cancelled
        );
        drop(counters);
    }

    #[test]
    fn panicking_counter_lease_rebuilds_its_slot() {
        let gate = OutputTokenGate::load().expect("embedded ranks");
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _counter = gate
                .acquire(&CancellationToken::new())
                .expect("counter lease");
            panic!("counter failure probe");
        }));

        assert!(panic.is_err());
        assert_eq!(gate.lock_counters().len(), 2);
    }

    #[test]
    #[ignore = "release contention gate"]
    fn two_counter_pool_handles_concurrent_exact_fits() {
        assert!(!cfg!(debug_assertions), "run this gate with --release");
        let gate = std::sync::Arc::new(OutputTokenGate::load().expect("embedded ranks"));
        for concurrency in [1, 4, 8, 16] {
            let started = std::time::Instant::now();
            let workers = (0..concurrency)
                .map(|_| {
                    let gate = std::sync::Arc::clone(&gate);
                    std::thread::spawn(move || {
                        gate.project_payload(
                            &" x".repeat(9_800),
                            0,
                            TOOL_CONTENT_TOKEN_LIMIT,
                            &tokio_util::sync::CancellationToken::new(),
                        )
                    })
                })
                .collect::<Vec<_>>();
            for worker in workers {
                assert!(matches!(
                    worker.join().expect("worker"),
                    ProjectionDecision::Fits(_)
                ));
            }
            eprintln!(
                "concurrency={concurrency} exact_fits_wall_ms={:.3}",
                started.elapsed().as_secs_f64() * 1_000.0
            );
        }
    }

    #[test]
    #[ignore = "release small-output latency gate"]
    fn complete_ticket_fast_path_stays_within_small_output_p95_target() {
        assert!(!cfg!(debug_assertions), "run this gate with --release");
        let cancellation = CancellationToken::new();
        let token_gate = std::sync::Arc::new(OutputTokenGate::load().expect("embedded ranks"));
        let burst_gate = crate::output::BurstOutputGate::new(8_192);
        for bytes in [128, 256, 384] {
            let output = "a".repeat(bytes);
            let mut baseline = Vec::new();
            let mut gated = Vec::new();
            for sample in 0..9 {
                let measure_baseline = || {
                    let started = std::time::Instant::now();
                    for _ in 0..2_000 {
                        std::hint::black_box(
                            std::fs::metadata("Cargo.toml")
                                .expect("benchmark fixture")
                                .len(),
                        );
                        std::hint::black_box(token_gate.project_tool_output(
                            &output,
                            0,
                            TOOL_CONTENT_TOKEN_LIMIT,
                            &cancellation,
                        ));
                    }
                    started.elapsed()
                };
                let measure_gated = || {
                    let started = std::time::Instant::now();
                    for _ in 0..2_000 {
                        std::hint::black_box(
                            std::fs::metadata("Cargo.toml")
                                .expect("benchmark fixture")
                                .len(),
                        );
                        let budget = crate::output::CallOutputBudget::new(
                            crate::output::MODEL_BYTE_LIMIT,
                            std::sync::Arc::clone(&token_gate),
                            burst_gate.begin_call(),
                        );
                        let decision = budget.project_tool_output(&output, 0, &cancellation);
                        if let ProjectionDecision::Fits(cost) = decision {
                            budget.cache_response_cost(cost);
                            budget.finish(0, false);
                        }
                        std::hint::black_box(decision);
                    }
                    started.elapsed()
                };
                if sample % 2 == 0 {
                    baseline.push(measure_baseline());
                    gated.push(measure_gated());
                } else {
                    gated.push(measure_gated());
                    baseline.push(measure_baseline());
                }
            }
            baseline.sort_unstable();
            gated.sort_unstable();
            let baseline_p95 = baseline[baseline.len() - 1];
            let gated_p95 = gated[gated.len() - 1];
            let ratio = gated_p95.as_secs_f64() / baseline_p95.as_secs_f64();
            eprintln!(
                "ticket_fast_path_bytes={bytes} baseline_p95_ms={:.3} gated_p95_ms={:.3} ratio={ratio:.4}",
                baseline_p95.as_secs_f64() * 1_000.0,
                gated_p95.as_secs_f64() * 1_000.0,
            );
            assert!(ratio <= 1.05, "small-output p95 regression target");
        }
    }
}
