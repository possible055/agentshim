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

async fn run(config: RuntimeLimits, command: CliCommand) -> Result<(), Box<dyn Error>> {
    match command {
        CliCommand::Serve(read_scope) => {
            let service = CodexShim::builder(std::env::current_dir()?)?
                .runtime_limits(config)
                .read_scope(read_scope)
                .build()?;
            let (stdin, stdout) = stdio();
            let reader = ShutdownReader {
                inner: ReceiveFrameReader::new(stdin),
                shutdown: service.shutdown_token(),
                termination_reported: false,
            };
            tracing::info!(target: "codexshim", event = "server_start", phase = "lifecycle", read_scope = %read_scope);
            let running = service.serve((reader, DiagnosticWriter(stdout))).await?;
            tracing::info!(target: "codexshim", event = "server_ready", phase = "lifecycle");
            running.waiting().await?;
            tracing::info!(target: "codexshim", event = "server_stop", phase = "lifecycle");
        }
        CliCommand::Doctor(read_scope) => {
            let service = CodexShim::builder(std::env::current_dir()?)?
                .runtime_limits(config)
                .read_scope(read_scope)
                .build()?;
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
                service.runtime_limits().worker_lanes
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
