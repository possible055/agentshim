#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsStr, path::PathBuf};

    use super::super::{launcher_for, validate_launcher_request};
    use super::{
        BATCH_COMMAND_LINE_LIMIT, FAILURE_POINT, FailurePoint, LaunchEncoding, Launcher,
        ProcessRequest, ResolvedProgram, append_native_argument, append_native_argv0,
        finish_batch_command_line, finish_native_command_line, run,
    };

    fn fixture_request(args: Vec<&str>) -> ProcessRequest {
        ProcessRequest {
            program: "fixture".to_owned(),
            args: args.into_iter().map(str::to_owned).collect(),
            cwd: None,
            env: BTreeMap::new(),
            unset_env: Vec::new(),
            stdin: None,
            timeout_ms: Some(1_000),
        }
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
    fn native_launch_separates_executable_identity_from_argv0() {
        let resolved = ResolvedProgram {
            absolute: PathBuf::from(r"C:\tools\cargo.exe"),
            executable: PathBuf::from(r"C:\toolchains\rustup.exe"),
            launcher: Launcher::Native,
        };
        let request = fixture_request(vec!["--version"]);

        let launch = LaunchEncoding::new(&resolved, &request).expect("encode native proxy");

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
        let request = fixture_request(vec![
            "%PATH%", "!", "^", "&", "|", "<", ">", "a\"b", "tail\\", "", "界",
        ]);
        let launch = LaunchEncoding::new(&resolved, &request).expect("encode batch corpus");
        let encoded = String::from_utf16(&launch.command_line[..launch.command_line.len() - 1])
            .expect("valid fixture UTF-16");
        assert!(encoded.starts_with("cmd.exe /e:ON /v:OFF /d /c \"\"C:\\repo\\probe.cmd\""));
        assert!(encoded.contains("%%cd:~,%PATH%%cd:~,%"));
        assert!(encoded.ends_with('"'));

        let mut rejected = fixture_request(vec!["line\rbreak"]);
        assert!(LaunchEncoding::new(&resolved, &rejected).is_err());
        rejected.args = vec!["line\nbreak".to_owned()];
        assert!(LaunchEncoding::new(&resolved, &rejected).is_err());
        assert!(finish_batch_command_line(vec![u16::from(b'x'); BATCH_COMMAND_LINE_LIMIT]).is_ok());
        assert!(
            finish_batch_command_line(vec![u16::from(b'x'); BATCH_COMMAND_LINE_LIMIT + 1]).is_err()
        );
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
            let mut request = fixture_request(vec![
                "--exact",
                "tools::process::tests::windows_grandchild_child_fixture",
                "--nocapture",
            ]);
            request
                .env
                .insert("CODEXSHIM_PROCESS_FIXTURE".to_owned(), "child".to_owned());
            request.env.insert(
                "CODEXSHIM_PROCESS_PID_FILE".to_owned(),
                pid_file.to_string_lossy().into_owned(),
            );
            FAILURE_POINT.with(|configured| configured.set(Some(point)));
            let result = run(
                &resolved,
                fixture.path(),
                &request,
                std::time::Duration::from_secs(5),
                &tokio_util::sync::CancellationToken::new(),
            );
            FAILURE_POINT.with(|configured| configured.set(None));
            assert!(result.is_err(), "failure point {point:?} was not exercised");
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                assert_process_is_gone(pid.trim().parse().expect("pid integer"));
            }
        }
    }

    #[test]
    fn command_evaluation_launchers_and_ps1_are_rejected() {
        let cmd = ResolvedProgram {
            absolute: PathBuf::from(r"C:\tools\safe.exe"),
            executable: PathBuf::from(r"C:\Windows\System32\cmd.exe"),
            launcher: Launcher::Native,
        };
        let cmd_request = fixture_request(vec!["/d", "/c", "echo injected"]);
        assert!(validate_launcher_request(&cmd, &cmd_request).is_err());

        let powershell = ResolvedProgram {
            absolute: PathBuf::from(r"C:\tools\safe.exe"),
            executable: PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            launcher: Launcher::Native,
        };
        for switch in [
            "-Command",
            "-c",
            "-CommandWithArgs",
            "-cwa",
            "-EncodedCommand",
            "-e",
            "-ec",
            "-enc",
            "-command:Get-Process",
            "-encodedcommand=payload",
        ] {
            let powershell_request = fixture_request(vec!["-NoProfile", switch, "Get-Process"]);
            assert!(
                validate_launcher_request(&powershell, &powershell_request).is_err(),
                "{switch} must be rejected"
            );
        }
        assert!(launcher_for(PathBuf::from("script.ps1").as_path()).is_err());
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
