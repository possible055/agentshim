import os
import threading
import time
from dataclasses import dataclass
from pathlib import Path

from .base import BaseMonitor, ResourceDelta, ResourceSnapshot


@dataclass(frozen=True)
class ProcStat:
    pid: int
    ppid: int
    utime: int
    stime: int
    threads: int
    rss_pages: int


def parse_proc_stat(text: str) -> ProcStat:
    open_paren = text.find("(")
    close_paren = text.rfind(")")
    if open_paren < 0 or close_paren < 0 or close_paren <= open_paren:
        raise ValueError("invalid /proc/<pid>/stat: missing comm parentheses")

    pid = int(text[:open_paren].strip())
    remainder = text[close_paren + 1 :].split()
    # After comm: state ppid pgrp session tty_nr tpgid flags minflt cminflt
    # majflt cmajflt utime stime cutime cstime priority nice num_threads
    # itrealvalue starttime vsize rss ...
    if len(remainder) < 22:
        raise ValueError("invalid /proc/<pid>/stat: truncated field list")
    return ProcStat(
        pid=pid,
        ppid=int(remainder[1]),
        utime=int(remainder[11]),
        stime=int(remainder[12]),
        threads=int(remainder[17]),
        rss_pages=int(remainder[21]),
    )


def _page_size() -> int:
    if hasattr(os, "sysconf"):
        try:
            return int(os.sysconf("SC_PAGESIZE"))
        except (ValueError, OSError, TypeError):
            pass
    return 4096


class LinuxMonitor(BaseMonitor):
    def __init__(self) -> None:
        self._sampling = False
        self._sample_thread: threading.Thread | None = None
        self._samples: list[ResourceSnapshot] = []
        self._initial_snapshot: ResourceSnapshot | None = None
        self._monitored_roots: set[int] = set()

    def find_process_tree(self, root_pid: int) -> set[int]:
        result = {root_pid}
        frontier = [root_pid]
        proc = Path("/proc")
        if not proc.exists():
            return result

        parent_map: dict[int, int] = {}
        for entry in proc.iterdir():
            if not entry.name.isdigit():
                continue
            try:
                stat_file = entry / "stat"
                if stat_file.exists():
                    parsed = parse_proc_stat(stat_file.read_text())
                    parent_map[parsed.pid] = parsed.ppid
            except Exception:
                continue

        while frontier:
            curr = frontier.pop()
            for pid, ppid in parent_map.items():
                if ppid == curr and pid not in result:
                    result.add(pid)
                    frontier.append(pid)
        return result

    def snapshot_now(self, root_pids: set[int]) -> ResourceSnapshot:
        all_pids: set[int] = set()
        for root in root_pids:
            all_pids.update(self.find_process_tree(root))

        agg = ResourceSnapshot()
        clock_ticks = os.sysconf("SC_CLK_TCK") if hasattr(os, "sysconf") else 100
        page_size = _page_size()

        for pid in all_pids:
            proc_dir = Path(f"/proc/{pid}")
            if not proc_dir.exists():
                continue
            try:
                stat_file = proc_dir / "stat"
                if stat_file.exists():
                    parsed = parse_proc_stat(stat_file.read_text())
                    agg.cpu_user_ms += (parsed.utime / clock_ticks) * 1000.0
                    agg.cpu_kernel_ms += (parsed.stime / clock_ticks) * 1000.0
                    agg.threads += parsed.threads
                    working_set = parsed.rss_pages * page_size
                    agg.working_set_bytes += working_set
                    agg.peak_working_set_bytes += working_set

                status_file = proc_dir / "status"
                if status_file.exists():
                    for line in status_file.read_text().splitlines():
                        if line.startswith("FDSize:"):
                            agg.handles += int(line.split()[1])
                        elif line.startswith("VmPeak:"):
                            agg.peak_private_bytes += int(line.split()[1]) * 1024
                        elif line.startswith("VmSize:"):
                            agg.private_bytes += int(line.split()[1]) * 1024

                io_file = proc_dir / "io"
                if io_file.exists():
                    for line in io_file.read_text().splitlines():
                        if line.startswith("read_bytes:"):
                            agg.read_bytes += int(line.split()[1])
                        elif line.startswith("write_bytes:"):
                            agg.write_bytes += int(line.split()[1])
                        elif line.startswith("syscr:"):
                            agg.read_operations += int(line.split()[1])
                        elif line.startswith("syscw:"):
                            agg.write_operations += int(line.split()[1])
            except Exception:
                continue

        return agg

    def start_sampling(self, root_pids: set[int], interval_ms: int = 10) -> None:
        self._monitored_roots = set(root_pids)
        self._samples.clear()
        self._initial_snapshot = self.snapshot_now(self._monitored_roots)
        self._samples.append(self._initial_snapshot)
        self._sampling = True

        def _worker() -> None:
            interval_s = interval_ms / 1000.0
            while self._sampling:
                time.sleep(interval_s)
                if not self._sampling:
                    break
                try:
                    snap = self.snapshot_now(self._monitored_roots)
                    self._samples.append(snap)
                except Exception:
                    pass

        self._sample_thread = threading.Thread(target=_worker, daemon=True)
        self._sample_thread.start()

    def stop_sampling(self) -> ResourceDelta:
        self._sampling = False
        if self._sample_thread:
            self._sample_thread.join(timeout=2.0)
            self._sample_thread = None

        final_snapshot = self.snapshot_now(self._monitored_roots)
        self._samples.append(final_snapshot)

        peak_ws = max((s.working_set_bytes for s in self._samples), default=0)
        peak_priv = max((s.private_bytes for s in self._samples), default=0)
        peak_handles = max((s.handles for s in self._samples), default=0)
        peak_threads = max((s.threads for s in self._samples), default=0)

        init = self._initial_snapshot or ResourceSnapshot()
        cpu_delta = (final_snapshot.cpu_user_ms + final_snapshot.cpu_kernel_ms) - (
            init.cpu_user_ms + init.cpu_kernel_ms
        )
        io_read_delta = final_snapshot.read_bytes - init.read_bytes
        io_write_delta = final_snapshot.write_bytes - init.write_bytes
        io_ops_delta = (final_snapshot.read_operations + final_snapshot.write_operations) - (
            init.read_operations + init.write_operations
        )

        return ResourceDelta(
            peak_working_set_bytes=peak_ws,
            peak_private_bytes=peak_priv,
            peak_handles=peak_handles,
            peak_threads=peak_threads,
            delta_page_faults=0,
            delta_cpu_ms=max(0.0, cpu_delta),
            delta_io_read_bytes=max(0, io_read_delta),
            delta_io_write_bytes=max(0, io_write_delta),
            delta_io_other_bytes=0,
            delta_io_operations=max(0, io_ops_delta),
        )
