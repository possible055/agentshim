#[cfg(test)]
thread_local! {
    pub static BEFORE_READ_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
    pub static AFTER_READ_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn run_before_read_hook() {
    BEFORE_READ_HOOK.with(|hook| {
        if let Some(mut hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
pub fn run_before_read_hook() {}

#[cfg(test)]
pub fn run_after_read_hook() {
    AFTER_READ_HOOK.with(|hook| {
        if let Some(mut hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
pub fn run_after_read_hook() {}

/// Forces the next N `execute_prepared` calls to report `file_changed`.
///
/// The existing read hooks are thread-locals, so they cannot reach work the server runs
/// on a blocking pool. Proving that the PDF gate survives a retry needs the retry to
/// happen on that path, deterministically, so this seam is global. Tests that use it hold
/// [`global_read_state_guard`].
#[cfg(any(test, feature = "test-hooks"))]
pub static FORCED_CHANGES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Shortens the PDF mode runtime ceiling to 1 ms so the timeout path can be exercised
/// without a five-second test.
#[cfg(any(test, feature = "test-hooks"))]
pub static FORCED_PDF_RUNTIME_LIMIT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(any(test, feature = "test-hooks"))]
pub fn forced_runtime_limit() -> Option<std::time::Duration> {
    match FORCED_PDF_RUNTIME_LIMIT.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        millis => Some(std::time::Duration::from_millis(millis)),
    }
}

#[cfg(not(any(test, feature = "test-hooks")))]
pub fn forced_runtime_limit() -> Option<std::time::Duration> {
    None
}

/// Serialises tests that read or write process-global read state: the forced-change seam,
/// the forced runtime ceiling, and the PDF gate acquisition counter. All are global, so
/// concurrent PDF tests would otherwise see each other's writes.
#[cfg(any(test, feature = "test-hooks"))]
pub fn global_read_state_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(any(test, feature = "test-hooks"))]
pub fn take_forced_change() -> bool {
    FORCED_CHANGES
        .fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |remaining| (remaining > 0).then(|| remaining - 1),
        )
        .is_ok()
}

#[cfg(not(any(test, feature = "test-hooks")))]
pub fn take_forced_change() -> bool {
    false
}
