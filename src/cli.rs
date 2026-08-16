use std::{
    error::Error,
    ffi::OsString,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use codexshim::{
    ClientProfile, CodexShim, MAX_READ_ONLY_CALLS, ReadScope, RuntimeLimits, bash_report,
};
use rmcp::{
    ServiceExt,
    service::{QuitReason, ServerInitializeError},
    transport::{IntoTransport, stdio},
};

use self::transport::{DiagnosticTransport, ReceiveFrameReader, ShutdownReader, unix_epoch_millis};

mod transport;

#[cfg(test)]
mod tests;

pub(super) fn usage() {
    eprintln!(
        "Usage: codexshim <serve|doctor> [--read-scope <normal|unrestricted>] \
         [--client-profile <codex|cursor>] | logs <status|purge> | --version"
    );
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ServeOptions {
    read_scope: ReadScope,
    client_profile: ClientProfile,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum CliCommand {
    Serve(ServeOptions),
    Doctor(ServeOptions),
    LogsStatus,
    LogsPurge,
    Version,
}

pub(super) fn parse_command(
    args: impl IntoIterator<Item = OsString>,
) -> Result<CliCommand, String> {
    let mut args = args.into_iter();
    let command = args.next().ok_or_else(|| "missing command".to_owned())?;
    if command == "--version" || command == "-V" {
        if args.next().is_some() {
            return Err("--version does not accept arguments".to_owned());
        }
        return Ok(CliCommand::Version);
    }
    let kind = command
        .to_str()
        .ok_or_else(|| "command must be valid Unicode".to_owned())?;
    if kind == "logs" {
        let action = args
            .next()
            .ok_or_else(|| "logs requires `status` or `purge`".to_owned())?;
        if args.next().is_some() {
            return Err("logs accepts exactly one action".to_owned());
        }
        return match action.to_str() {
            Some("status") => Ok(CliCommand::LogsStatus),
            Some("purge") => Ok(CliCommand::LogsPurge),
            Some(action) => Err(format!("unknown logs action: {action}")),
            None => Err("logs action must be valid Unicode".to_owned()),
        };
    }
    if !matches!(kind, "serve" | "doctor") {
        return Err(format!("unknown command: {kind}"));
    }

    let mut read_scope = None;
    let mut client_profile = None;
    while let Some(argument) = args.next() {
        let argument = argument
            .to_str()
            .ok_or_else(|| "arguments must be valid Unicode".to_owned())?;
        let (flag, value) = flag_value(argument, &mut args)?;
        match flag {
            "--read-scope" => {
                if read_scope.is_some() {
                    return Err("--read-scope may be specified only once".to_owned());
                }
                read_scope = Some(
                    value
                        .parse::<ReadScope>()
                        .map_err(|error| error.to_string())?,
                );
            }
            "--client-profile" => {
                if client_profile.is_some() {
                    return Err("--client-profile may be specified only once".to_owned());
                }
                client_profile = Some(
                    value
                        .parse::<ClientProfile>()
                        .map_err(|error| error.to_string())?,
                );
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }

    let options = ServeOptions {
        read_scope: read_scope.unwrap_or_default(),
        client_profile: client_profile.unwrap_or_default(),
    };
    match kind {
        "serve" => Ok(CliCommand::Serve(options)),
        "doctor" => Ok(CliCommand::Doctor(options)),
        _ => unreachable!("command was validated"),
    }
}

/// Reports every per-tool reservation plus the pool they share.
///
/// The PDF reservations are also the ceilings the parser is held to, so the derived
/// per-page span limits are reported beside them: they are the number that decides
/// whether an unusually dense page is delivered or reported as unavailable, and it is
/// not recoverable from the byte figures without knowing the derivation.
fn print_memory_limits(limits: RuntimeLimits) {
    println!("grep memory bytes: {}", limits.grep_memory_bytes);
    println!("glob memory bytes: {}", limits.glob_memory_bytes);
    println!(
        "search heap charges against the shared memory pool; capture overflow skips or truncates that file"
    );
    println!("pdf text memory bytes: {}", limits.pdf_text_memory_bytes);
    println!("pdf image memory bytes: {}", limits.pdf_image_memory_bytes);
    println!(
        "pdf text page spans: {}",
        codexshim_pdf_read::PdfResourceLimits::text_within(limits.pdf_text_memory_bytes).page_spans
    );
    println!(
        "pdf image page spans: {}",
        codexshim_pdf_read::PdfResourceLimits::image_within(limits.pdf_image_memory_bytes)
            .page_spans
    );
    println!("global memory bytes: {}", limits.memory_bytes);
}

fn flag_value<'a>(
    argument: &'a str,
    args: &mut impl Iterator<Item = OsString>,
) -> Result<(&'a str, String), String> {
    if let Some((flag, value)) = argument.split_once('=') {
        if value.is_empty() {
            return Err(format!("{flag} requires a value"));
        }
        return Ok((flag, value.to_owned()));
    }
    let value = args
        .next()
        .ok_or_else(|| format!("{argument} requires a value"))?
        .into_string()
        .map_err(|_| format!("{argument} value must be valid Unicode"))?;
    Ok((argument, value))
}

pub(super) async fn run(config: RuntimeLimits, command: CliCommand) -> Result<(), Box<dyn Error>> {
    match command {
        CliCommand::Serve(options) => {
            let read_scope = options.read_scope;
            let client_profile = options.client_profile;
            let service = CodexShim::builder(std::env::current_dir()?)?
                .runtime_limits(config)
                .read_scope(read_scope)
                .client_profile(client_profile)
                .build()?;
            let (stdin, stdout) = stdio();
            let reader = ShutdownReader {
                inner: ReceiveFrameReader::new(stdin),
                shutdown: service.shutdown_token(),
                termination_reported: false,
            };
            tracing::info!(target: "codexshim", event = "server_start", phase = "lifecycle", read_scope = %read_scope, client_profile = %service.client_profile(), tool_output_tokens = service.tool_output_token_limit(), burst_tokens = service.burst_token_limit(), idle_timeout_secs = service.runtime_limits().idle_timeout.map(|timeout| timeout.as_secs()));
            let shutdown_token = service.shutdown_token();
            let transport = (reader, stdout).into_transport();
            // Seed before transport polling starts so an instance whose client never
            // sends its first frame is still reclaimed after the configured timeout.
            let last_activity = Arc::new(AtomicU64::new(unix_epoch_millis()));
            let (transport, transport_failure) = DiagnosticTransport::new(
                transport,
                shutdown_token.clone(),
                Arc::clone(&last_activity),
            );
            // Process shutdown begins the moment the global token fires — EOF, transport
            // failure, or explicit stop — and runs in parallel with the protocol drain,
            // instead of waiting for the service to finish first.
            let shutdown_watcher = {
                let shutdown_service = service.clone();
                let watcher_token = shutdown_token.clone();
                tokio::spawn(async move {
                    watcher_token.cancelled().await;
                    shutdown_service.shutdown_processes().await;
                })
            };
            let idle_watchdog = spawn_idle_watchdog(
                service.clone(),
                Arc::clone(&last_activity),
                shutdown_token.clone(),
            );
            let drain_service = service.clone();
            let running = match service.serve_with_ct(transport, shutdown_token).await {
                Ok(running) => running,
                Err(error) => {
                    drain_service.shutdown_processes().await;
                    let _ = shutdown_watcher.await;
                    let _ = idle_watchdog.await;
                    tracing::error!(target: "codexshim", event = "server_stop", phase = "lifecycle", outcome = "error", error_class = initialize_error_class(&error));
                    return Err(error.into());
                }
            };
            tracing::info!(target: "codexshim", event = "server_ready", phase = "lifecycle");
            let outcome = running.waiting().await;
            drain_service.shutdown_processes().await;
            let _ = shutdown_watcher.await;
            let _ = idle_watchdog.await;
            match outcome {
                Ok(QuitReason::Closed) if transport_failure.failed() => {
                    tracing::error!(target: "codexshim", event = "server_stop", phase = "lifecycle", outcome = "error", error_class = "transport");
                    return Err("MCP transport failed".into());
                }
                Ok(QuitReason::Closed) => {
                    tracing::info!(target: "codexshim", event = "server_stop", phase = "lifecycle", outcome = "success", reason = "transport_closed");
                }
                Ok(QuitReason::Cancelled) if transport_failure.failed() => {
                    tracing::error!(target: "codexshim", event = "server_stop", phase = "lifecycle", outcome = "error", error_class = "transport");
                    return Err("MCP transport failed".into());
                }
                Ok(QuitReason::Cancelled) => {
                    tracing::info!(target: "codexshim", event = "server_stop", phase = "lifecycle", outcome = "success", reason = "shutdown");
                }
                Ok(QuitReason::JoinError(error)) | Err(error) => {
                    tracing::error!(target: "codexshim", event = "server_stop", phase = "lifecycle", outcome = "error", error_class = "framework");
                    return Err(error.into());
                }
                Ok(_) => {
                    tracing::error!(target: "codexshim", event = "server_stop", phase = "lifecycle", outcome = "error", error_class = "framework");
                    return Err("MCP server stopped for an unknown reason".into());
                }
            }
        }
        CliCommand::Doctor(options) => run_doctor(config, &options)?,
        CliCommand::Version => {
            println!("codexshim {}", env!("CARGO_PKG_VERSION"));
        }
        CliCommand::LogsStatus | CliCommand::LogsPurge => {
            unreachable!("log management commands do not require a Tokio runtime")
        }
    }
    Ok(())
}

fn spawn_idle_watchdog(
    service: CodexShim,
    last_activity: Arc<AtomicU64>,
    shutdown: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Some(timeout) = service.runtime_limits().idle_timeout else {
            return;
        };
        let recheck = timeout.min(Duration::from_secs(30));
        loop {
            let last = last_activity.load(Ordering::Acquire);
            let elapsed = Duration::from_millis(unix_epoch_millis().saturating_sub(last));
            let Some(remaining) = timeout.checked_sub(elapsed) else {
                if !service.is_idle_quiescent() {
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        () = tokio::time::sleep(recheck) => {}
                    }
                    continue;
                }
                // Re-read after the quiescence probe so a message that raced the
                // deadline gets its stay of execution. A frame landing between this
                // load and cancellation is the unavoidable idle-timeout boundary race.
                if last != last_activity.load(Ordering::Acquire) {
                    continue;
                }
                tracing::info!(target: "codexshim", event = "idle_shutdown", phase = "lifecycle", idle_secs = timeout.as_secs());
                shutdown.cancel();
                return;
            };
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(remaining) => {}
            }
        }
    })
}

fn run_doctor(config: RuntimeLimits, options: &ServeOptions) -> Result<(), Box<dyn Error>> {
    let service = CodexShim::builder(std::env::current_dir()?)?
        .runtime_limits(config)
        .read_scope(options.read_scope)
        .client_profile(options.client_profile)
        .build()?;
    service.verify_root()?;
    service.verify_process_runtime()?;
    println!("codexshim doctor: ok");
    println!("root: {}", service.root_path().display());
    println!("protocol: 2026-07-28");
    println!("read scope: {}", service.read_scope());
    println!("read-only calls: {MAX_READ_ONLY_CALLS}");
    println!("process calls: {}", service.runtime_limits().process_calls);
    println!(
        "detached calls: {}",
        service.runtime_limits().detached_calls
    );
    println!("output bytes: {}", service.runtime_limits().output_bytes);
    println!(
        "respect gitignore: {}",
        service.runtime_limits().respect_gitignore
    );
    println!("client profile: {}", service.client_profile());
    println!("tool output tokens: {}", service.tool_output_token_limit());
    println!("burst tokens: {}", service.burst_token_limit());
    match service.runtime_limits().idle_timeout {
        Some(timeout) => println!("idle timeout: {}s", timeout.as_secs()),
        None => println!("idle timeout: disabled"),
    }
    print_memory_limits(service.runtime_limits());
    match bash_report() {
        Ok((executable, locale)) => {
            println!("bash: {}", executable.display());
            println!("bash locale: {locale}");
        }
        Err(error) => println!("bash: unavailable ({error})"),
    }
    println!("process lifecycle: ok");
    println!("worker lanes: {}", service.runtime_limits().worker_lanes);
    println!(
        "blocking threads: {}",
        service.runtime_limits().blocking_threads
    );
    Ok(())
}

fn initialize_error_class(error: &ServerInitializeError) -> &'static str {
    match error {
        ServerInitializeError::ExpectedInitializeRequest(_)
        | ServerInitializeError::UnexpectedInitializeResponse(_)
        | ServerInitializeError::InitializeFailed(_) => "protocol",
        ServerInitializeError::ConnectionClosed(_)
        | ServerInitializeError::TransportError { .. } => "transport",
        ServerInitializeError::Cancelled => "cancelled",
        _ => "server_initialize",
    }
}
