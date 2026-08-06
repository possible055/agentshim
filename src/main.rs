include!("cli/transport.rs");
include!("cli.rs");

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
    let config = match RuntimeLimits::from_env() {
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
    if config.mode == LogMode::Off {
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

include!("cli/tests.rs");
