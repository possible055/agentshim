#[cfg(test)]
mod tests {
    use super::{
        AcquireError, MAX_PROCESS_CALLS, MEMORY_BUDGET_BYTES, RuntimeConfig, RuntimeResources,
        default_scheduler_threads, default_worker_lanes,
    };
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn read_only_admission_is_bounded_and_cancellable() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let request = CancellationToken::new();
        let mut permits = Vec::new();
        for _ in 0..super::MAX_READ_ONLY_CALLS {
            permits.push(
                resources
                    .acquire_read_only(&request)
                    .await
                    .expect("acquire slot"),
            );
        }

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            resources.acquire_read_only(&cancelled).await.unwrap_err(),
            AcquireError::Cancelled
        );
        drop(permits);
    }

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

    #[tokio::test]
    async fn process_admission_is_independent_bounded_and_cancellable() {
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let request = CancellationToken::new();
        let mut permits = Vec::new();
        for _ in 0..MAX_PROCESS_CALLS {
            permits.push(
                resources
                    .acquire_process(&request)
                    .await
                    .expect("process slot"),
            );
        }
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            resources.acquire_process(&cancelled).await.unwrap_err(),
            AcquireError::Cancelled
        );
        drop(permits);
        let _read_only = resources
            .acquire_read_only(&request)
            .await
            .expect("read-only admission remains independent");
    }
}
