use std::{
    env,
    ffi::OsStr,
    io,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

pub(crate) const BURST_TOKENS_ENV: &str = "CODEXSHIM_BURST_TOKENS";
pub(crate) const DEFAULT_BURST_TOKENS: usize = 8_192;
pub(crate) const MIN_BURST_TOKENS: usize = 2_048;
pub(crate) const MAX_BURST_TOKENS: usize = DEFAULT_BURST_TOKENS;
pub(crate) const BURST_QUIET_PERIOD: Duration = Duration::from_secs(2);
pub(crate) const MAX_CONTROL_RESPONSE_TOKENS: usize = 192;

pub(crate) fn configured_burst_tokens() -> io::Result<usize> {
    parse_burst_tokens(env::var_os(BURST_TOKENS_ENV).as_deref())
}

fn parse_burst_tokens(value: Option<&OsStr>) -> io::Result<usize> {
    value.map_or(Ok(DEFAULT_BURST_TOKENS), |value| {
        value
            .to_str()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (MIN_BURST_TOKENS..=MAX_BURST_TOKENS).contains(value))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{BURST_TOKENS_ENV} must be an integer from {MIN_BURST_TOKENS} to \
                         {MAX_BURST_TOKENS}"
                    ),
                )
            })
    })
}

#[derive(Debug)]
pub(crate) struct BurstOutputGate {
    limit: usize,
    quiet_period: Duration,
    state: Mutex<BurstState>,
}

#[derive(Debug)]
struct BurstState {
    burst_id: u64,
    active_calls: usize,
    unclaimed_calls: usize,
    spent_tokens: usize,
    reserved_tokens: usize,
    last_completed: Option<Instant>,
}

impl BurstOutputGate {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Self::with_quiet_period(limit, BURST_QUIET_PERIOD)
    }

    fn with_quiet_period(limit: usize, quiet_period: Duration) -> Arc<Self> {
        Arc::new(Self {
            limit,
            quiet_period,
            state: Mutex::new(BurstState {
                burst_id: 0,
                active_calls: 0,
                unclaimed_calls: 0,
                spent_tokens: 0,
                reserved_tokens: 0,
                last_completed: None,
            }),
        })
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    pub(crate) fn begin_call(self: &Arc<Self>) -> BurstTicket {
        let now = Instant::now();
        let mut state = self.lock_state();
        let reset = state.active_calls == 0
            && state
                .last_completed
                .is_some_and(|completed| now.duration_since(completed) >= self.quiet_period);
        if state.burst_id == 0 || reset {
            state.burst_id = state.burst_id.wrapping_add(1).max(1);
            state.spent_tokens = 0;
            state.reserved_tokens = 0;
            state.last_completed = None;
        }
        state.active_calls = state.active_calls.saturating_add(1);
        state.unclaimed_calls = state.unclaimed_calls.saturating_add(1);
        let burst_id = state.burst_id;
        let active_calls = state.active_calls;
        let unclaimed_calls = state.unclaimed_calls;
        drop(state);
        tracing::trace!(
            target: "codexshim",
            event = "burst_begin",
            burst_id,
            active_calls,
            unclaimed_calls,
            reset_reason = if reset { "quiet_period" } else { "none" }
        );
        BurstTicket {
            inner: Arc::new(TicketInner {
                gate: Arc::clone(self),
                state: Mutex::new(TicketState::default()),
            }),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, BurstState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BurstTicket {
    inner: Arc<TicketInner>,
}

#[derive(Debug)]
struct TicketInner {
    gate: Arc<BurstOutputGate>,
    state: Mutex<TicketState>,
}

#[derive(Debug, Default)]
struct TicketState {
    allowance: Option<usize>,
    response_cost: Option<usize>,
    completed: bool,
}

impl BurstTicket {
    pub(crate) fn allowance(&self) -> usize {
        let mut ticket = self.lock_state();
        if let Some(allowance) = ticket.allowance {
            return allowance;
        }
        let mut burst = self.inner.gate.lock_state();
        let remaining = self
            .inner
            .gate
            .limit
            .saturating_sub(burst.spent_tokens.saturating_add(burst.reserved_tokens));
        let allowance = remaining / burst.unclaimed_calls.max(1);
        burst.unclaimed_calls = burst.unclaimed_calls.saturating_sub(1);
        burst.reserved_tokens = burst.reserved_tokens.saturating_add(allowance);
        let burst_id = burst.burst_id;
        let active_calls = burst.active_calls;
        let unclaimed_calls = burst.unclaimed_calls;
        let reserved_tokens = burst.reserved_tokens;
        ticket.allowance = Some(allowance);
        drop(burst);
        drop(ticket);
        tracing::trace!(
            target: "codexshim",
            event = "burst_claim",
            burst_id,
            active_calls,
            unclaimed_calls,
            allowance,
            reserved_tokens
        );
        allowance
    }

    pub(crate) fn cache_response_cost(&self, tokens: usize) {
        self.lock_state().response_cost = Some(tokens);
    }

    pub(crate) fn cached_response_cost(&self) -> Option<usize> {
        self.lock_state().response_cost
    }

    pub(crate) fn finish(&self, actual_tokens: usize, limited: bool) {
        let mut ticket = self.lock_state();
        if ticket.completed {
            return;
        }
        let mut burst = self.inner.gate.lock_state();
        if let Some(allowance) = ticket.allowance {
            burst.reserved_tokens = burst.reserved_tokens.saturating_sub(allowance);
        } else {
            burst.unclaimed_calls = burst.unclaimed_calls.saturating_sub(1);
        }
        burst.spent_tokens = burst.spent_tokens.saturating_add(actual_tokens);
        burst.active_calls = burst.active_calls.saturating_sub(1);
        if burst.active_calls == 0 {
            burst.last_completed = Some(Instant::now());
        }
        let burst_id = burst.burst_id;
        let active_calls = burst.active_calls;
        let spent_tokens = burst.spent_tokens;
        let reserved_tokens = burst.reserved_tokens;
        let remaining = self
            .inner
            .gate
            .limit
            .saturating_sub(spent_tokens.saturating_add(reserved_tokens));
        let soft_overflow = spent_tokens.saturating_sub(self.inner.gate.limit);
        ticket.completed = true;
        drop(burst);
        drop(ticket);
        tracing::trace!(
            target: "codexshim",
            event = "burst_finish",
            burst_id,
            active_calls,
            actual_tokens,
            spent_tokens,
            reserved_tokens,
            remaining,
            limited,
            soft_overflow
        );
    }

    fn lock_state(&self) -> MutexGuard<'_, TicketState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for TicketInner {
    fn drop(&mut self) {
        let ticket = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ticket.completed {
            return;
        }
        let mut burst = self.gate.lock_state();
        if let Some(allowance) = ticket.allowance {
            burst.reserved_tokens = burst.reserved_tokens.saturating_sub(allowance);
        } else {
            burst.unclaimed_calls = burst.unclaimed_calls.saturating_sub(1);
        }
        burst.active_calls = burst.active_calls.saturating_sub(1);
        if burst.active_calls == 0 {
            burst.last_completed = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BurstOutputGate, DEFAULT_BURST_TOKENS};
    use std::time::Duration;

    #[test]
    fn active_calls_share_the_remaining_budget() {
        let gate = BurstOutputGate::new(DEFAULT_BURST_TOKENS);
        let first = gate.begin_call();
        let second = gate.begin_call();
        assert_eq!(first.allowance(), DEFAULT_BURST_TOKENS / 2);
        assert_eq!(second.allowance(), DEFAULT_BURST_TOKENS / 2);
        first.finish(3_000, false);
        second.finish(2_000, false);
        let third = gate.begin_call();
        assert_eq!(third.allowance(), DEFAULT_BURST_TOKENS - 5_000);
    }

    #[test]
    fn dropped_ticket_releases_its_reservation() {
        let gate = BurstOutputGate::new(DEFAULT_BURST_TOKENS);
        let first = gate.begin_call();
        let second = gate.begin_call();
        assert_eq!(first.allowance(), DEFAULT_BURST_TOKENS / 2);
        drop(first);
        assert_eq!(second.allowance(), DEFAULT_BURST_TOKENS);
    }

    #[test]
    fn panicking_owner_releases_unclaimed_and_reserved_state() {
        let gate = BurstOutputGate::new(DEFAULT_BURST_TOKENS);
        let sibling = gate.begin_call();
        let panic_gate = gate.clone();
        let panic = std::panic::catch_unwind(move || {
            let ticket = panic_gate.begin_call();
            assert!(ticket.allowance() > 0);
            panic!("ticket owner failed");
        });
        assert!(panic.is_err());
        assert_eq!(sibling.allowance(), DEFAULT_BURST_TOKENS);
    }

    #[test]
    fn quiet_period_resets_only_after_every_call_finishes() {
        let gate = BurstOutputGate::with_quiet_period(8_192, Duration::from_millis(10));
        let long = gate.begin_call();
        let first = gate.begin_call();
        first.finish(2_000, false);
        std::thread::sleep(Duration::from_millis(20));
        let same_burst = gate.begin_call();
        assert_eq!(same_burst.allowance(), (8_192 - 2_000) / 2);
        same_burst.finish(1_000, false);
        long.finish(1_000, false);
        std::thread::sleep(Duration::from_millis(20));
        let reset = gate.begin_call();
        assert_eq!(reset.allowance(), 8_192);
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        for value in ["0", "2047", "8193", "many", "-1"] {
            assert!(super::parse_burst_tokens(Some(OsStr::new(value))).is_err());
        }
        assert_eq!(
            super::parse_burst_tokens(Some(OsStr::new("4096"))).ok(),
            Some(4_096)
        );
    }

    use std::ffi::OsStr;
}
