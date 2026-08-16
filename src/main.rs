mod cli;

use std::{env, process::ExitCode, time::Duration};

use agentshim::{
    DiagnosticsConfig, DiagnosticsGuard, LogMode, MAX_READ_ONLY_CALLS, RuntimeLimits,
    bounded_diagnostic, capacity_bytes, purge, retention_days, status,
};
use cli::{CliCommand, parse_command, run, usage};
use tracing_subscriber::prelude::*;

fn main() -> ExitCode {
    let command = match parse_command(env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            usage();
            eprintln!("agentshim: {error}");
            return ExitCode::FAILURE;
        }
    };
    if matches!(command, CliCommand::LogsStatus | CliCommand::LogsPurge) {
        return run_logs_command(&command);
    }
    let _diagnostics = initialize_diagnostics();
    install_panic_hook();
    let config = match RuntimeLimits::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("agentshim: {error}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(target: "agentshim", event = "runtime_config", phase = "startup", counters = %format!("process_calls={},detached_calls={},read_only_calls={},worker_lanes={},blocking_threads={},grep_memory_bytes={},glob_memory_bytes={},memory_bytes={}", config.process_calls, config.detached_calls, MAX_READ_ONLY_CALLS, config.worker_lanes, config.blocking_threads, config.grep_memory_bytes, config.glob_memory_bytes, config.memory_bytes));
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.scheduler_threads)
        .max_blocking_threads(config.blocking_threads)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("agentshim: failed to build runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(target: "agentshim", event = "runtime_ready", phase = "startup");
    let outcome = runtime.block_on(run(config, command));
    // `tokio::io::stdin` uses an uncancellable blocking read. Once the service has
    // completed its own bounded shutdown, do not let that helper keep the process alive
    // solely because the client retained its stdin pipe.
    runtime.shutdown_timeout(Duration::ZERO);
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agentshim: {error}");
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
            tracing::info!(target: "agentshim", event = "diagnostics_start", phase = "startup");
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
        bounded_diagnostic(&format!("agentshim diagnostics disabled: {error}"))
    );
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        tracing::error!(target: "agentshim", event = "panic", phase = "lifecycle", error_class = "worker_panic");
        previous(panic);
    }));
}

fn run_logs_command(command: &CliCommand) -> ExitCode {
    let config = match DiagnosticsConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("agentshim: {error}");
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
            println!("recorded dropped records: {}", report.dropped);
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
            eprintln!("agentshim: {error}");
            ExitCode::FAILURE
        }
    }
}
