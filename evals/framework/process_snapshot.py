import os
from pathlib import Path


def pids_named(names: set[str]) -> set[int]:
    wanted = {name.lower() for name in names}
    if os.name == "nt":
        return _windows_pids_named(wanted)
    return _posix_pids_named(wanted)


def _windows_pids_named(wanted: set[str]) -> set[int]:
    from .monitor.windows import WindowsMonitor

    table = WindowsMonitor().process_table()
    found: set[int] = set()
    for pid, (_parent_id, exe_file) in table.items():
        stem = Path(exe_file).stem.lower()
        names = {Path(name).stem.lower() for name in wanted}
        if Path(exe_file).name.lower() in wanted or stem in names:
            found.add(pid)
    return found


def _posix_pids_named(wanted: set[str]) -> set[int]:
    proc = Path("/proc")
    if not proc.exists():
        return set()
    found: set[int] = set()
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        comm_path = entry / "comm"
        try:
            comm = comm_path.read_text(encoding="utf-8", errors="replace").strip().lower()
        except OSError:
            continue
        if comm in wanted or f"{comm}.exe" in wanted:
            found.add(int(entry.name))
    return found
