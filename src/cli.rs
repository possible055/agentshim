fn usage() {
    eprintln!(
        "Usage: codexshim <serve|doctor> [--read-scope <normal|unrestricted>] [--allow-programs <comma-separated>] | logs <status|purge> | --version"
    );
}

#[derive(Clone, Debug, PartialEq)]
struct ServeOptions {
    read_scope: ReadScope,
    allowed_programs: AllowedPrograms,
}

#[derive(Clone, Debug, PartialEq)]
enum CliCommand {
    Serve(ServeOptions),
    Doctor(ServeOptions),
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
    let mut allow_programs = None;
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
            "--allow-programs" => {
                if allow_programs.is_some() {
                    return Err("--allow-programs may be specified only once".to_owned());
                }
                allow_programs = Some(
                    AllowedPrograms::parse(&value).map_err(|error| error.to_string())?,
                );
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }

    // The startup flag wins over the environment so a deployment can pin the allowlist even
    // when an inherited variable disagrees.
    let allowed_programs = match allow_programs {
        Some(allowed) => allowed,
        None => AllowedPrograms::from_env().map_err(|error| error.to_string())?,
    };
    let options = ServeOptions {
        read_scope: read_scope.unwrap_or_default(),
        allowed_programs,
    };
    match kind {
        "serve" => Ok(CliCommand::Serve(options)),
        "doctor" => Ok(CliCommand::Doctor(options)),
        _ => unreachable!("command was validated"),
    }
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

async fn run(config: RuntimeLimits, command: CliCommand) -> Result<(), Box<dyn Error>> {
    match command {
        CliCommand::Serve(options) => {
            let read_scope = options.read_scope;
            let service = CodexShim::builder(std::env::current_dir()?)?
                .runtime_limits(config)
                .read_scope(read_scope)
                .allowed_programs(options.allowed_programs)
                .build()?;
            let (stdin, stdout) = stdio();
            let reader = ShutdownReader {
                inner: ReceiveFrameReader::new(stdin),
                shutdown: service.shutdown_token(),
                termination_reported: false,
            };
            tracing::info!(target: "codexshim", event = "server_start", phase = "lifecycle", read_scope = %read_scope);
            let shutdown = service.clone();
            let running = service.serve((reader, DiagnosticWriter(stdout))).await?;
            tracing::info!(target: "codexshim", event = "server_ready", phase = "lifecycle");
            let outcome = running.waiting().await;
            shutdown.terminate_detached();
            outcome?;
            tracing::info!(target: "codexshim", event = "server_stop", phase = "lifecycle");
        }
        CliCommand::Doctor(options) => {
            let allowed = options.allowed_programs.clone();
            let service = CodexShim::builder(std::env::current_dir()?)?
                .runtime_limits(config)
                .read_scope(options.read_scope)
                .allowed_programs(options.allowed_programs)
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
            println!(
                "process calls: {}",
                service.runtime_limits().process_calls
            );
            println!(
                "detached calls: {}",
                service.runtime_limits().detached_calls
            );
            println!("output bytes: {}", service.runtime_limits().output_bytes);
            println!(
                "grep memory bytes: {}",
                service.runtime_limits().grep_memory_bytes
            );
            println!(
                "glob memory bytes: {}",
                service.runtime_limits().glob_memory_bytes
            );
            println!(
                "global memory bytes: {}",
                service.runtime_limits().memory_bytes
            );
            println!("allowed programs: {}", allowed.describe());
            match bash_report() {
                Ok((executable, locale)) => {
                    println!("bash: {}", executable.display());
                    println!("bash locale: {locale}");
                }
                Err(error) => println!("bash: unavailable ({error})"),
            }
            println!("process lifecycle: ok");
            println!(
                "worker lanes: {}",
                service.runtime_limits().worker_lanes
            );
            println!(
                "blocking threads: {}",
                service.runtime_limits().blocking_threads
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
