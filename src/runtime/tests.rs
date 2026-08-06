#[cfg(test)]
mod tests {
    use super::{
        AcquireError, MAX_PROCESS_CALLS, MEMORY_BUDGET_BYTES, RuntimeConfig, RuntimeResources,
        default_scheduler_threads, default_worker_lanes,
    };
    use tokio_util::sync::CancellationToken;

    #[test]
    fn default_workers_allow_bounded_io_overlap() {
        assert_eq!(default_worker_lanes(1), 2);
        assert_eq!(default_worker_lanes(2), 4);
        assert_eq!(default_worker_lanes(4), 8);
        assert_eq!(default_worker_lanes(64), 8);
        assert_eq!(default_scheduler_threads(1), 1);
        assert_eq!(default_scheduler_threads(64), 2);
    }

    #[tokio::test]
    async fn search_lanes_preserve_global_fair_share() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(4));
        let request = CancellationToken::new();
        let lanes = resources
            .acquire_search_lanes(4, &request)
            .await
            .expect("search lanes");
        assert_eq!(lanes.len(), 3);
        let other = resources
            .acquire_worker(&request)
            .await
            .expect("reserved lane");
        drop(other);
        drop(lanes);
    }

    #[tokio::test]
    async fn memory_reservations_are_hard_bounded() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let request = CancellationToken::new();
        assert_eq!(
            resources
                .reserve_memory(MEMORY_BUDGET_BYTES + 1, &request)
                .await
                .unwrap_err(),
            AcquireError::TooLarge
        );
    }

    #[test]
    fn class_admission_is_fail_fast_independent_and_recovers_on_drop() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let read_permits = (0..super::MAX_READ_ONLY_CALLS)
            .map(|_| resources.try_admit_read_only().expect("read admission"))
            .collect::<Vec<_>>();
        let process_permits = (0..MAX_PROCESS_CALLS)
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
}
