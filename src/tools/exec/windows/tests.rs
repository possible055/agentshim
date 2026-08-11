#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        path::PathBuf,
        sync::{Arc, Barrier, atomic::AtomicBool},
        time::{Duration, Instant},
    };

    use crate::tools::exec::{
        capture::drain,
        resolve::{Launcher, ResolvedProgram},
        spawn::{ThreadCompletion, spawn_monitored},
        windows::{
            platform::{
                BATCH_COMMAND_LINE_LIMIT, LaunchEncoding, Pipe, append_native_argument,
                append_native_argv0, finish_batch_command_line, finish_native_command_line,
                settle_threads_with_deadlines,
            },
            runner::{FAILURE_POINT, FailurePoint, run},
        },
    };
    use crate::tools::exec::{
        resolve::launcher_for,
        spawn::{EnvironmentPlan, ExecPlan, Streams},
    };

    fn fixture_args(args: Vec<&str>) -> Vec<String> {
        args.into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn native_encoder_matches_msvc_argument_rules() {
        let mut encoded = Vec::new();
        append_native_argv0(&mut encoded, OsStr::new(r"C:\tool.exe"));
        for argument in ["", "a b", "q\"r", r"trail\", "a b\\"] {
            encoded.push(u16::from(b' '));
            append_native_argument(&mut encoded, OsStr::new(argument));
        }
        assert_eq!(
            String::from_utf16(&encoded).expect("valid fixture UTF-16"),
            "\"C:\\tool.exe\" \"\" \"a b\" q\\\"r trail\\ \"a b\\\\\""
        );
        assert!(finish_native_command_line(vec![u16::from(b'x'); 32_767]).is_err());
    }

    #[test]
    fn blocked_output_drain_is_cancelled_and_joined() {
        let pipe = Pipe::stdout().expect("output pipe");
        let reader = pipe.parent.into_file();
        let _writer = pipe.child.into_file();
        let completion = ThreadCompletion::new();
        let failed = Arc::new(AtomicBool::new(false));
        let stdin = spawn_monitored(Arc::clone(&failed), completion.clone(), || Ok(()));
        let entered = Arc::new(Barrier::new(2));
        let drain_entered = Arc::clone(&entered);
        let drain = spawn_monitored(failed, completion.clone(), move || {
            drain_entered.wait();
            drain(reader, 1024, None)
        });
        entered.wait();
        let started = Instant::now();

        let (_, captures) = settle_threads_with_deadlines(
            &completion,
            stdin,
            vec![drain],
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
        .expect("cancel blocked output drain");

        assert_eq!(captures.len(), 1);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn native_launch_separates_executable_identity_from_argv0() {
        let resolved = ResolvedProgram {
            absolute: PathBuf::from(r"C:\tools\cargo.exe"),
            executable: PathBuf::from(r"C:\toolchains\rustup.exe"),
            launcher: Launcher::Native,
        };

        let launch = LaunchEncoding::new(&resolved, &fixture_args(vec!["--version"]))
            .expect("encode native proxy");

        assert_eq!(
            String::from_utf16(&launch.application[..launch.application.len() - 1])
                .expect("valid application UTF-16"),
            r"C:\toolchains\rustup.exe"
        );
        assert_eq!(
            String::from_utf16(&launch.command_line[..launch.command_line.len() - 1])
                .expect("valid command-line UTF-16"),
            r#""C:\tools\cargo.exe" --version"#
        );
    }

    #[test]
    fn batch_encoder_tracks_rust_1_88_regular_argument_policy() {
        let resolved = ResolvedProgram {
            absolute: PathBuf::from(r"C:\repo\probe.cmd"),
            executable: PathBuf::from(r"C:\repo\probe.cmd"),
            launcher: Launcher::CmdCompat,
        };
        let args = fixture_args(vec![
            "%PATH%", "!", "^", "&", "|", "<", ">", "a\"b", "tail\\", "", "界",
        ]);
        let launch = LaunchEncoding::new(&resolved, &args).expect("encode batch corpus");
        let encoded = String::from_utf16(&launch.command_line[..launch.command_line.len() - 1])
            .expect("valid fixture UTF-16");
        assert!(encoded.starts_with("cmd.exe /e:ON /v:OFF /d /c \"\"C:\\repo\\probe.cmd\""));
        assert!(encoded.contains("%%cd:~,%PATH%%cd:~,%"));
        assert!(encoded.ends_with('"'));

        assert!(LaunchEncoding::new(&resolved, &fixture_args(vec!["line\rbreak"])).is_err());
        assert!(LaunchEncoding::new(&resolved, &fixture_args(vec!["line\nbreak"])).is_err());
        assert!(finish_batch_command_line(vec![u16::from(b'x'); BATCH_COMMAND_LINE_LIMIT]).is_ok());
        assert!(
            finish_batch_command_line(vec![u16::from(b'x'); BATCH_COMMAND_LINE_LIMIT + 1]).is_err()
        );
    }

    #[test]
    fn batch_script_path_percent_expansion_is_escaped_and_executes_the_literal_file() {
        let fixture = tempfile::tempdir().expect("fixture");
        let script = fixture.path().join("%CD%.cmd");
        std::fs::write(&script, "@echo literal-script\r\n").expect("batch fixture");
        let resolved = ResolvedProgram {
            absolute: script.clone(),
            executable: script,
            launcher: Launcher::CmdCompat,
        };
        let launch = LaunchEncoding::new(&resolved, &[]).expect("encode literal batch path");
        let encoded = String::from_utf16(&launch.command_line[..launch.command_line.len() - 1])
            .expect("valid fixture UTF-16");
        assert!(encoded.contains("%%cd:~,%CD%%cd:~,%"));
        let plan = ExecPlan {
            resolved: &resolved,
            cwd: fixture.path(),
            args: &[],
            environment: &EnvironmentPlan::default(),
            stdin: None,
            streams: Streams::Merged,
            timeout: Duration::from_secs(5),
        };
        let Ok(outcome) = run(&plan, &tokio_util::sync::CancellationToken::new()) else {
            panic!("literal batch run failed");
        };
        assert_eq!(outcome.exit, "0");
        let rendered = outcome.captures[0].render(outcome.captures[0].retained());
        assert!(rendered.text.contains("literal-script"));
    }

    #[test]
    fn every_launcher_transition_failure_uses_raii_cleanup() {
        let fixture = tempfile::tempdir().expect("fixture");
        let executable = std::env::current_exe().expect("test executable");
        let resolved = ResolvedProgram {
            absolute: executable.clone(),
            executable,
            launcher: Launcher::Native,
        };
        for point in [
            FailurePoint::SpawnedSuspended,
            FailurePoint::JobReady,
            FailurePoint::JobAssigned,
            FailurePoint::Running,
        ] {
            let pid_file = fixture.path().join(format!("{point:?}.pid"));
            let args = fixture_args(vec![
                "--exact",
                "tools::run_program::tests::windows::windows_grandchild_child_fixture",
                "--nocapture",
            ]);
            let mut environment = EnvironmentPlan::default();
            environment
                .overrides
                .push(("CODEXSHIM_PROCESS_FIXTURE".to_owned(), "child".to_owned()));
            environment.overrides.push((
                "CODEXSHIM_PROCESS_PID_FILE".to_owned(),
                pid_file.to_string_lossy().into_owned(),
            ));
            let plan = ExecPlan {
                resolved: &resolved,
                cwd: fixture.path(),
                args: &args,
                environment: &environment,
                stdin: None,
                streams: Streams::Separate,
                timeout: Duration::from_secs(5),
            };
            FAILURE_POINT.with(|configured| configured.set(Some(point)));
            let result = run(&plan, &tokio_util::sync::CancellationToken::new());
            FAILURE_POINT.with(|configured| configured.set(None));
            assert!(result.is_err(), "failure point {point:?} was not exercised");
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                assert_process_is_gone(pid.trim().parse().expect("pid integer"));
            }
        }
    }

    #[test]
    fn powershell_scripts_report_a_missing_launcher_capability() {
        let error =
            launcher_for(PathBuf::from("script.ps1").as_path()).expect_err(".ps1 has no launcher");
        assert!(error.to_string().contains("not implemented"));
    }

    fn assert_process_is_gone(pid: u32) {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        };

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if !handle.is_null() {
            unsafe {
                CloseHandle(handle);
            }
        }
        assert!(handle.is_null(), "injected failure left child {pid} alive");
    }
}
