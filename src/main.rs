use std::{
    env,
    error::Error,
    ffi::OsString,
    pin::Pin,
    process::ExitCode,
    task::{Context, Poll},
};

use codexshim::{
    path::ReadScope,
    runtime::{MAX_PROCESS_CALLS, MAX_READ_ONLY_CALLS, RuntimeConfig, RuntimeResources},
    server::CodexShim,
};
use rmcp::{ServiceExt, transport::stdio};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::sync::CancellationToken;

struct ShutdownReader<R> {
    inner: R,
    shutdown: CancellationToken,
}

impl<R: AsyncRead + Unpin> AsyncRead for ShutdownReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if matches!(result, Poll::Ready(Err(_)))
            || matches!(result, Poll::Ready(Ok(()))) && buffer.filled().len() == before
        {
            self.shutdown.cancel();
        }
        result
    }
}

fn usage() {
    eprintln!(
        "Usage: codexshim <serve|doctor> [--read-scope <repository|unrestricted>] | --version"
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CliCommand {
    Serve(ReadScope),
    Doctor(ReadScope),
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
            };
            let running = service.serve((reader, stdout)).await?;
            running.waiting().await?;
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
    let config = match RuntimeConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("codexshim: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.worker_lanes)
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
    match runtime.block_on(run(config, command)) {
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
        assert_eq!(
            parse(&["serve"]),
            Ok(CliCommand::Serve(ReadScope::Repository))
        );
        assert_eq!(
            parse(&["serve", "--read-scope", "unrestricted"]),
            Ok(CliCommand::Serve(ReadScope::Unrestricted))
        );
        assert_eq!(
            parse(&["doctor", "--read-scope=repository"]),
            Ok(CliCommand::Doctor(ReadScope::Repository))
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
                "repository",
                "--read-scope=unrestricted",
            ][..],
            &["serve", "--unknown"][..],
            &["--version", "extra"][..],
        ] {
            assert!(parse(args).is_err(), "unexpectedly accepted {args:?}");
        }
    }
}
