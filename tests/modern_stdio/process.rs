use super::support::*;
use super::*;

#[test]
fn eof_process_child_fixture() {
    if std::env::var("AGENTSHIM_EOF_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    let pid_file = std::env::var_os("AGENTSHIM_EOF_PID_FILE").expect("fixture PID file");
    std::fs::write(pid_file, std::process::id().to_string()).expect("write fixture PID");
    thread::sleep(Duration::from_secs(30));
}

#[test]
fn stdin_eof_cancels_in_flight_process_and_exits_server() {
    let fixture = tempfile::tempdir().expect("fixture");
    let pid_file = fixture.path().join("eof-child.pid");
    let executable = std::env::current_exe().expect("integration test executable");
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let mut call = empty_params();
    call.insert("name".to_owned(), json!("run_program"));
    call.insert(
        "arguments".to_owned(),
        json!({
            "program": executable,
            "args": ["--exact", "process::eof_process_child_fixture", "--nocapture"],
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "env": {
                "AGENTSHIM_EOF_FIXTURE": "child",
                "AGENTSHIM_EOF_PID_FILE": pid_file,
            },
            "timeout_ms": 30_000,
        }),
    );
    session.send(&modern_request(2, "tools/call", call));

    let child_start_deadline = Instant::now() + Duration::from_secs(5);
    let child_pid = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(
            Instant::now() < child_start_deadline,
            "in-flight child did not publish its PID"
        );
        thread::sleep(Duration::from_millis(10));
    };

    session.stdin.take();
    let shutdown_deadline = Instant::now() + Duration::from_secs(12);
    let status = loop {
        if let Some(status) = session.child.try_wait().expect("poll server") {
            break status;
        }
        if Instant::now() >= shutdown_deadline {
            session.child.kill().expect("kill hung server");
            panic!("server did not exit within shutdown and cleanup bounds");
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "server exited with {status}");

    let child_exit_deadline = Instant::now() + Duration::from_secs(2);
    while process_is_running(child_pid) && Instant::now() < child_exit_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_is_running(child_pid),
        "in-flight child survived server EOF shutdown"
    );
}

#[cfg(unix)]
#[test]
#[allow(clippy::zombie_processes)] // The fixture must exit without waiting so the helper escapes its session.
fn unix_outcome_uncertain_parent_fixture() {
    if std::env::var("AGENTSHIM_OUTCOME_UNCERTAIN_FIXTURE").as_deref() != Ok("parent") {
        return;
    }
    let pid_file =
        std::env::var_os("AGENTSHIM_OUTCOME_UNCERTAIN_PID_FILE").expect("fixture PID file");
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("integration test executable"));
    command
        .args([
            "--exact",
            "process::unix_outcome_uncertain_helper_fixture",
            "--nocapture",
        ])
        .env("AGENTSHIM_OUTCOME_UNCERTAIN_FIXTURE", "helper")
        .env("AGENTSHIM_OUTCOME_UNCERTAIN_PID_FILE", &pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().expect("spawn session-escaped helper");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !std::path::Path::new(&pid_file).exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        std::path::Path::new(&pid_file).exists(),
        "session-escaped helper did not record its PID"
    );
}

#[cfg(unix)]
#[test]
fn unix_outcome_uncertain_helper_fixture() {
    if std::env::var("AGENTSHIM_OUTCOME_UNCERTAIN_FIXTURE").as_deref() != Ok("helper") {
        return;
    }
    let pid_file =
        std::env::var_os("AGENTSHIM_OUTCOME_UNCERTAIN_PID_FILE").expect("fixture PID file");
    std::fs::write(pid_file, std::process::id().to_string()).expect("write helper PID");
    thread::sleep(Duration::from_secs(30));
}

#[cfg(unix)]
#[test]
fn session_escaped_descendant_preserves_outcome_uncertain_wire_contract() {
    let fixture = tempfile::tempdir().expect("fixture");
    let pid_file = fixture.path().join("session-escaped.pid");
    let executable = std::env::current_exe().expect("integration test executable");
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let mut call = empty_params();
    call.insert("name".to_owned(), json!("run_program"));
    call.insert(
        "arguments".to_owned(),
        json!({
            "program": executable,
            "args": ["--exact", "process::unix_outcome_uncertain_parent_fixture", "--nocapture"],
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "env": {
                "AGENTSHIM_OUTCOME_UNCERTAIN_FIXTURE": "parent",
                "AGENTSHIM_OUTCOME_UNCERTAIN_PID_FILE": pid_file,
            },
            "timeout_ms": 10_000,
        }),
    );
    let started = Instant::now();
    session.send(&modern_request(2, "tools/call", call));
    let helper_start_deadline = Instant::now() + Duration::from_secs(3);
    while !pid_file.exists() && Instant::now() < helper_start_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let helper_pid = std::fs::read_to_string(&pid_file)
        .expect("helper PID")
        .trim()
        .parse::<u32>()
        .expect("numeric helper PID");
    let mut helper = EscapedHelper::new(helper_pid);
    let response = session.receive();
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "uncertain cleanup exceeded the shared deadline"
    );
    helper.terminate();

    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "outcome_uncertain"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["retryable"],
        false
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["details"]["termination_outcome"],
        "uncertain"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["details"]["containment_scope"],
        "process_group"
    );
    assert!(
        !process_is_running(helper_pid),
        "fixture failed to clean up its escaped helper"
    );
    session.close();
}

#[cfg(unix)]
struct EscapedHelper {
    pid: Option<u32>,
}

#[cfg(unix)]
impl EscapedHelper {
    fn new(pid: u32) -> Self {
        Self { pid: Some(pid) }
    }

    fn terminate(&mut self) {
        let Some(pid) = self.pid.take() else {
            return;
        };
        let pid_i32 = i32::try_from(pid).expect("helper PID fits pid_t");
        // SAFETY: The PID belongs to the fixture-created escaped helper.
        unsafe { libc::kill(pid_i32, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_running(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(unix)]
impl Drop for EscapedHelper {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
pub(super) fn process_is_running(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: Signal zero performs a read-only existence check for the numeric PID.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
pub(super) fn process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    const STILL_ACTIVE_EXIT_CODE: u32 = 259;
    // SAFETY: OpenProcess receives a numeric PID and the returned handle is checked and closed.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let mut exit_code = 0_u32;
    // SAFETY: process is valid and exit_code points to writable memory.
    let succeeded = unsafe { GetExitCodeProcess(process, &raw mut exit_code) } != 0;
    // SAFETY: process is an owned handle returned by OpenProcess.
    unsafe { CloseHandle(process) };
    succeeded && exit_code == STILL_ACTIVE_EXIT_CODE
}
