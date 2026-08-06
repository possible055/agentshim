use std::{
    env,
    error::Error,
    ffi::OsString,
    pin::Pin,
    process::ExitCode,
    task::{Context, Poll},
};

use codexshim::{
    CodexShim, DiagnosticsConfig, DiagnosticsGuard, LogMode, MAX_PROCESS_CALLS,
    MAX_READ_ONLY_CALLS, ReadScope, RuntimeLimits, bounded_diagnostic, capacity_bytes, purge,
    retention_days, status,
};
use rmcp::{ServiceExt, transport::stdio};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::prelude::*;

struct ShutdownReader<R> {
    inner: R,
    shutdown: CancellationToken,
    termination_reported: bool,
}

impl<R: AsyncRead + Unpin> AsyncRead for ShutdownReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
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

struct DiagnosticWriter<W>(W);

impl<W: AsyncWrite + Unpin> AsyncWrite for DiagnosticWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.0).poll_write(context, buffer);
        if let Poll::Ready(Err(error)) = &result {
            tracing::error!(target: "codexshim", event = "stdout_write_error", phase = "transport", error_class = "io", io_kind = ?error.kind());
        }
        result
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let result = Pin::new(&mut self.0).poll_flush(context);
        if let Poll::Ready(Err(error)) = &result {
            tracing::error!(target: "codexshim", event = "stdout_write_error", phase = "transport", error_class = "io", io_kind = ?error.kind());
        }
        result
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let result = Pin::new(&mut self.0).poll_shutdown(context);
        if let Poll::Ready(Err(error)) = &result {
            tracing::error!(target: "codexshim", event = "stdout_write_error", phase = "transport", error_class = "io", io_kind = ?error.kind());
        }
        result
    }
}
