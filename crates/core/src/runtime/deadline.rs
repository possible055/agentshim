use std::time::Instant;

use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeadlineEvent {
    Expired,
    Finished,
    Shutdown,
}

pub async fn wait(
    deadline: Instant,
    finished: CancellationToken,
    shutdown: CancellationToken,
) -> DeadlineEvent {
    tokio::select! {
        biased;
        () = finished.cancelled() => DeadlineEvent::Finished,
        () = shutdown.cancelled() => DeadlineEvent::Shutdown,
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => DeadlineEvent::Expired,
    }
}
