#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use crate::runtime::{
        DEFAULT_BACKGROUND_JOB_TIMEOUT_MAX, DEFAULT_GLOB_MEMORY_BYTES,
        DEFAULT_GREP_CONCURRENT_CALLS, DEFAULT_GREP_MEMORY_BYTES, DEFAULT_MEMORY_BYTES,
        DEFAULT_PDF_IMAGE_MEMORY_BYTES, DEFAULT_PDF_TEXT_MEMORY_BYTES, DEFAULT_PROCESS_CALLS,
        DEFAULT_TOOL_TIMEOUT_SHELF, GLOB_MEMORY_BYTES_ENV, GREP_MEMORY_BYTES_ENV,
        MAX_BACKGROUND_JOB_TIMEOUT_MAX, MAX_IDLE_TIMEOUT, MAX_PDF_IMAGE_MEMORY_BYTES,
        MAX_PDF_TEXT_MEMORY_BYTES, MAX_READ_ONLY_CALLS, MAX_TOOL_MEMORY_BYTES,
        MAX_TOOL_TIMEOUT_SHELF, MIN_IDLE_TIMEOUT, MIN_PDF_IMAGE_MEMORY_BYTES,
        MIN_PDF_TEXT_MEMORY_BYTES, MIN_TOOL_MEMORY_BYTES, MemoryReservation,
        PDF_IMAGE_MEMORY_BYTES_ENV, PDF_TEXT_MEMORY_BYTES_ENV, RESPECT_GITIGNORE_ENV,
        RuntimeConfig, RuntimeResources, blocking_threads, global_memory_bytes,
        parse_background_job_timeout_max, parse_idle_timeout, parse_memory_bytes_in_range,
        parse_process_calls, parse_respect_gitignore, parse_tool_memory_bytes,
        parse_tool_timeout_shelf,
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

    #[test]
    fn respect_gitignore_defaults_false_and_accepts_boolean_tokens() {
        assert!(!parse_respect_gitignore(None).expect("default"));
        for value in ["0", "false", "False", " FALSE "] {
            assert!(!parse_respect_gitignore(Some(OsStr::new(value))).expect(value));
        }
        for value in ["1", "true", "True", " TRUE "] {
            assert!(parse_respect_gitignore(Some(OsStr::new(value))).expect(value));
        }
        for value in ["yes", "2", ""] {
            let error = parse_respect_gitignore(Some(OsStr::new(value))).expect_err(value);
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains(RESPECT_GITIGNORE_ENV));
        }
        let config = RuntimeConfig::for_tests(1);
        assert!(config.include_ignored(None));
        assert!(!config.include_ignored(Some(false)));
        assert!(config.include_ignored(Some(true)));
        let mut respect = config;
        respect.respect_gitignore = true;
        assert!(!respect.include_ignored(None));
        assert!(respect.include_ignored(Some(true)));
    }

    #[test]
    fn process_call_configuration_is_bounded() {
        assert_eq!(
            parse_process_calls(None).expect("default process calls"),
            DEFAULT_PROCESS_CALLS
        );
        assert_eq!(
            parse_process_calls(Some(OsStr::new("1"))).expect("minimum process calls"),
            1
        );
        for value in ["0", "33", "-1", "many"] {
            let error =
                parse_process_calls(Some(OsStr::new(value))).expect_err("invalid process calls");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn tool_timeout_shelf_defaults_and_bounds_match_the_documented_range() {
        assert_eq!(
            parse_tool_timeout_shelf(None).expect("default shelf"),
            DEFAULT_TOOL_TIMEOUT_SHELF,
        );
        assert_eq!(
            parse_tool_timeout_shelf(Some(OsStr::new("600"))).expect("explicit default"),
            DEFAULT_TOOL_TIMEOUT_SHELF,
        );
        assert_eq!(
            parse_tool_timeout_shelf(Some(OsStr::new("300"))).expect("lower shelf"),
            std::time::Duration::from_secs(300),
        );
        assert_eq!(
            parse_tool_timeout_shelf(Some(OsStr::new("3600"))).expect("maximum shelf"),
            MAX_TOOL_TIMEOUT_SHELF,
        );
        for value in ["14", "3601", "0", "-1", "many"] {
            let error =
                parse_tool_timeout_shelf(Some(OsStr::new(value))).expect_err("invalid shelf");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn background_job_timeout_defaults_and_bounds_match_the_host_contract() {
        assert_eq!(
            parse_background_job_timeout_max(None).expect("default"),
            DEFAULT_BACKGROUND_JOB_TIMEOUT_MAX,
        );
        assert_eq!(
            parse_background_job_timeout_max(Some(OsStr::new("600"))).expect("minimum"),
            std::time::Duration::from_secs(600),
        );
        assert_eq!(
            parse_background_job_timeout_max(Some(OsStr::new("14400"))).expect("maximum"),
            MAX_BACKGROUND_JOB_TIMEOUT_MAX,
        );
        for value in ["599", "14401", "0", "-1", "many"] {
            let error = parse_background_job_timeout_max(Some(OsStr::new(value)))
                .expect_err("invalid background maximum");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn idle_timeout_is_opt_in_and_bounded() {
        assert_eq!(parse_idle_timeout(None).expect("disabled"), None);
        assert_eq!(
            parse_idle_timeout(Some(OsStr::new("1"))).expect("minimum"),
            Some(MIN_IDLE_TIMEOUT)
        );
        assert_eq!(
            parse_idle_timeout(Some(OsStr::new("86400"))).expect("maximum"),
            Some(MAX_IDLE_TIMEOUT)
        );
        for value in ["0", "86401", "-1", "many", ""] {
            let error = parse_idle_timeout(Some(OsStr::new(value))).expect_err(value);
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("AGENTSHIM_IDLE_TIMEOUT"));
        }
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
        let first_share = pool.try_credits(2);
        let second_share = pool.try_credits(1);
        assert_eq!(first_share.len(), 2);
        assert_eq!(second_share.len(), 1);
        drop((first_share, second_share));
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
        let mut reservation = MemoryReservation::from_initial(&resources, initial, 1024 * 1024);
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
                MemoryReservation::from_initial(&panic_resources, initial, 1024 * 1024);
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

    #[test]
    fn file_work_pool_allows_concurrent_requests() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(3));
        let pool = resources.file_work_pool();
        let _first_request = pool.begin_request();
        let _second_request = pool.begin_request();

        let first_share = pool.try_credits(1);
        let second_share = pool.try_credits(1);
        assert_eq!(first_share.len(), 1);
        assert_eq!(second_share.len(), 1);
    }

    #[test]
    fn grep_concurrent_calls_admission_limit() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let permits = (0..DEFAULT_GREP_CONCURRENT_CALLS)
            .map(|_| resources.try_admit_grep().expect("grep admission"))
            .collect::<Vec<_>>();

        assert!(resources.try_admit_grep().is_none());
        drop(permits);
        assert!(resources.try_admit_grep().is_some());
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
