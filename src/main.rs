use std::{
    env,
    error::Error,
    ffi::OsString,
    pin::Pin,
    process::ExitCode,
    task::{Context, Poll},
};

use codexshim::{
    diagnostics::{
        DiagnosticsConfig, DiagnosticsGuard, capacity_bytes, purge, retention_days, status,
    },
    output::bounded_diagnostic,
    path::ReadScope,
    runtime::{MAX_PROCESS_CALLS, MAX_READ_ONLY_CALLS, RuntimeConfig, RuntimeResources},
    server::CodexShim,
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

fn usage() {
    eprintln!(
        "Usage: codexshim <serve|doctor> [--read-scope <normal|unrestricted>] | logs <status|purge> | --version"
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CliCommand {
    Serve(ReadScope),
    Doctor(ReadScope),
    LogsStatus,
    LogsPurge,
    Version,
}

fn parse_command(args: impl IntoIterator<Item = OsString>) -> Result<CliCommand, String> {
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
    while let Some(argument) = args.next() {
        let argument = argument
            .to_str()
            .ok_or_else(|| "arguments must be valid Unicode".to_owned())?;
        let value = if argument == "--read-scope" {
            args.next()
                .ok_or_else(|| "--read-scope requires a value".to_owned())?
                .into_string()
                .map_err(|_| "read scope must be valid Unicode".to_owned())?
        } else if let Some(value) = argument.strip_prefix("--read-scope=") {
            if value.is_empty() {
                return Err("--read-scope requires a value".to_owned());
            }
            value.to_owned()
        } else {
            return Err(format!("unknown argument: {argument}"));
        };
        if read_scope.is_some() {
            return Err("--read-scope may be specified only once".to_owned());
        }
        read_scope = Some(
            value
                .parse::<ReadScope>()
                .map_err(|error| error.to_string())?,
        );
    }

    let read_scope = read_scope.unwrap_or_default();
    match kind {
        "serve" => Ok(CliCommand::Serve(read_scope)),
        "doctor" => Ok(CliCommand::Doctor(read_scope)),
        _ => unreachable!("command was validated"),
    }
}

async fn run(config: RuntimeConfig, command: CliCommand) -> Result<(), Box<dyn Error>> {
    match command {
        CliCommand::Serve(read_scope) => {
            let resources = RuntimeResources::new(config);
            let service = CodexShim::from_current_dir_with_resources_and_scope(
                resources.clone(),
                read_scope,
            )?;
            let (stdin, stdout) = stdio();
            let reader = ShutdownReader {
                inner: stdin,
                shutdown: resources.shutdown_token(),
                termination_reported: false,
            };
            tracing::info!(target: "codexshim", event = "server_start", phase = "lifecycle", read_scope = %read_scope);
            let running = service.serve((reader, DiagnosticWriter(stdout))).await?;
            tracing::info!(target: "codexshim", event = "server_ready", phase = "lifecycle");
            running.waiting().await?;
            tracing::info!(target: "codexshim", event = "server_stop", phase = "lifecycle");
        }
        CliCommand::Doctor(read_scope) => {
            let service = CodexShim::from_current_dir_with_resources_and_scope(
                RuntimeResources::new(config),
                read_scope,
            )?;
            service.verify_root()?;
            service.verify_process_runtime()?;
            println!("codexshim doctor: ok");
            println!("root: {}", service.root_path().display());
            println!("protocol: 2026-07-28");
            println!(
                "protocol compatibility: {}",
                service.protocol_compatibility()
            );
            println!("read scope: {}", service.read_scope());
            println!("read-only calls: {MAX_READ_ONLY_CALLS}");
            println!("process calls: {MAX_PROCESS_CALLS}");
            println!("process lifecycle: ok");
            println!(
                "worker lanes: {}",
                service.resources().config().worker_lanes
            );
        }
        CliCommand::Version => {
            println!("codexshim {}", env!("CARGO_PKG_VERSION"));
        }
        CliCommand::LogsStatus | CliCommand::LogsPurge => {
            unreachable!("log management commands do not require a Tokio runtime")
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let command = match parse_command(env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            usage();
            eprintln!("codexshim: {error}");
            return ExitCode::FAILURE;
        }
    };
    if matches!(command, CliCommand::LogsStatus | CliCommand::LogsPurge) {
        return run_logs_command(command);
    }
    let _diagnostics = initialize_diagnostics();
    install_panic_hook();
    let config = match RuntimeConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("codexshim: {error}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(target: "codexshim", event = "runtime_config", phase = "startup", counters = %format!("worker_lanes={},blocking_threads={}", config.worker_lanes, config.blocking_threads));
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.scheduler_threads)
        .max_blocking_threads(config.blocking_threads)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("codexshim: failed to build runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(target: "codexshim", event = "runtime_ready", phase = "startup");
    match runtime.block_on(run(config, command)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("codexshim: {error}");
            ExitCode::FAILURE
        }
    }
}

fn initialize_diagnostics() -> DiagnosticsGuard {
    let config = match DiagnosticsConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            diagnostics_warning(&error);
            return DiagnosticsGuard::disabled(std::path::PathBuf::default());
        }
    };
    let directory = config.directory.clone();
    if config.mode == codexshim::diagnostics::LogMode::Off {
        return DiagnosticsGuard::disabled(directory);
    }
    match DiagnosticsGuard::start(config) {
        Ok((guard, Some(layer))) => {
            if let Err(error) =
                tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
            {
                diagnostics_warning(&error);
                return DiagnosticsGuard::disabled(directory);
            }
            tracing::info!(target: "codexshim", event = "diagnostics_start", phase = "startup");
            guard
        }
        Ok((guard, None)) => guard,
        Err(error) => {
            diagnostics_warning(&error);
            DiagnosticsGuard::disabled(directory)
        }
    }
}

fn diagnostics_warning(error: &dyn std::fmt::Display) {
    eprintln!(
        "{}",
        bounded_diagnostic(&format!("codexshim diagnostics disabled: {error}"))
    );
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        tracing::error!(target: "codexshim", event = "panic", phase = "lifecycle", error_class = "worker_panic");
        previous(panic);
    }));
}

fn run_logs_command(command: CliCommand) -> ExitCode {
    let config = match DiagnosticsConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("codexshim: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = match command {
        CliCommand::LogsStatus => status(&config).map(|report| {
            println!("mode: {}", report.mode);
            println!("directory: {}", report.directory.display());
            println!("JSONL files: {}", report.files);
            println!("total bytes: {}", report.bytes);
            println!(
                "date range: {}",
                match (report.oldest, report.newest) {
                    (Some(oldest), Some(newest)) => format!("{oldest} to {newest}"),
                    _ => "none".to_owned(),
                }
            );
            println!("retention days: {}", retention_days());
            println!("capacity bytes: {}", capacity_bytes());
            println!("recorded dropped batches: {}", report.dropped);
        }),
        CliCommand::LogsPurge => purge(&config).map(|report| {
            println!("deleted files: {}", report.files);
            println!("deleted bytes: {}", report.bytes);
        }),
        _ => unreachable!("validated logs command"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("codexshim: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CliCommand, parse_command};
    use codexshim::path::ReadScope;

    fn parse(args: &[&str]) -> Result<CliCommand, String> {
        parse_command(args.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn read_scope_defaults_and_accepts_both_argument_forms() {
        assert_eq!(parse(&["serve"]), Ok(CliCommand::Serve(ReadScope::Normal)));
        assert_eq!(
            parse(&["serve", "--read-scope", "normal"]),
            Ok(CliCommand::Serve(ReadScope::Normal))
        );
        assert_eq!(
            parse(&["serve", "--read-scope", "unrestricted"]),
            Ok(CliCommand::Serve(ReadScope::Unrestricted))
        );
        assert_eq!(
            parse(&["doctor", "--read-scope=normal"]),
            Ok(CliCommand::Doctor(ReadScope::Normal))
        );
    }

    #[test]
    fn read_scope_rejects_incomplete_duplicate_and_unknown_arguments() {
        for args in [
            &["serve", "--read-scope"][..],
            &["serve", "--read-scope="][..],
            &["serve", "--read-scope", "unknown"][..],
            &[
                "serve",
                "--read-scope",
                "normal",
                "--read-scope=unrestricted",
            ][..],
            &["serve", "--unknown"][..],
            &["--version", "extra"][..],
        ] {
            assert!(parse(args).is_err(), "unexpectedly accepted {args:?}");
        }
    }

    #[test]
    fn parses_log_management_commands() {
        assert_eq!(parse(&["logs", "status"]), Ok(CliCommand::LogsStatus));
        assert_eq!(parse(&["logs", "purge"]), Ok(CliCommand::LogsPurge));
        assert!(parse(&["logs"]).is_err());
        assert!(parse(&["logs", "clear"]).is_err());
    }
}
