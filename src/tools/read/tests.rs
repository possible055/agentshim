#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::{
        AFTER_READ_HOOK, BEFORE_READ_HOOK, MAX_LINE_COUNT, ReadError, ReadRequest, execute,
    };
    use crate::path::{FileAccess, ReadScope, RepositoryRoot};

    fn access(path: &std::path::Path) -> Arc<FileAccess> {
        access_with_scope(path, ReadScope::Normal)
    }

    fn access_with_scope(path: &std::path::Path, scope: ReadScope) -> Arc<FileAccess> {
        Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(path).expect("root")),
            scope,
        ))
    }

    fn request(path: &str) -> ReadRequest {
        ReadRequest {
            path: path.to_owned(),
            start_line: None,
            line_count: None,
            encoding: None,
        }
    }

    #[test]
    fn reads_numbered_utf8_crlf_and_utf16_pages() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("utf8.txt"), "alpha\r\nbeta\n").expect("utf8");
        fs::write(fixture.path().join("utf8-bom.txt"), b"\xEF\xBB\xBFbom\n").expect("utf8 bom");
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "one\ntwo\nthree".encode_utf16() {
            utf16.extend(unit.to_le_bytes());
        }
        fs::write(fixture.path().join("utf16.txt"), utf16).expect("utf16");
        let mut utf16be = vec![0xFE, 0xFF];
        for unit in "big\nend".encode_utf16() {
            utf16be.extend(unit.to_be_bytes());
        }
        fs::write(fixture.path().join("utf16be.txt"), utf16be).expect("utf16be");
        fs::write(fixture.path().join("latin.txt"), [0x63, 0x61, 0x66, 0xE9])
            .expect("windows-1252");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();

        let utf8 = execute(&root, &request("utf8.txt"), &cancellation).expect("read utf8");
        assert!(utf8.contains("1\talpha\n2\tbeta\nComplete."));
        let bom = execute(&root, &request("utf8-bom.txt"), &cancellation).expect("utf8 bom");
        assert!(bom.contains("1\tbom"));

        let mut page = request("utf16.txt");
        page.start_line = Some(2);
        page.line_count = Some(1);
        let utf16 = execute(&root, &page, &cancellation).expect("read utf16");
        assert!(utf16.contains("Encoding: UTF-16LE\n2\ttwo"));
        assert!(utf16.ends_with("Partial: next_start_line=3."));
        let be = execute(&root, &request("utf16be.txt"), &cancellation).expect("utf16be");
        assert!(be.contains("Encoding: UTF-16BE\n1\tbig"));
        let mut latin = request("latin.txt");
        latin.encoding = Some("windows-1252".to_owned());
        let latin = execute(&root, &latin, &cancellation).expect("explicit encoding");
        assert!(latin.contains("Encoding: windows-1252\n1\tcafé"));
    }

    #[test]
    fn empty_long_binary_invalid_and_out_of_range_are_bounded() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("empty.txt"), "").expect("empty");
        fs::write(fixture.path().join("long.txt"), "x".repeat(100_000)).expect("long");
        fs::write(fixture.path().join("binary.bin"), b"\x89PNG\r\n\x1A\nrest").expect("binary");
        fs::write(fixture.path().join("invalid.txt"), [0xFF]).expect("invalid");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();

        let empty = execute(&root, &request("empty.txt"), &cancellation).expect("empty read");
        assert!(empty.ends_with("\nComplete."));
        let long = execute(&root, &request("long.txt"), &cancellation).expect("long read");
        assert!(long.contains("[line truncated]"));
        assert!(long.len() <= crate::output::MODEL_BYTE_LIMIT);
        assert!(matches!(
            execute(&root, &request("binary.bin"), &cancellation),
            Err(ReadError::Binary)
        ));
        assert!(matches!(
            execute(&root, &request("invalid.txt"), &cancellation),
            Err(ReadError::Decode(_))
        ));

        let mut beyond = request("empty.txt");
        beyond.start_line = Some(100);
        let output = execute(&root, &beyond, &cancellation).expect("past eof");
        assert!(output.ends_with("\nComplete."));
    }

    #[test]
    fn retries_one_change_then_succeeds() {
        let fixture = tempfile::tempdir().expect("fixture");
        let path = fixture.path().join("race.txt");
        fs::write(&path, "old\n").expect("old");
        let changed = path.clone();
        BEFORE_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(&changed, "new\n").expect("change file");
            }));
        });
        let root = access(fixture.path());
        let output = execute(&root, &request("race.txt"), &CancellationToken::new())
            .expect("retry succeeds");
        assert!(output.contains("1\tnew"));
    }

    #[test]
    fn unrestricted_external_read_preserves_change_detection() {
        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        let path = outside.path().join("race.txt");
        fs::write(&path, "old\n").expect("old");
        let changed = path.clone();
        BEFORE_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(&changed, "new\n").expect("change file");
            }));
        });
        let root = access_with_scope(fixture.path(), ReadScope::Unrestricted);
        let output = execute(
            &root,
            &request(&path.to_string_lossy()),
            &CancellationToken::new(),
        )
        .expect("ambient retry succeeds");
        assert!(output.contains("1\tnew"));
    }

    #[test]
    fn second_change_fails_explicitly() {
        let fixture = tempfile::tempdir().expect("fixture");
        let path = fixture.path().join("race.txt");
        fs::write(&path, "old\n").expect("old");
        let changed = path.clone();
        AFTER_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(&changed, "first\n").expect("first change");
                let changed_again = changed.clone();
                BEFORE_READ_HOOK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move || {
                        fs::write(&changed_again, "second\n").expect("second change");
                    }));
                });
            }));
        });
        let root = access(fixture.path());
        assert!(matches!(
            execute(&root, &request("race.txt"), &CancellationToken::new()),
            Err(ReadError::Changed)
        ));
    }

    #[test]
    fn webp_magic_requires_riff_container() {
        assert!(!super::has_binary_magic(b"abcdefghWEBP source text"));
        assert!(super::has_binary_magic(b"RIFF1234WEBP"));
    }

    #[test]
    fn validation_and_directory_fail_before_content_read() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir(fixture.path().join("directory")).expect("directory");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();

        let mut invalid = request("directory");
        invalid.line_count = Some(MAX_LINE_COUNT + 1);
        assert!(matches!(
            execute(&root, &invalid, &cancellation),
            Err(ReadError::Validation(_))
        ));
        assert!(matches!(
            execute(&root, &request("directory"), &cancellation),
            Err(ReadError::Directory)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn capability_allows_internal_symlink_and_blocks_escape() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(fixture.path().join("target.txt"), "inside\n").expect("target");
        fs::write(outside.path().join("secret.txt"), "outside\n").expect("secret");
        symlink("target.txt", fixture.path().join("inside-link")).expect("inside link");
        symlink(
            outside.path().join("secret.txt"),
            fixture.path().join("escape-link"),
        )
        .expect("escape link");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();

        let inside =
            execute(&root, &request("inside-link"), &cancellation).expect("internal symlink read");
        assert!(inside.contains("1\tinside"));
        assert!(execute(&root, &request("escape-link"), &cancellation).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unrestricted_external_read_rejects_explicit_symlinks() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        let target = outside.path().join("target.txt");
        let link = outside.path().join("link.txt");
        fs::write(&target, "outside\n").expect("target");
        symlink(&target, &link).expect("link");
        let root = access_with_scope(fixture.path(), ReadScope::Unrestricted);
        assert!(matches!(
            execute(
                &root,
                &request(&link.to_string_lossy()),
                &CancellationToken::new()
            ),
            Err(ReadError::NotRegular)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn named_pipe_and_device_paths_are_rejected_without_blocking() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        let fixture = tempfile::tempdir().expect("fixture");
        let fifo = fixture.path().join("source.fifo");
        let fifo_bytes = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path");
        assert_eq!(unsafe { libc::mkfifo(fifo_bytes.as_ptr(), 0o600) }, 0);
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();
        assert!(execute(&root, &request("source.fifo"), &cancellation).is_err());
        assert!(execute(&root, &request("/dev/null"), &cancellation).is_err());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an elevated Windows process to create symbolic-link fixtures"]
    fn windows_symlink_capability_allows_internal_link_and_blocks_reparse_escape() {
        use std::os::windows::fs::symlink_file;

        let fixture = tempfile::tempdir().expect("fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(fixture.path().join("target.txt"), "inside\n").expect("target");
        fs::write(outside.path().join("secret.txt"), "outside\n").expect("secret");
        symlink_file("target.txt", fixture.path().join("inside-link")).expect("inside link");
        symlink_file(
            outside.path().join("secret.txt"),
            fixture.path().join("escape-link"),
        )
        .expect("escape reparse link");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();

        let inside =
            execute(&root, &request("inside-link"), &cancellation).expect("internal link read");
        assert!(inside.contains("1\tinside"));
        assert!(execute(&root, &request("escape-link"), &cancellation).is_err());

        let ambient_link = outside.path().join("ambient-link");
        symlink_file(outside.path().join("secret.txt"), &ambient_link).expect("ambient link");
        let unrestricted = access_with_scope(fixture.path(), ReadScope::Unrestricted);
        assert!(matches!(
            execute(
                &unrestricted,
                &request(&ambient_link.to_string_lossy()),
                &cancellation
            ),
            Err(ReadError::NotRegular)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_unicode_space_and_long_path_read() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut relative = std::path::PathBuf::from("Unicode space 界");
        for index in 0..12 {
            relative.push(format!("long-segment-{index:02}-xxxxxxxx"));
        }
        fs::create_dir_all(fixture.path().join(&relative)).expect("long path directories");
        relative.push("source file.rs");
        fs::write(fixture.path().join(&relative), "long path content\n").expect("long path file");
        assert!(fixture.path().join(&relative).as_os_str().len() > 260);
        let root = access(fixture.path());
        let output = execute(
            &root,
            &request(&relative.to_string_lossy()),
            &CancellationToken::new(),
        )
        .expect("long path read");
        assert!(output.contains("1\tlong path content"));
    }
}
