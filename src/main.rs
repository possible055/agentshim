use std::{
    env,
    error::Error,
    pin::Pin,
    process::ExitCode,
    task::{Context, Poll},
};

use codexshim::{
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
    eprintln!("Usage: codexshim <serve|doctor|--version>");
}

async fn run(config: RuntimeConfig) -> Result<(), Box<dyn Error>> {
    match env::args_os().nth(1).as_deref() {
        Some(command) if command == "serve" => {
            let resources = RuntimeResources::new(config);
            let service = CodexShim::from_current_dir_with_resources(resources.clone())?;
            let (stdin, stdout) = stdio();
            let reader = ShutdownReader {
                inner: stdin,
                shutdown: resources.shutdown_token(),
            };
            let running = service.serve((reader, stdout)).await?;
            running.waiting().await?;
        }
        Some(command) if command == "doctor" => {
            let service =
                CodexShim::from_current_dir_with_resources(RuntimeResources::new(config))?;
            service.verify_root()?;
            service.verify_process_runtime()?;
            println!("codexshim doctor: ok");
            println!("root: {}", service.root_path().display());
            println!("protocol: 2026-07-28");
            println!(
                "protocol compatibility: {}",
                service.protocol_compatibility()
            );
            println!("read-only calls: {MAX_READ_ONLY_CALLS}");
            println!("process calls: {MAX_PROCESS_CALLS}");
            println!("process lifecycle: ok");
            println!(
                "worker lanes: {}",
                service.resources().config().worker_lanes
            );
        }
        Some(command) if command == "--version" || command == "-V" => {
            println!("codexshim {}", env!("CARGO_PKG_VERSION"));
        }
        _ => {
            usage();
            return Err("invalid command".into());
        }
    }
    Ok(())
}

fn main() -> ExitCode {
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
    match runtime.block_on(run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("codexshim: {error}");
            ExitCode::FAILURE
        }
    }
}
