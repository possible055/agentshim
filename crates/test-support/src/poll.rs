use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// Cadence between condition polls: short enough that deadlines stay accurate,
/// long enough not to spin. Every wait in test code should go through the
/// helpers in this module; a bare `thread::sleep` cannot express "wait for the
/// event" and turns slow CI machines into flaky tests.
pub const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Polls `condition` until it returns `Some`, or panics naming `what` at `deadline`.
pub fn poll_until<T>(deadline: Instant, what: &str, mut condition: impl FnMut() -> Option<T>) -> T {
    loop {
        if let Some(value) = condition() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after waiting for {what}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

/// [`poll_until`] with a relative timeout.
pub fn poll_until_within<T>(
    timeout: Duration,
    what: &str,
    condition: impl FnMut() -> Option<T>,
) -> T {
    poll_until(Instant::now() + timeout, what, condition)
}

/// Asserts that `alive` still holds at every poll until `deadline` passes,
/// then returns normally. The observation-driven counterpart to sleeping a
/// fixed duration and hoping a process was still alive throughout.
pub fn hold_until(deadline: Instant, what: &str, mut alive: impl FnMut() -> bool) {
    loop {
        assert!(alive(), "{what}");
        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Waits until `path` has not changed size for `quiet`, returning the settled
/// size. Panics if the file keeps changing past `deadline`. Use this instead of
/// sleep-and-compare when asserting that a detached process stopped writing.
pub fn wait_for_quiet(path: &Path, quiet: Duration, deadline: Instant, what: &str) -> u64 {
    let mut last_size = file_size(path);
    let mut last_change = Instant::now();
    loop {
        thread::sleep(POLL_INTERVAL);
        assert!(
            Instant::now() < deadline,
            "{what} never settled before the deadline"
        );
        let size = file_size(path);
        if size != last_size {
            last_size = size;
            last_change = Instant::now();
        } else if last_change.elapsed() >= quiet {
            return size;
        }
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |metadata| metadata.len())
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use super::{poll_until_within, wait_for_quiet};

    #[test]
    fn poll_until_within_returns_first_observed_value() {
        let value = poll_until_within(Duration::from_secs(1), "immediate value", || Some(7));
        assert_eq!(value, 7);
    }

    #[test]
    #[should_panic(expected = "timed out after waiting for nothing")]
    fn poll_until_within_panics_at_the_deadline() {
        poll_until_within::<()>(Duration::from_millis(50), "nothing", || None);
    }

    #[test]
    fn wait_for_quiet_returns_the_settled_size() {
        let fixture = tempfile::tempdir().expect("fixture");
        let marker = fixture.path().join("marker");
        fs::write(&marker, b"payload").expect("write marker");
        let settled = wait_for_quiet(
            &marker,
            Duration::from_millis(30),
            std::time::Instant::now() + Duration::from_secs(2),
            "marker",
        );
        assert_eq!(settled, 7);
    }
}
