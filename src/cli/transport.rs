use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::SystemTime,
};

use codexshim::ToolsListCorrelation;
use rmcp::{
    RoleServer,
    model::{ClientNotification, ClientRequest, GetExtensions, JsonRpcMessage, RequestId},
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(super) const MAX_RECEIVE_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOOLS_LIST_CORRELATIONS: usize = 256;
const MAX_CORRELATION_REQUEST_ID_BYTES: usize = 256;

pub(super) struct ReceiveFrameReader<R> {
    inner: R,
    frame_bytes: usize,
    failed: bool,
}

impl<R> ReceiveFrameReader<R> {
    pub(super) fn new(inner: R) -> Self {
        Self {
            inner,
            frame_bytes: 0,
            failed: false,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for ReceiveFrameReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.failed {
            return Poll::Ready(Err(frame_too_large()));
        }

        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if let Poll::Ready(Ok(())) = result {
            let mut frame_bytes = self.frame_bytes;
            for byte in &buffer.filled()[before..] {
                if *byte == b'\n' {
                    frame_bytes = 0;
                } else {
                    frame_bytes += 1;
                    if frame_bytes > MAX_RECEIVE_FRAME_BYTES {
                        tracing::warn!(
                            target: "codexshim",
                            event = "mcp_frame_rejected",
                            phase = "transport",
                            outcome = "rejected",
                            error_class = "validation",
                            reason = "frame_too_large",
                            frame_limit_bytes = MAX_RECEIVE_FRAME_BYTES
                        );
                        buffer.set_filled(before);
                        self.failed = true;
                        return Poll::Ready(Err(frame_too_large()));
                    }
                }
            }
            self.frame_bytes = frame_bytes;
        }
        result
    }
}

fn frame_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("encoded JSON-RPC frame exceeds {MAX_RECEIVE_FRAME_BYTES} bytes before delimiter"),
    )
}

pub(super) struct ShutdownReader<R> {
    pub(super) inner: R,
    pub(super) shutdown: CancellationToken,
    pub(super) termination_reported: bool,
}

impl<R: AsyncRead + Unpin> AsyncRead for ShutdownReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.shutdown.is_cancelled() {
            return Poll::Ready(Ok(()));
        }
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        match &result {
            Poll::Ready(Err(error)) => {
                if !self.termination_reported {
                    tracing::error!(target: "codexshim", event = "stdin_read_error", phase = "transport", error_class = "io", io_kind = ?error.kind());
                    self.termination_reported = true;
                }
                self.shutdown.cancel();
            }
            Poll::Ready(Ok(())) if buffer.filled().len() == before => {
                if !self.termination_reported {
                    tracing::info!(target: "codexshim", event = "stdin_eof", phase = "transport", outcome = "shutdown");
                    self.termination_reported = true;
                }
                self.shutdown.cancel();
            }
            _ => {}
        }
        result
    }
}

#[derive(Clone)]
pub(super) struct TransportFailure(Arc<AtomicBool>);

impl TransportFailure {
    pub(super) fn failed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub(super) struct DiagnosticTransport<T> {
    inner: T,
    correlations: Arc<Mutex<CorrelationTracker>>,
    failure: TransportFailure,
    shutdown: CancellationToken,
    last_activity: Arc<AtomicU64>,
}

/// Wall-clock milliseconds for the idle watchdog's activity tracker. Wall clock rather
/// than a monotonic source because the value is shared across tasks as a plain atomic.
pub(super) fn unix_epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since_epoch| {
            u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX)
        })
}

#[derive(Default)]
struct CorrelationTracker {
    entries: HashMap<RequestId, String>,
    capacity_warning_emitted: bool,
    request_id_warning_emitted: bool,
}

impl CorrelationTracker {
    fn insert(&mut self, id: RequestId, correlation: String) -> bool {
        if matches!(&id, RequestId::String(id) if id.len() > MAX_CORRELATION_REQUEST_ID_BYTES) {
            if !self.request_id_warning_emitted {
                tracing::warn!(
                    target: "codexshim",
                    event = "tools_list_correlation_skipped",
                    phase = "transport",
                    outcome = "degraded",
                    reason = "request_id_too_long",
                    request_id_limit_bytes = MAX_CORRELATION_REQUEST_ID_BYTES
                );
                self.request_id_warning_emitted = true;
            }
            return false;
        }
        if !self.entries.contains_key(&id) && self.entries.len() >= MAX_TOOLS_LIST_CORRELATIONS {
            if !self.capacity_warning_emitted {
                tracing::warn!(
                    target: "codexshim",
                    event = "tools_list_correlation_skipped",
                    phase = "transport",
                    outcome = "degraded",
                    reason = "capacity",
                    correlation_limit = MAX_TOOLS_LIST_CORRELATIONS
                );
                self.capacity_warning_emitted = true;
            }
            return false;
        }
        self.entries.insert(id, correlation);
        true
    }

    fn remove(&mut self, id: &RequestId) -> Option<String> {
        self.entries.remove(id)
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

impl<T> DiagnosticTransport<T> {
    pub(super) fn new(
        inner: T,
        shutdown: CancellationToken,
        last_activity: Arc<AtomicU64>,
    ) -> (Self, TransportFailure) {
        let failure = TransportFailure(Arc::new(AtomicBool::new(false)));
        (
            Self {
                inner,
                correlations: Arc::new(Mutex::new(CorrelationTracker::default())),
                failure: failure.clone(),
                shutdown,
                last_activity,
            },
            failure,
        )
    }
}

impl<T> Transport<RoleServer> for DiagnosticTransport<T>
where
    T: Transport<RoleServer, Error = io::Error>,
{
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let (response_id, successful_response) = match &item {
            JsonRpcMessage::Response(response) => (Some(response.id.clone()), true),
            JsonRpcMessage::Error(error) => (error.id.clone(), false),
            _ => (None, false),
        };
        let correlation = response_id.as_ref().and_then(|id| {
            self.correlations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id)
        });
        let failure = self.failure.clone();
        let shutdown = self.shutdown.clone();
        let send = self.inner.send(item);
        async move {
            let result = send.await;
            match (&result, correlation) {
                (Ok(()), Some(request_id)) if successful_response => {
                    tracing::info!(target: "codexshim", event = "tools_list_sent", phase = "transport", outcome = "success", request_id);
                }
                (Err(error), Some(request_id)) => {
                    failure.0.store(true, Ordering::Release);
                    shutdown.cancel();
                    tracing::error!(target: "codexshim", event = "stdout_write_error", phase = "transport", outcome = "error", error_class = "io", io_kind = ?error.kind(), request_id);
                }
                (Err(error), None) => {
                    failure.0.store(true, Ordering::Release);
                    shutdown.cancel();
                    tracing::error!(target: "codexshim", event = "stdout_write_error", phase = "transport", outcome = "error", error_class = "io", io_kind = ?error.kind());
                }
                (Ok(()), _) => {}
            }
            result
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        let correlations = Arc::clone(&self.correlations);
        let last_activity = Arc::clone(&self.last_activity);
        let receive = self.inner.receive();
        async move {
            let mut message = receive.await;
            // Every inbound frame — request, notification, or response — proves the
            // client is still driving this server, which is all the idle watchdog
            // needs; `None` means the transport closed, which cancels the token anyway.
            if message.is_some() {
                last_activity.store(unix_epoch_millis(), Ordering::Release);
            }
            match &mut message {
                Some(JsonRpcMessage::Request(request))
                    if matches!(&request.request, ClientRequest::ListToolsRequest(_)) =>
                {
                    let correlation = Uuid::new_v4().to_string();
                    if correlations
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(request.id.clone(), correlation.clone())
                    {
                        request
                            .request
                            .extensions_mut()
                            .insert(ToolsListCorrelation(correlation));
                    }
                }
                Some(JsonRpcMessage::Notification(notification)) => {
                    if let ClientNotification::CancelledNotification(cancelled) =
                        &notification.notification
                        && let Some(id) = &cancelled.params.request_id
                    {
                        correlations
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(id);
                    }
                }
                _ => {}
            }
            message
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.correlations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.inner.close()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::ready,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    use rmcp::{
        RoleServer,
        model::{ClientRequest, JsonRpcMessage, RequestId},
        service::{RxJsonRpcMessage, TxJsonRpcMessage},
        transport::Transport,
    };

    use super::{
        CorrelationTracker, DiagnosticTransport, MAX_CORRELATION_REQUEST_ID_BYTES,
        MAX_TOOLS_LIST_CORRELATIONS,
    };

    struct TestTransport {
        received: VecDeque<RxJsonRpcMessage<RoleServer>>,
        fail_send: bool,
    }

    impl Transport<RoleServer> for TestTransport {
        type Error = std::io::Error;

        fn send(
            &mut self,
            _item: TxJsonRpcMessage<RoleServer>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let result = if self.fail_send {
                Err(std::io::Error::other("test send failure"))
            } else {
                Ok(())
            };
            ready(result)
        }

        fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
            ready(self.received.pop_front())
        }

        fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
            ready(Ok(()))
        }
    }

    fn request_id(value: i64) -> RequestId {
        RequestId::Number(value)
    }

    fn response(id: i64) -> TxJsonRpcMessage<RoleServer> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        }))
        .expect("response JSON")
    }

    fn error_response(id: i64) -> TxJsonRpcMessage<RoleServer> {
        serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32603,
                "message": "test error"
            }
        }))
        .expect("error response JSON")
    }

    fn inbound(value: serde_json::Value) -> RxJsonRpcMessage<RoleServer> {
        serde_json::from_value(value).expect("inbound JSON")
    }

    fn activity() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    #[test]
    fn tracker_accepts_numeric_and_bounded_string_ids() {
        let mut tracker = CorrelationTracker::default();

        assert!(tracker.insert(request_id(1), "numeric".to_owned()));
        assert!(tracker.insert(
            RequestId::String("s".repeat(MAX_CORRELATION_REQUEST_ID_BYTES).into()),
            "string".to_owned()
        ));

        assert_eq!(tracker.entries.len(), 2);
    }

    #[test]
    fn tracker_rejects_long_string_ids_and_caps_unique_entries() {
        let mut tracker = CorrelationTracker::default();
        let long_id = RequestId::String("s".repeat(MAX_CORRELATION_REQUEST_ID_BYTES + 1).into());

        assert!(!tracker.insert(long_id, "long".to_owned()));
        for id in 0..MAX_TOOLS_LIST_CORRELATIONS {
            assert!(tracker.insert(
                RequestId::Number(i64::try_from(id).expect("bounded ID")),
                id.to_string()
            ));
        }
        assert!(!tracker.insert(RequestId::Number(1_000), "overflow".to_owned()));

        assert_eq!(tracker.entries.len(), MAX_TOOLS_LIST_CORRELATIONS);
    }

    #[test]
    fn tracker_replaces_duplicate_ids_without_growing() {
        let mut tracker = CorrelationTracker::default();
        assert!(tracker.insert(request_id(1), "first".to_owned()));
        assert!(tracker.insert(request_id(1), "second".to_owned()));

        assert_eq!(tracker.entries.len(), 1);
        assert_eq!(tracker.remove(&request_id(1)).as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn cancellation_removes_only_the_referenced_correlation() {
        let received = VecDeque::from([
            inbound(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": 1, "reason": "test" }
            })),
            inbound(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": 999 }
            })),
            inbound(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {}
            })),
            inbound(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": "string-request" }
            })),
        ]);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (mut transport, _) = DiagnosticTransport::new(
            TestTransport {
                received,
                fail_send: false,
            },
            shutdown,
            activity(),
        );
        {
            let mut tracker = transport.correlations.lock().expect("tracker");
            tracker.insert(request_id(1), "first".to_owned());
            tracker.insert(request_id(2), "second".to_owned());
            tracker.insert(
                RequestId::String("string-request".into()),
                "string".to_owned(),
            );
        }

        transport.receive().await;
        transport.receive().await;
        transport.receive().await;
        transport.receive().await;

        let tracker = transport.correlations.lock().expect("tracker");
        assert!(!tracker.entries.contains_key(&request_id(1)));
        assert!(tracker.entries.contains_key(&request_id(2)));
        assert!(
            !tracker
                .entries
                .contains_key(&RequestId::String("string-request".into()))
        );
    }

    #[tokio::test]
    async fn tools_list_receive_injects_and_tracks_a_correlation() {
        let received = VecDeque::from([inbound(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/list",
            "params": {}
        }))]);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (mut transport, _) = DiagnosticTransport::new(
            TestTransport {
                received,
                fail_send: false,
            },
            shutdown,
            activity(),
        );

        let message = transport.receive().await.expect("tools/list request");

        let JsonRpcMessage::Request(request) = message else {
            panic!("expected request");
        };
        let ClientRequest::ListToolsRequest(list_tools) = request.request else {
            panic!("expected tools/list");
        };
        assert!(
            list_tools
                .extensions
                .get::<codexshim::ToolsListCorrelation>()
                .is_some()
        );
        assert!(
            transport
                .correlations
                .lock()
                .expect("tracker")
                .entries
                .contains_key(&request_id(7))
        );
    }

    #[tokio::test]
    async fn response_and_error_remove_correlations_before_send_completes() {
        for message in [response(1), error_response(1)] {
            let shutdown = tokio_util::sync::CancellationToken::new();
            let (mut transport, _) = DiagnosticTransport::new(
                TestTransport {
                    received: VecDeque::new(),
                    fail_send: false,
                },
                shutdown,
                activity(),
            );
            transport
                .correlations
                .lock()
                .expect("tracker")
                .insert(request_id(1), "correlation".to_owned());

            let send = transport.send(message);
            assert!(
                transport
                    .correlations
                    .lock()
                    .expect("tracker")
                    .entries
                    .is_empty()
            );
            send.await.expect("send");
        }
    }

    #[tokio::test]
    async fn failed_send_does_not_restore_correlation() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (mut transport, failure) = DiagnosticTransport::new(
            TestTransport {
                received: VecDeque::new(),
                fail_send: true,
            },
            shutdown.clone(),
            activity(),
        );
        transport
            .correlations
            .lock()
            .expect("tracker")
            .insert(request_id(1), "correlation".to_owned());

        transport.send(response(1)).await.expect_err("send failure");

        assert!(
            transport
                .correlations
                .lock()
                .expect("tracker")
                .entries
                .is_empty()
        );
        assert!(failure.failed());
        assert!(shutdown.is_cancelled());
    }

    #[tokio::test]
    async fn receive_updates_last_activity_for_every_inbound_message() {
        let received = VecDeque::from([inbound(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping",
            "params": {}
        }))]);
        let last_activity = activity();
        let (mut transport, _) = DiagnosticTransport::new(
            TestTransport {
                received,
                fail_send: false,
            },
            tokio_util::sync::CancellationToken::new(),
            Arc::clone(&last_activity),
        );

        assert!(transport.receive().await.is_some());
        assert!(last_activity.load(Ordering::Acquire) > 0);
    }
}
