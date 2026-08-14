from evals.framework.monitor.linux import parse_proc_stat


def test_parse_proc_stat_without_spaces_in_comm() -> None:
    text = "42 (bash) S 1 42 42 0 -1 4194304 100 0 0 0 12 34 0 0 20 0 1 0 12345 123456 77 0"
    parsed = parse_proc_stat(text)
    assert parsed.pid == 42
    assert parsed.ppid == 1
    assert parsed.utime == 12
    assert parsed.stime == 34
    assert parsed.threads == 1
    assert parsed.rss_pages == 77


def test_parse_proc_stat_with_spaces_in_comm() -> None:
    text = "99 ( codex shim ) S 7 99 99 0 -1 4194304 100 0 0 0 50 60 0 0 20 0 4 0 12345 123456 88 0"
    parsed = parse_proc_stat(text)
    assert parsed.pid == 99
    assert parsed.ppid == 7
    assert parsed.utime == 50
    assert parsed.stime == 60
    assert parsed.threads == 4
    assert parsed.rss_pages == 88


if __name__ == "__main__":
    test_parse_proc_stat_without_spaces_in_comm()
    test_parse_proc_stat_with_spaces_in_comm()
    print("proc stat fixtures ok")
