#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use crate::runtime::{
        DEFAULT_GLOB_MEMORY_BYTES, DEFAULT_GREP_MEMORY_BYTES, DEFAULT_MEMORY_BYTES,
        DEFAULT_PDF_IMAGE_MEMORY_BYTES, DEFAULT_PDF_TEXT_MEMORY_BYTES, DEFAULT_PROCESS_CALLS,
        GLOB_MEMORY_BYTES_ENV, GREP_MEMORY_BYTES_ENV, MAX_CONFIGURED_PROCESS_CALLS,
        MAX_PDF_IMAGE_MEMORY_BYTES, MAX_PDF_TEXT_MEMORY_BYTES, MAX_READ_ONLY_CALLS,
        MAX_TOOL_MEMORY_BYTES, MIN_PDF_IMAGE_MEMORY_BYTES, MIN_PDF_TEXT_MEMORY_BYTES,
        MIN_TOOL_MEMORY_BYTES, MemoryReservation, PDF_IMAGE_MEMORY_BYTES_ENV,
        PDF_TEXT_MEMORY_BYTES_ENV, RuntimeConfig, RuntimeResources, blocking_threads,
        default_scheduler_threads, default_worker_lanes, global_memory_bytes,
        parse_memory_bytes_in_range, parse_process_calls, parse_tool_memory_bytes,
    };
    use tokio_util::sync::CancellationToken;

    /// The shared grep/glob parser fixes one 8 MiB–1 GiB range; the PDF variables need
    /// narrower ones, and reusing that helper would silently accept out-of-range values.
    #[test]
    fn pdf_memory_variables_use_their_own_ranges() {
        for (environment, default, minimum, maximum) in [
            (
                PDF_TEXT_MEMORY_BYTES_ENV,
                DEFAULT_PDF_TEXT_MEMORY_BYTES,
                MIN_PDF_TEXT_MEMORY_BYTES,
                MAX_PDF_TEXT_MEMORY_BYTES,
            ),
            (
                PDF_IMAGE_MEMORY_BYTES_ENV,
                DEFAULT_PDF_IMAGE_MEMORY_BYTES,
                MIN_PDF_IMAGE_MEMORY_BYTES,
                MAX_PDF_IMAGE_MEMORY_BYTES,
            ),
        ] {
            let parse = |value: String| {
                parse_memory_bytes_in_range(
                    Some(OsStr::new(&value)),
                    environment,
                    default,
                    minimum,
                    maximum,
                )
            };
            assert_eq!(
                parse_memory_bytes_in_range(None, environment, default, minimum, maximum)
                    .expect("default"),
                default
            );
            assert_eq!(parse(minimum.to_string()).expect("minimum"), minimum);
            assert_eq!(parse(maximum.to_string()).expect("maximum"), maximum);
            assert!(parse((minimum - 1).to_string()).is_err());
            assert!(parse((maximum + 1).to_string()).is_err());
            // Inside the shared helper's range but outside this variable's.
            assert!(parse(MAX_TOOL_MEMORY_BYTES.to_string()).is_err());
            assert!(
                parse_tool_memory_bytes(
                    Some(OsStr::new(&MAX_TOOL_MEMORY_BYTES.to_string())),
                    environment,
                    default
                )
                .is_ok(),
                "the shared helper is the wrong range for {environment}"
            );
        }
    }

    /// Both mode reservations must fit the pool they are drawn from, or the call could
    /// never be admitted.
    #[test]
    fn pdf_mode_reservations_fit_the_shared_pool() {
        let config = RuntimeConfig::for_tests(1);
        assert!(config.pdf_text_memory_bytes <= config.memory_bytes);
        assert!(config.pdf_image_memory_bytes <= config.memory_bytes);
        // Const-evaluable, so `assert!` would be optimised out; compare at runtime.
        assert_eq!(
            MAX_PDF_TEXT_MEMORY_BYTES.min(DEFAULT_MEMORY_BYTES),
            MAX_PDF_TEXT_MEMORY_BYTES
        );
        assert_eq!(
            MAX_PDF_IMAGE_MEMORY_BYTES.min(DEFAULT_MEMORY_BYTES),
            MAX_PDF_IMAGE_MEMORY_BYTES
        );
    }

    #[tokio::test]
    async fn the_pdf_gate_admits_one_call_and_recovers_on_drop() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(4));
        let request = CancellationToken::new();

        let first = resources
            .acquire_pdf_gate(&request)
            .await
            .expect("first PDF call");
        assert!(resources.try_acquire_pdf_gate().is_none());

        // The bounded wait must fail rather than queue indefinitely.
        let waited = std::time::Instant::now();
        assert!(resources.acquire_pdf_gate(&request).await.is_none());
        let waited = waited.elapsed();
        assert!(
            waited >= crate::runtime::PDF_GATE_WAIT,
            "gate returned before its bounded wait elapsed: {waited:?}"
        );
        assert!(
            waited < crate::runtime::PDF_GATE_WAIT * 4,
            "gate waited far past its bound: {waited:?}"
        );

        drop(first);
        assert!(resources.try_acquire_pdf_gate().is_some());
    }

    /// A text read must never queue behind a PDF's share of the pool. With the mode
    /// reservations charged, the pool keeps enough headroom for one.
    #[tokio::test]
    async fn a_charged_pdf_reservation_leaves_room_for_text_reads() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(4));
        let config = resources.config();

        let image = resources
            .try_reserve_memory(config.pdf_image_memory_bytes)
            .expect("image mode reservation");
        let text = resources
            .try_reserve_memory(256 * 1024)
            .expect("a text read must not have to wait behind the PDF reservation");
        assert!(config.pdf_image_memory_bytes < config.memory_bytes);
        drop((image, text));
    }

    /// Mirrors the admission the server performs: a PDF takes its gate and mode
    /// reservation, then an unrelated text read asks for its 256 KiB the waiting way.
    ///
    /// This is the defect the split exists to remove. A PDF that reserves the whole pool
    /// makes the text read's wait unbounded in practice, so the assertion is that the
    /// wait resolves promptly, not merely that it eventually resolves.
    #[tokio::test]
    async fn a_running_pdf_does_not_stall_a_concurrent_text_read() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(4));
        let request = CancellationToken::new();
        let config = resources.config();

        let gate = resources
            .acquire_pdf_gate(&request)
            .await
            .expect("PDF gate");
        let reservation = resources
            .try_reserve_memory(config.pdf_image_memory_bytes)
            .expect("image mode reservation");

        let started = std::time::Instant::now();
        let text = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            resources.reserve_memory(256 * 1024, &request),
        )
        .await
        .expect("a text read must not wait on the PDF reservation")
        .expect("text reservation");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "text read waited {:?} behind a PDF",
            started.elapsed()
        );

        drop((gate, reservation, text));
    }

    /// The behaviour the split replaced: a call that reserves the whole pool starves
    /// every other tool. Kept as the contrast case, and as the reason the PDF charge is
    /// now a per-mode fraction rather than the pool.
    #[tokio::test]
    async fn a_whole_pool_reservation_would_block_text_reads() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(4));
        let request = CancellationToken::new();

        let whole_pool = resources
            .try_reserve_memory(DEFAULT_MEMORY_BYTES)
            .expect("whole-pool reservation");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                resources.reserve_memory(256 * 1024, &request),
            )
            .await
            .is_err(),
            "a whole-pool reservation is supposed to starve everything else"
        );
        drop(whole_pool);
    }

    /// The per-mode charge is only truthful because the core refuses allocations above
    /// the same ceilings. If the reservation is ever raised above what the parser
    /// enforces, the scheduler goes back to describing rather than bounding.
    #[test]
    fn mode_reservations_match_the_ceilings_the_core_enforces() {
        let config = RuntimeConfig::for_tests(1);
        assert_eq!(
            config.pdf_text_memory_bytes,
            codexshim_pdf_read::PdfResourceLimits::text().call_total_bytes
        );
        assert_eq!(
            config.pdf_image_memory_bytes,
            codexshim_pdf_read::PdfResourceLimits::image().call_total_bytes
        );
    }

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
                blocking_threads(calls, crate::tools::bash::detached::DEFAULT_DETACHED_CALLS),
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
        assert!(!pool.is_initialized());
        let credits = pool.try_credits(usize::MAX);
        assert_eq!(credits.len(), 3);
        assert!(!pool.is_initialized());
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
        assert!(!inline.is_initialized());
    }

    #[test]
    fn file_work_panic_poisoning_falls_back_to_inline() {
        let pool = RuntimeResources::new(RuntimeConfig::for_tests(2)).file_work_pool();
        assert!(!pool.is_initialized());
        let credit = pool.try_credit().expect("extra credit");
        let (sent, received) = std::sync::mpsc::channel();
        assert!(
            pool.spawn(credit, move |_| {
                sent.send(()).expect("notify panic");
                panic!("injected file worker panic");
            })
            .is_ok()
        );
        assert!(pool.is_initialized());
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
        assert!(
            task.await
                .expect_err("task must be cancelled")
                .is_cancelled()
        );
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
