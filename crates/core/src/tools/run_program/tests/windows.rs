use super::*;

#[cfg(windows)]
#[test]
fn windows_grandchild_child_fixture() {
    if env::var("AGENTSHIM_PROCESS_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    let pid_file = env::var_os("AGENTSHIM_PROCESS_PID_FILE").expect("pid file");
    std::fs::write(pid_file, std::process::id().to_string()).expect("write child pid");
    thread::sleep(Duration::from_secs(30));
}

#[cfg(windows)]
#[test]
fn windows_grandchild_parent_fixture() {
    use std::io::Write as _;

    if env::var("AGENTSHIM_PROCESS_FIXTURE").as_deref() != Ok("parent") {
        return;
    }
    let pid_file =
        std::path::PathBuf::from(env::var_os("AGENTSHIM_PROCESS_PID_FILE").expect("pid file"));
    let executable = env::current_exe().expect("test executable");
    let mut child = Command::new(executable)
        .args([
            "--exact",
            "tools::run_program::tests::windows::windows_grandchild_child_fixture",
            "--nocapture",
        ])
        .env("AGENTSHIM_PROCESS_FIXTURE", "child")
        .env(
            "AGENTSHIM_PROCESS_PID_FILE",
            pid_file.with_extension("child-ready"),
        )
        .spawn()
        .expect("spawn child fixture");
    std::fs::write(pid_file, child.id().to_string()).expect("write child pid");
    writeln!(std::io::stdout(), "timeout stdout evidence").expect("write stdout evidence");
    std::io::stdout().flush().expect("flush stdout evidence");
    writeln!(std::io::stderr(), "timeout stderr evidence").expect("write stderr evidence");
    std::io::stderr().flush().expect("flush stderr evidence");
    let status = child.wait().expect("wait for child fixture");
    assert!(status.success());
}

#[cfg(windows)]
#[test]
fn windows_lingering_grandchild_parent_fixture() {
    if env::var("AGENTSHIM_PROCESS_FIXTURE").as_deref() != Ok("lingering-parent") {
        return;
    }
    let pid_file = env::var_os("AGENTSHIM_PROCESS_PID_FILE").expect("pid file");
    let executable = env::current_exe().expect("test executable");
    let child = Command::new(executable)
        .args([
            "--exact",
            "tools::run_program::tests::windows::windows_grandchild_child_fixture",
            "--nocapture",
        ])
        .env("AGENTSHIM_PROCESS_FIXTURE", "child")
        .env("AGENTSHIM_PROCESS_PID_FILE", &pid_file)
        .spawn()
        .expect("spawn lingering child fixture");
    let pid_file = std::path::PathBuf::from(pid_file);
    let started = std::time::Instant::now();
    while std::fs::read_to_string(&pid_file).map_or(true, |content| content.trim().is_empty())
        && started.elapsed() < Duration::from_secs(2)
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        std::fs::read_to_string(&pid_file).is_ok_and(|content| !content.trim().is_empty()),
        "lingering child did not start"
    );
    drop(child);
}

#[cfg(windows)]
fn windows_process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    const STILL_ACTIVE_EXIT_CODE: u32 = 259;
    // Safety: a pure query-open of a PID this helper owns.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0_u32;
    // Safety: the handle is open and `exit_code` is a valid out parameter.
    let succeeded = unsafe { GetExitCodeProcess(handle, &raw mut exit_code) } != 0;
    // Safety: the handle is closed exactly once here.
    unsafe {
        CloseHandle(handle);
    }
    succeeded && exit_code == STILL_ACTIVE_EXIT_CODE
}

#[cfg(windows)]
#[test]
fn windows_primary_exit_terminates_lingering_grandchild() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let pid_file = fixture.path().join("lingering-grandchild.pid");
    let executable = env::current_exe().expect("test executable");
    let mut request = request(executable.to_string_lossy().into_owned());
    request.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::windows::windows_lingering_grandchild_parent_fixture"
            .to_owned(),
        "--nocapture".to_owned(),
    ];
    request.env.insert(
        "AGENTSHIM_PROCESS_FIXTURE".to_owned(),
        "lingering-parent".to_owned(),
    );
    request.env.insert(
        "AGENTSHIM_PROCESS_PID_FILE".to_owned(),
        pid_file.to_string_lossy().into_owned(),
    );
    request.timeout_ms = Some(5_000);
    let started = std::time::Instant::now();

    let output = execute(
        &root,
        &ProcessResolver::capture(),
        &request,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .expect("completed primary process");

    assert!(output.contains("Exit code: 0"));
    assert!(started.elapsed() < Duration::from_secs(2));
    let pid = std::fs::read_to_string(pid_file)
        .expect("lingering child pid")
        .trim()
        .parse::<u32>()
        .expect("pid integer");
    assert!(
        !windows_process_is_running(pid),
        "lingering grandchild survived primary completion"
    );
}

#[cfg(windows)]
#[test]
fn windows_timeout_terminates_grandchild_job_tree() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let pid_file = fixture.path().join("grandchild.pid");
    let executable = env::current_exe().expect("test executable");
    let mut timed = request(executable.to_string_lossy().into_owned());
    timed.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::windows::windows_grandchild_parent_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    timed
        .env
        .insert("AGENTSHIM_PROCESS_FIXTURE".to_owned(), "parent".to_owned());
    timed.env.insert(
        "AGENTSHIM_PROCESS_PID_FILE".to_owned(),
        pid_file.to_string_lossy().into_owned(),
    );
    timed.timeout_ms = Some(2_000);
    let started = std::time::Instant::now();
    let error = execute(
        &root,
        &ProcessResolver::capture(),
        &timed,
        Duration::from_millis(2_000),
        &CancellationToken::new(),
    )
    .expect_err("timeout");
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "timeout cleanup waited for inherited output pipes"
    );
    assert!(
        matches!(&error, ProcessError::Timeout { .. }),
        "unexpected process error: {error}"
    );
    let report = error.to_string();
    assert!(report.contains("Resolved program:"));
    assert!(report.contains("Launcher: native"));
    assert!(report.contains("Cwd:"));
    assert!(report.contains("timeout stdout evidence"));
    assert!(report.contains("timeout stderr evidence"));
    assert!(report.contains("Exit code: unavailable (timed out)"));
    assert!(report.ends_with("Incomplete."));
    assert!(report.len() <= crate::output::MODEL_BYTE_LIMIT);
    let pid = std::fs::read_to_string(pid_file)
        .expect("grandchild pid")
        .trim()
        .parse::<u32>()
        .expect("pid integer");
    assert!(
        !windows_process_is_running(pid),
        "grandchild process survived job termination"
    );
}

#[cfg(windows)]
#[test]
fn windows_memory_limit_child_fixture() {
    if env::var("AGENTSHIM_PROCESS_FIXTURE").as_deref() != Ok("memory") {
        return;
    }
    let mut allocation = Vec::with_capacity(256 * 1024 * 1024);
    allocation.resize(256 * 1024 * 1024, 1_u8);
    std::hint::black_box(allocation);
}

#[cfg(windows)]
#[test]
fn windows_job_memory_limit_fixture() {
    let Ok(kind) = env::var("AGENTSHIM_PROCESS_FIXTURE") else {
        return;
    };
    if kind == "job-memory-child" {
        let allocation = vec![1_u8; 96 * 1024 * 1024];
        std::hint::black_box(allocation);
        return;
    }
    if kind != "job-memory-parent" {
        return;
    }
    let allocation = vec![1_u8; 96 * 1024 * 1024];
    let status = Command::new(env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "tools::run_program::tests::windows::windows_job_memory_limit_fixture",
            "--nocapture",
        ])
        .env("AGENTSHIM_PROCESS_FIXTURE", "job-memory-child")
        .status()
        .expect("run aggregate-memory child");
    std::hint::black_box(allocation);
    assert!(status.success(), "aggregate-memory child was limited");
}

#[cfg(windows)]
fn assert_nonzero_exit_report(output: &str, context: &str) {
    let exit = output
        .lines()
        .find(|line| line.starts_with("Exit code: "))
        .unwrap_or_else(|| panic!("{context} omitted the exit diagnostic: {output}"));
    let code = exit
        .strip_prefix("Exit code: ")
        .and_then(|code| code.parse::<u32>().ok())
        .unwrap_or_else(|| panic!("{context} did not report a numeric exit: {output}"));
    assert_ne!(code, 0, "{context} unexpectedly succeeded");
}

#[cfg(windows)]
#[test]
fn windows_active_process_limit_blocks_a_grandchild() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let pid_file = fixture.path().join("limited-grandchild.pid");
    let executable = env::current_exe().expect("test executable");
    let mut limited = request(executable.to_string_lossy().into_owned());
    limited.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::windows::windows_grandchild_parent_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    limited
        .env
        .insert("AGENTSHIM_PROCESS_FIXTURE".to_owned(), "parent".to_owned());
    limited.env.insert(
        "AGENTSHIM_PROCESS_PID_FILE".to_owned(),
        pid_file.to_string_lossy().into_owned(),
    );
    let policy = crate::platform::process::WindowsJobLimits {
        active_process_limit: Some(1),
        ..Default::default()
    };

    let output = crate::platform::process::with_windows_job_limits_for_test(policy, || {
        execute(
            &root,
            &ProcessResolver::capture(),
            &limited,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
    })
    .expect("primary process report");

    assert_nonzero_exit_report(&output, "active-process limit");
    assert!(
        !pid_file.exists(),
        "limited job created a grandchild PID file"
    );
}

#[cfg(windows)]
#[test]
fn windows_process_memory_limit_refuses_an_oversized_allocation() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let executable = env::current_exe().expect("test executable");
    let mut limited = request(executable.to_string_lossy().into_owned());
    limited.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::windows::windows_memory_limit_child_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    limited
        .env
        .insert("AGENTSHIM_PROCESS_FIXTURE".to_owned(), "memory".to_owned());
    let policy = crate::platform::process::WindowsJobLimits {
        process_memory_bytes: Some(64 * 1024 * 1024),
        ..Default::default()
    };

    let output = crate::platform::process::with_windows_job_limits_for_test(policy, || {
        execute(
            &root,
            &ProcessResolver::capture(),
            &limited,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
    })
    .expect("limited process report");

    assert_nonzero_exit_report(&output, "per-process memory limit");
}

#[cfg(windows)]
#[test]
fn windows_job_memory_limit_refuses_aggregate_allocations() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let executable = env::current_exe().expect("test executable");
    let mut limited = request(executable.to_string_lossy().into_owned());
    limited.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::windows::windows_job_memory_limit_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    limited.env.insert(
        "AGENTSHIM_PROCESS_FIXTURE".to_owned(),
        "job-memory-parent".to_owned(),
    );
    let policy = crate::platform::process::WindowsJobLimits {
        job_memory_bytes: Some(160 * 1024 * 1024),
        ..Default::default()
    };

    let output = crate::platform::process::with_windows_job_limits_for_test(policy, || {
        execute(
            &root,
            &ProcessResolver::capture(),
            &limited,
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
    })
    .expect("aggregate-limited process report");

    assert_nonzero_exit_report(&output, "aggregate job memory limit");
}
