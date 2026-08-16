#[cfg(unix)]
use std::fs;
use std::path::PathBuf;

use super::{
    capture::{Capture, capture_bytes_per_stream},
    resolve::ProcessResolver,
};
#[cfg(unix)]
#[test]
fn resolver_ignores_empty_path_and_requires_executable_regular_file() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().expect("fixture");
    let executable = fixture.path().join("probe");
    fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("write probe");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("chmod");
    let resolver = ProcessResolver::for_tests(vec![fixture.path().to_owned()]);
    let program = resolver.resolve("probe", fixture.path()).expect("resolve");
    let executable = fs::canonicalize(executable).expect("canonical");
    assert_eq!(program.absolute, executable);
    assert_eq!(program.executable, executable);
    assert!(resolver.resolve("probe arg", fixture.path()).is_err());
}

fn install_probe(directory: &std::path::Path) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let probe = directory.join("cachedprobe");
        std::fs::write(&probe, "#!/bin/sh\nexit 0\n").expect("write probe");
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        probe
    }
    #[cfg(windows)]
    {
        let probe = directory.join("cachedprobe.exe");
        std::fs::copy(std::env::current_exe().expect("test executable"), &probe)
            .expect("copy probe");
        probe
    }
}

#[test]
fn failed_resolution_is_not_cached() {
    let fixture = tempfile::tempdir().expect("fixture");
    let resolver = ProcessResolver::for_tests(vec![fixture.path().to_owned()]);

    assert!(resolver.resolve("cachedprobe", fixture.path()).is_err());
    install_probe(fixture.path());

    let found = resolver
        .resolve("cachedprobe", fixture.path())
        .expect("a program installed mid-session must still resolve");
    assert!(found.executable.is_absolute());
}

#[test]
fn cached_resolution_repeats_the_first_answer() {
    let fixture = tempfile::tempdir().expect("fixture");
    let probe = install_probe(fixture.path());
    let resolver = ProcessResolver::for_tests(vec![fixture.path().to_owned()]);

    let first = resolver
        .resolve("cachedprobe", fixture.path())
        .expect("first resolve");
    std::fs::remove_file(&probe).expect("remove probe");
    let cached = resolver
        .resolve("cachedprobe", fixture.path())
        .expect("cached resolve");

    assert_eq!(first.absolute, cached.absolute);
    assert_eq!(first.executable, cached.executable);
}

#[cfg(unix)]
#[test]
fn resolver_cache_preserves_multicall_proxy_identity() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("fixture");
    let proxy = fixture.path().join("cargo");
    symlink(std::env::current_exe().expect("test executable"), &proxy).expect("proxy");
    let resolver = ProcessResolver::for_tests(vec![fixture.path().to_owned()]);

    let first = resolver.resolve("cargo", fixture.path()).expect("resolve");
    let cached = resolver
        .resolve("cargo", fixture.path())
        .expect("cached resolve");

    let expected_absolute = fs::canonicalize(fixture.path())
        .expect("canonical fixture")
        .join("cargo");
    let expected_executable =
        fs::canonicalize(std::env::current_exe().expect("test executable")).expect("canonical");
    for resolved in [&first, &cached] {
        assert_eq!(resolved.absolute, expected_absolute);
        assert_eq!(resolved.executable, expected_executable);
    }
    assert_ne!(first.absolute, first.executable);
}

#[test]
fn invalid_utf8_is_escaped_across_valid_spans() {
    let mut capture = Capture::new(capture_bytes_per_stream(2, crate::output::MODEL_BYTE_LIMIT));
    capture.push(b"a\xF0\x9F");
    capture.push(b"\x92\xA9b\xFF");
    let rendered = capture.render(capture.retained());
    assert_eq!(rendered.text, "a💩b\\xFF");
    assert_eq!(rendered.invalid_bytes, 1);
    assert_eq!(rendered.encoding, "utf-8-with-byte-escapes");
}

#[cfg(windows)]
#[test]
fn cmd_compat_oem_fallback_preserves_utf8_and_decodes_cp950_runs() {
    let mut utf8 = Capture::new_windows_oem(1024, 950);
    utf8.push("UTF-8 界".as_bytes());
    let rendered = utf8.render(utf8.retained());
    assert_eq!(rendered.text, "UTF-8 界");
    assert_eq!(rendered.encoding, "utf-8");
    assert_eq!(rendered.invalid_bytes, 0);

    let mut mixed = Capture::new_windows_oem(1024, 950);
    mixed.push(b"prefix \xA4\xA4\xA4\xE5 suffix");
    let rendered = mixed.render(mixed.retained());
    assert_eq!(rendered.text, "prefix 中文 suffix");
    assert_eq!(rendered.encoding, "windows-oem-950-fallback");
    assert_eq!(rendered.invalid_bytes, 4);

    let mut binary = Capture::new_windows_oem(1024, 950);
    binary.push(b"binary \xA4");
    let rendered = binary.render(binary.retained());
    assert_eq!(rendered.text, "binary \\xA4");
    assert_eq!(rendered.encoding, "utf-8-with-byte-escapes");
    assert_eq!(rendered.invalid_bytes, 1);
}

#[test]
fn capture_keeps_bounded_head_and_tail_while_counting_all_bytes() {
    let mut capture = Capture::new(capture_bytes_per_stream(2, crate::output::MODEL_BYTE_LIMIT));
    let budget = capture.head_limit() + capture.tail_limit();
    let bytes = vec![b'x'; budget + 17];
    capture.push(&bytes);
    assert_eq!(capture.bytes_read, bytes.len());
    assert_eq!(capture.retained(), budget);
    assert_eq!(capture.dropped(), 17);
}

#[test]
fn capture_projection_preserves_valid_utf8_at_byte_boundaries() {
    let mut capture = Capture::new(capture_bytes_per_stream(2, crate::output::MODEL_BYTE_LIMIT));
    let source = "界".repeat(capture.head_limit() + capture.tail_limit());
    capture.push(source.as_bytes());

    let rendered = capture.render(capture.retained());

    assert_eq!(rendered.invalid_bytes, 0);
    assert!(rendered.omitted_bytes > 0);
    assert_eq!(rendered.shown_bytes + rendered.omitted_bytes, source.len());
}

#[test]
fn line_alignment_does_not_discard_most_of_a_long_partial_line() {
    let mut capture = Capture::new(capture_bytes_per_stream(2, crate::output::MODEL_BYTE_LIMIT));
    let (head_limit, tail_limit) = (capture.head_limit(), capture.tail_limit());
    let mut source = Vec::with_capacity(head_limit + tail_limit + 17);
    source.push(b'\n');
    source.extend(std::iter::repeat_n(b'h', head_limit - 1));
    source.extend(std::iter::repeat_n(b'x', 17));
    source.extend(std::iter::repeat_n(b't', tail_limit - 2));
    source.extend_from_slice(b"\nz");
    capture.push(&source);

    let rendered = capture.render(capture.retained());

    assert_eq!(rendered.shown_bytes, capture.retained());
    assert_eq!(rendered.omitted_bytes, 17);
    assert!(rendered.text.starts_with("\nhhhh"));
    assert!(rendered.text.contains("tttt"));
    assert!(rendered.text.ends_with("\nz"));
}
