#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::{
        DEFAULT_PROCESS_CALLS, MAX_CONFIGURED_PROCESS_CALLS, MAX_READ_ONLY_CALLS,
        MEMORY_SOFT_TARGET_BYTES, RuntimeConfig, RuntimeResources, blocking_threads,
        default_scheduler_threads, default_worker_lanes, parse_process_calls,
    };
    use tokio_util::sync::CancellationToken;

    #[test]
    fn default_workers_allow_bounded_io_overlap() {
        #[cfg(windows)]
        {
            assert_eq!(default_worker_lanes(1), 4);
            assert_eq!(default_worker_lanes(2), 8);
            assert_eq!(default_worker_lanes(4), 16);
            assert_eq!(default_worker_lanes(64), 16);
        }
        #[cfg(not(windows))]
        {
        assert_eq!(default_worker_lanes(1), 2);
        assert_eq!(default_worker_lanes(2), 4);
        assert_eq!(default_worker_lanes(4), 8);
        assert_eq!(default_worker_lanes(64), 8);
        }
        assert_eq!(default_scheduler_threads(1), 1);
        assert_eq!(default_scheduler_threads(64), 2);
    }

    #[test]
    fn process_call_configuration_is_bounded_and_determines_blocking_capacity() {
        assert_eq!(
            parse_process_calls(None).expect("default process calls"),
            DEFAULT_PROCESS_CALLS
        );
        for (value, expected_threads) in [("1", 19), ("16", 34), ("32", 50)] {
            let calls = parse_process_calls(Some(OsStr::new(value))).expect("valid process calls");
            assert_eq!(blocking_threads(calls), expected_threads);
        }
        for value in ["0", "33", "-1", "many"] {
            let error =
                parse_process_calls(Some(OsStr::new(value))).expect_err("invalid process calls");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
        assert_eq!(MAX_CONFIGURED_PROCESS_CALLS, 32);
    }

    #[test]
    fn file_work_credits_are_try_only_and_recover_on_drop() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(4));
        let pool = resources.file_work_pool();
        assert_eq!(pool.extra_capacity(), 3);
        let credits = (0..3)
            .map(|_| pool.try_credit().expect("extra credit"))
            .collect::<Vec<_>>();
        assert!(pool.try_credit().is_none());
        drop(credits);
        assert_eq!(pool.available_credits(), 3);

        let inline = RuntimeResources::new(RuntimeConfig::for_tests(1)).file_work_pool();
        assert_eq!(inline.extra_capacity(), 0);
        assert!(inline.try_credit().is_none());
    }

    #[test]
    fn file_work_panic_poisoning_falls_back_to_inline() {
        let pool = RuntimeResources::new(RuntimeConfig::for_tests(2)).file_work_pool();
        let credit = pool.try_credit().expect("extra credit");
        let (sent, received) = std::sync::mpsc::channel();
        assert!(
            pool.spawn(credit, move |_| {
                sent.send(()).expect("notify panic");
                panic!("injected file worker panic");
            })
            .is_ok()
        );
        received
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("worker started");
        for _ in 0..5_000 {
            if pool.is_poisoned() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(pool.is_poisoned());
        assert!(pool.try_credit().is_none());
    }

    #[tokio::test]
    async fn memory_reservations_use_a_soft_target() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let request = CancellationToken::new();
        let oversized = resources
            .reserve_memory(MEMORY_SOFT_TARGET_BYTES + 1, &request)
            .await
            .expect("soft target must not fail the request");
        assert!(resources.try_reserve_memory(1).is_none());
        drop(oversized);
        let reservation = resources
            .try_reserve_memory(MEMORY_SOFT_TARGET_BYTES)
            .expect("try reservation");
        assert!(resources.try_reserve_memory(1).is_none());
        drop(reservation);
        assert!(resources.try_reserve_memory(1).is_some());
    }

    #[test]
    fn class_admission_is_fail_fast_independent_and_recovers_on_drop() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let read_permits = (0..MAX_READ_ONLY_CALLS)
            .map(|_| resources.try_admit_read_only().expect("read admission"))
            .collect::<Vec<_>>();
        let process_permits = (0..DEFAULT_PROCESS_CALLS)
            .map(|_| resources.try_admit_process().expect("process admission"))
            .collect::<Vec<_>>();

        assert!(resources.try_admit_read_only().is_none());
        assert!(resources.try_admit_process().is_none());
        drop(read_permits);
        assert!(resources.try_admit_read_only().is_some());
        assert!(resources.try_admit_process().is_none());
        drop(process_permits);
        assert!(resources.try_admit_process().is_some());
    }

    #[tokio::test]
    async fn process_admission_recovers_after_worker_panic() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let permit = resources.try_admit_process().expect("process admission");
        let panic = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            panic!("injected worker panic");
        })
        .await;
        assert!(panic.expect_err("worker must panic").is_panic());
        assert!(resources.try_admit_process().is_some());
    }

    #[tokio::test]
    async fn process_admission_recovers_after_task_cancellation() {
        let mut config = RuntimeConfig::for_tests(1);
        config.process_calls = 1;
        config.blocking_threads = blocking_threads(config.process_calls);
        let resources = RuntimeResources::new(config);
        let permit = resources.try_admit_process().expect("process admission");
        let task = tokio::spawn(async move {
            let _permit = permit;
            std::future::pending::<()>().await;
        });

        assert!(resources.try_admit_process().is_none());
        task.abort();
        assert!(task.await.expect_err("task must be cancelled").is_cancelled());
        assert!(resources.try_admit_process().is_some());
    }

    #[test]
    fn process_admission_uses_the_runtime_configuration() {
        let mut config = RuntimeConfig::for_tests(1);
        config.process_calls = 2;
        config.blocking_threads = blocking_threads(config.process_calls);
        let resources = RuntimeResources::new(config);
        let permits = (0..2)
            .map(|_| resources.try_admit_process().expect("process admission"))
            .collect::<Vec<_>>();

        assert!(resources.try_admit_process().is_none());
        drop(permits);
        assert!(resources.try_admit_process().is_some());
    }
}
