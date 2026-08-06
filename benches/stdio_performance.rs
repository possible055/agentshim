use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::Instant,
};

use serde_json::{Value, json};

const WARM_SAMPLES: usize = 7;
const COLD_LIMIT_ENV: &str = "CODEXSHIM_BENCH_MAX_STDIO_COLD_MS";
const P95_LIMIT_ENV: &str = "CODEXSHIM_BENCH_MAX_STDIO_P95_MS";
const PROCESS_LIMIT_ENV: &str = "CODEXSHIM_BENCH_MAX_PROCESS_MS";

fn main() {
    for mode in ["off", "errors", "all"] {
        benchmark_mode(mode);
    }
}

fn benchmark_mode(mode: &str) {
    let logs = tempfile::tempdir().expect("diagnostic directory");
    let cold_started = Instant::now();
    let mut session = Session::start(mode, logs.path());
    let expected = session.read(1);
    let cold_ms = cold_started.elapsed().as_secs_f64() * 1_000.0;
    let mut warm_ms = Vec::with_capacity(WARM_SAMPLES);
    for id in 2..=WARM_SAMPLES as u64 + 1 {
        let started = Instant::now();
        assert_eq!(session.read(id), expected, "stdio output changed");
        warm_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    let process_started = Instant::now();
    let process_output = session.run_process(WARM_SAMPLES as u64 + 2);
    let process_ms = process_started.elapsed().as_secs_f64() * 1_000.0;
    session.close();
    warm_ms.sort_by(f64::total_cmp);
    let p95_ms = percentile(&warm_ms, 95, 100);
    let cold_limit_ms = configured_limit(COLD_LIMIT_ENV, 750.0);
    let p95_limit_ms = configured_limit(P95_LIMIT_ENV, 10.0);
    let process_limit_ms = configured_limit(PROCESS_LIMIT_ENV, 300.0);
    assert!(
        cold_ms <= cold_limit_ms,
        "stdio {mode} cold start {cold_ms:.3} ms exceeds {cold_limit_ms:.3} ms"
    );
    assert!(
        p95_ms <= p95_limit_ms,
        "stdio {mode} p95 {p95_ms:.3} ms exceeds {p95_limit_ms:.3} ms"
    );
    assert!(
        process_ms <= process_limit_ms,
        "stdio {mode} process {process_ms:.3} ms exceeds {process_limit_ms:.3} ms"
    );
    println!(
        "{}",
        json!({
            "benchmark": "stdio_diagnostics",
            "mode": mode,
            "cold_ms": cold_ms,
            "warm_ms": warm_ms,
            "p50_ms": percentile(&warm_ms, 50, 100),
            "p95_ms": p95_ms,
            "p99_ms": percentile(&warm_ms, 99, 100),
            "cold_limit_ms": cold_limit_ms,
            "p95_limit_ms": p95_limit_ms,
            "process_limit_ms": process_limit_ms,
            "output_equivalent": true,
            "process_ms": process_ms,
            "process_output_bytes": process_output.to_string().len(),
        })
    );
}

fn configured_limit(name: &str, default: f64) -> f64 {
    let value = std::env::var(name).map_or(default, |value| {
        value
            .parse::<f64>()
            .unwrap_or_else(|_| panic!("{name} must be a number"))
    });
    assert!(value.is_finite() && value > 0.0, "{name} must be positive");
    value
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(mode: &str, logs: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexshim"))
            .arg("serve")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("CODEXSHIM_LOG_MODE", mode)
            .env("CODEXSHIM_LOG_DIR", logs)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start stdio server");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    fn read(&mut self, id: u64) -> Value {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "read",
                "arguments": { "path": "Cargo.toml", "line_count": 1000 },
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "stdio-performance",
                        "version": "1.0.0"
                    },
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        let stdin = self.stdin.as_mut().expect("stdin open");
        serde_json::to_writer(&mut *stdin, &request).expect("request");
        stdin.write_all(b"\n").expect("request newline");
        stdin.flush().expect("request flush");
        let mut response = String::new();
        self.stdout.read_line(&mut response).expect("response");
        let response: Value = serde_json::from_str(&response).expect("JSON-RPC response");
        assert_eq!(response["id"], id);
        response["result"].clone()
    }

    fn run_process(&mut self, id: u64) -> Value {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "run_process",
                "arguments": {
                    "program": env!("CARGO_BIN_EXE_codexshim"),
                    "args": ["--version"],
                    "timeout_ms": 30000
                },
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "stdio-performance",
                        "version": "1.0.0"
                    },
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        let stdin = self.stdin.as_mut().expect("stdin open");
        serde_json::to_writer(&mut *stdin, &request).expect("process request");
        stdin.write_all(b"\n").expect("process request newline");
        stdin.flush().expect("process request flush");
        let mut response = String::new();
        self.stdout
            .read_line(&mut response)
            .expect("process response");
        let response: Value = serde_json::from_str(&response).expect("process JSON-RPC response");
        assert_eq!(response["id"], id);
        response["result"].clone()
    }

    fn close(mut self) {
        self.stdin.take();
        let status = self.child.wait().expect("stdio server exit");
        assert!(status.success(), "stdio server exited with {status}");
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn percentile(samples: &[f64], numerator: usize, denominator: usize) -> f64 {
    let index = samples
        .len()
        .saturating_mul(numerator)
        .div_ceil(denominator)
        .saturating_sub(1)
        .min(samples.len() - 1);
    samples[index]
}
