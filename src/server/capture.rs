use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rmcp::{
    RoleServer,
    model::{ClientResult, CustomRequest, ServerRequest},
    service::{Peer, PeerRequestOptions},
};
use serde_json::{Value, json};

pub(crate) use agentshim_core::dsh_bridge::DshCaptureRequest;

use crate::tools::exec::{CaptureFailureKind, CaptureTransportError, spawn::CaptureSink};

const CAPTURE_ACK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct RemoteCaptureSink {
    peer: Peer<RoleServer>,
    capture_id: Arc<str>,
    streams: Arc<[String]>,
    offsets: Arc<Mutex<Vec<u64>>>,
    runtime: tokio::runtime::Handle,
}

impl RemoteCaptureSink {
    pub(crate) fn new(peer: Peer<RoleServer>, request: &DshCaptureRequest) -> Self {
        Self {
            peer,
            capture_id: Arc::from(request.id.as_str()),
            streams: Arc::from(request.streams.clone()),
            offsets: Arc::new(Mutex::new(vec![0; request.streams.len()])),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub(crate) fn totals(&self) -> Vec<u64> {
        self.offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn complete(&self, complete: bool, error: Option<&str>) -> Result<(), io::Error> {
        let totals = self
            .streams
            .iter()
            .cloned()
            .zip(self.totals())
            .map(|(name, total)| (name, Value::from(total)))
            .collect::<serde_json::Map<String, Value>>();
        let params = json!({
            "bridgeVersion": super::DSH_BRIDGE_VERSION,
            "captureId": self.capture_id,
            "complete": complete,
            "totals": totals,
            "error": error,
        });
        self.send("agentshim/dsh.capture.complete", params)
            .map(|_| ())
    }

    fn send(&self, method: &str, params: Value) -> Result<Value, io::Error> {
        let request = ServerRequest::CustomRequest(CustomRequest::new(method, Some(params)));
        let peer = self.peer.clone();
        let result = self.runtime.block_on(async move {
            let mut options = PeerRequestOptions::no_options();
            options.timeout = Some(CAPTURE_ACK_TIMEOUT);
            options.max_total_timeout = Some(CAPTURE_ACK_TIMEOUT);
            let handle = peer.send_request_with_option(request, options).await?;
            handle.await_response().await
        });
        match result {
            Ok(ClientResult::CustomResult(result)) => Ok(result.0),
            Ok(_) => Err(capture_error(
                CaptureFailureKind::Protocol,
                "capture client returned a non-custom result",
            )),
            Err(error) => {
                let message = error.to_string();
                let kind = if message.contains("AGENTSHIM_CAPTURE_LIMIT_EXCEEDED")
                    || message.contains("exceeded")
                {
                    CaptureFailureKind::LimitExceeded
                } else if message.contains("AGENTSHIM_CAPTURE_IO_FAILED")
                    || message.contains("write failed")
                {
                    CaptureFailureKind::Io
                } else {
                    CaptureFailureKind::Protocol
                };
                Err(capture_error(kind, message))
            }
        }
    }
}

impl CaptureSink for RemoteCaptureSink {
    fn append(&self, stream: usize, bytes: &[u8]) -> io::Result<()> {
        let name = self.streams.get(stream).ok_or_else(|| {
            capture_error(
                CaptureFailureKind::Protocol,
                "capture stream index is out of range",
            )
        })?;
        let offset = {
            let offsets = self
                .offsets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            offsets[stream]
        };
        let params = json!({
            "bridgeVersion": super::DSH_BRIDGE_VERSION,
            "captureId": self.capture_id,
            "stream": name,
            "offset": offset,
            "data": STANDARD.encode(bytes),
        });
        let result = self.send("agentshim/dsh.capture.append", params)?;
        let next = result
            .get("nextOffset")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                capture_error(
                    CaptureFailureKind::Protocol,
                    "capture ACK omitted nextOffset",
                )
            })?;
        let expected = offset.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if next != expected {
            return Err(capture_error(
                CaptureFailureKind::Protocol,
                format!("capture ACK expected {expected}, received {next}"),
            ));
        }
        let mut offsets = self
            .offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if offsets[stream] != offset {
            return Err(capture_error(
                CaptureFailureKind::Protocol,
                "capture stream received concurrent out-of-order ACKs",
            ));
        }
        offsets[stream] = next;
        Ok(())
    }

    fn complete(&self, complete: bool, error: Option<&str>) -> io::Result<()> {
        RemoteCaptureSink::complete(self, complete, error)
    }
}

fn capture_error(kind: CaptureFailureKind, message: impl Into<String>) -> io::Error {
    let message = message.into();
    io::Error::other(CaptureTransportError { kind, message })
}
