use super::*;

#[cfg(windows)]
#[test]
fn windows_grandchild_child_fixture() {
    if env::var("CODEXSHIM_PROCESS_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    let pid_file = env::var_os("CODEXSHIM_PROCESS_PID_FILE").expect("pid file");
    std::fs::write(pid_file, std::process::id().to_string()).expect("write child pid");
    thread::sleep(Duration::from_secs(30));
}

#[cfg(windows)]
#[test]
fn windows_grandchild_parent_fixture() {
    use std::io::Write as _;

    if env::var("CODEXSHIM_PROCESS_FIXTURE").as_deref() != Ok("parent") {
        return;
    }
    writeln!(std::io::stdout(), "timeout stdout evidence").expect("write stdout evidence");
    std::io::stdout().flush().expect("flush stdout evidence");
    writeln!(std::io::stderr(), "timeout stderr evidence").expect("write stderr evidence");
    std::io::stderr().flush().expect("flush stderr evidence");
    let executable = env::current_exe().expect("test executable");
    let status = Command::new(executable)
        .args([
            "--exact",
            "tools::run_program::tests::windows::windows_grandchild_child_fixture",
            "--nocapture",
        ])
        .env("CODEXSHIM_PROCESS_FIXTURE", "child")
        .env(
            "CODEXSHIM_PROCESS_PID_FILE",
            env::var_os("CODEXSHIM_PROCESS_PID_FILE").expect("pid file"),
        )
        .status()
        .expect("spawn child fixture");
    assert!(status.success());
}

#[cfg(windows)]
#[test]
fn windows_lingering_grandchild_parent_fixture() {
    if env::var("CODEXSHIM_PROCESS_FIXTURE").as_deref() != Ok("lingering-parent") {
        return;
    }
    let pid_file = env::var_os("CODEXSHIM_PROCESS_PID_FILE").expect("pid file");
    let executable = env::current_exe().expect("test executable");
    let child = Command::new(executable)
        .args([
            "--exact",
            "tools::run_program::tests::windows::windows_grandchild_child_fixture",
            "--nocapture",
        ])
        .env("CODEXSHIM_PROCESS_FIXTURE", "child")
        .env("CODEXSHIM_PROCESS_PID_FILE", &pid_file)
        .spawn()
        .expect("spawn lingering child fixture");
    let pid_file = std::path::PathBuf::from(pid_file);
    let started = std::time::Instant::now();
    while !pid_file.exists() && started.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(pid_file.exists(), "lingering child did not start");
    drop(child);
}

#[cfg(windows)]
fn windows_process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    const STILL_ACTIVE_EXIT_CODE: u32 = 259;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0_u32;
    let succeeded = unsafe { GetExitCodeProcess(handle, &raw mut exit_code) } != 0;
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
        "CODEXSHIM_PROCESS_FIXTURE".to_owned(),
        "lingering-parent".to_owned(),
    );
    request.env.insert(
        "CODEXSHIM_PROCESS_PID_FILE".to_owned(),
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
        .insert("CODEXSHIM_PROCESS_FIXTURE".to_owned(), "parent".to_owned());
    timed.env.insert(
        "CODEXSHIM_PROCESS_PID_FILE".to_owned(),
        pid_file.to_string_lossy().into_owned(),
    );
    timed.timeout_ms = Some(750);
    let started = std::time::Instant::now();
    let error = execute(
        &root,
        &ProcessResolver::capture(),
        &timed,
        Duration::from_millis(750),
        &CancellationToken::new(),
    )
    .expect_err("timeout");
    assert!(
        started.elapsed() < Duration::from_secs(3),
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
