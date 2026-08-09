#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use super::{
        DEFAULT_GLOB_MEMORY_BYTES, DEFAULT_GREP_MEMORY_BYTES, DEFAULT_MEMORY_BYTES,
        DEFAULT_PROCESS_CALLS, GLOB_MEMORY_BYTES_ENV, GREP_MEMORY_BYTES_ENV,
        MAX_CONFIGURED_PROCESS_CALLS, MAX_READ_ONLY_CALLS, MAX_TOOL_MEMORY_BYTES,
        MIN_TOOL_MEMORY_BYTES, MemoryReservation, RuntimeConfig, RuntimeResources,
        blocking_threads, default_scheduler_threads, default_worker_lanes, global_memory_bytes,
        parse_process_calls, parse_tool_memory_bytes,
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
        for (value, expected_threads) in [("1", 35), ("16", 50), ("32", 66)] {
            let calls = parse_process_calls(Some(OsStr::new(value))).expect("valid process calls");
            assert_eq!(
                blocking_threads(
                    calls,
                    crate::tools::bash::detached::DEFAULT_DETACHED_CALLS
                ),
                expected_threads
            );
        }
        for value in ["0", "33", "-1", "many"] {
            let error =
                parse_process_calls(Some(OsStr::new(value))).expect_err("invalid process calls");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
        assert_eq!(MAX_CONFIGURED_PROCESS_CALLS, 32);
    }

    #[test]
    fn search_memory_configuration_defaults_bounds_and_global_capacity() {
        assert_eq!(
            parse_tool_memory_bytes(None, GREP_MEMORY_BYTES_ENV, DEFAULT_GREP_MEMORY_BYTES)
                .expect("grep default"),
            DEFAULT_GREP_MEMORY_BYTES
        );
        assert_eq!(
            parse_tool_memory_bytes(None, GLOB_MEMORY_BYTES_ENV, DEFAULT_GLOB_MEMORY_BYTES)
                .expect("glob default"),
            DEFAULT_GLOB_MEMORY_BYTES
        );
        for value in [MIN_TOOL_MEMORY_BYTES, MAX_TOOL_MEMORY_BYTES] {
            let rendered = value.to_string();
            assert_eq!(
                parse_tool_memory_bytes(
                    Some(OsStr::new(&rendered)),
                    GREP_MEMORY_BYTES_ENV,
                    DEFAULT_GREP_MEMORY_BYTES
                )
                .expect("valid bound"),
                value
            );
        }
        for value in [
            "0".to_owned(),
            (MIN_TOOL_MEMORY_BYTES - 1).to_string(),
            (MAX_TOOL_MEMORY_BYTES + 1).to_string(),
            "many".to_owned(),
            "-1".to_owned(),
        ] {
            assert!(
                parse_tool_memory_bytes(
                    Some(OsStr::new(&value)),
                    GREP_MEMORY_BYTES_ENV,
                    DEFAULT_GREP_MEMORY_BYTES
                )
                .is_err(),
                "{value} must be rejected"
            );
        }
        let invalid = invalid_unicode();
        assert!(
            parse_tool_memory_bytes(
                Some(&invalid),
                GREP_MEMORY_BYTES_ENV,
                DEFAULT_GREP_MEMORY_BYTES
            )
            .is_err()
        );
        assert_eq!(
            global_memory_bytes(DEFAULT_GREP_MEMORY_BYTES, DEFAULT_GLOB_MEMORY_BYTES),
            DEFAULT_MEMORY_BYTES
        );
        assert_eq!(
            global_memory_bytes(MAX_TOOL_MEMORY_BYTES, DEFAULT_GLOB_MEMORY_BYTES),
            MAX_TOOL_MEMORY_BYTES
        );
    }

    #[cfg(windows)]
    fn invalid_unicode() -> OsString {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xD800])
    }

    #[cfg(unix)]
    fn invalid_unicode() -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xFF])
    }

    #[test]
    fn file_work_credits_are_try_only_and_recover_on_drop() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(4));
        let pool = resources.file_work_pool();
        assert_eq!(pool.extra_capacity(), 3);
        let credits = pool.try_credits(usize::MAX);
        assert_eq!(credits.len(), 3);
        assert!(pool.try_credit().is_none());
        drop(credits);
        assert_eq!(pool.available_credits(), 3);

        let first_request = pool.begin_request();
        let second_request = pool.begin_request();
        assert!(pool.try_credits(3).is_empty());
        drop(second_request);
        assert_eq!(pool.try_credits(2).len(), 2);
        drop(first_request);

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
            .reserve_memory(DEFAULT_MEMORY_BYTES + 1, &request)
            .await
            .expect("soft target must not fail the request");
        assert!(resources.try_reserve_memory(1).is_none());
        drop(oversized);
        let reservation = resources
            .try_reserve_memory(DEFAULT_MEMORY_BYTES)
            .expect("try reservation");
        assert!(resources.try_reserve_memory(1).is_none());
        drop(reservation);
        assert!(resources.try_reserve_memory(1).is_some());
    }

    #[test]
    fn dynamic_memory_reservations_fail_fast_and_recover_on_drop_and_panic() {
        let mut config = RuntimeConfig::for_tests(1);
        config.memory_bytes = MIN_TOOL_MEMORY_BYTES;
        let resources = RuntimeResources::new(config);
        let initial = resources
            .try_reserve_memory(1024 * 1024)
            .expect("initial reservation");
        let mut reservation =
            MemoryReservation::from_initial(resources.clone(), initial, 1024 * 1024);
        let pressure = resources
            .try_reserve_memory(MIN_TOOL_MEMORY_BYTES - 1024 * 1024)
            .expect("competing reservation");
        assert!(!reservation.try_grow_to(2 * 1024 * 1024));
        drop(pressure);
        assert!(reservation.try_grow_to(2 * 1024 * 1024));
        drop(reservation);

        let panic_resources = resources.clone();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let initial = panic_resources
                .try_reserve_memory(1024 * 1024)
                .expect("panic reservation");
            let _reservation =
                MemoryReservation::from_initial(panic_resources, initial, 1024 * 1024);
            panic!("injected reservation panic");
        }));
        assert!(panic.is_err());
        assert!(
            resources
                .try_reserve_memory(MIN_TOOL_MEMORY_BYTES)
                .is_some()
        );
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
        config.blocking_threads = blocking_threads(config.process_calls, config.detached_calls);
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
        config.blocking_threads = blocking_threads(config.process_calls, config.detached_calls);
        let resources = RuntimeResources::new(config);
        let permits = (0..2)
            .map(|_| resources.try_admit_process().expect("process admission"))
            .collect::<Vec<_>>();

        assert!(resources.try_admit_process().is_none());
        drop(permits);
        assert!(resources.try_admit_process().is_some());
    }
}
