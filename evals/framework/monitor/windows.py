import ctypes
import threading
import time
from ctypes import wintypes

from .base import BaseMonitor, ResourceDelta, ResourceSnapshot


class FileTime(ctypes.Structure):
    _fields_ = [("low", wintypes.DWORD), ("high", wintypes.DWORD)]


class IoCounters(ctypes.Structure):
    _fields_ = [
        ("read_operations", ctypes.c_ulonglong),
        ("write_operations", ctypes.c_ulonglong),
        ("other_operations", ctypes.c_ulonglong),
        ("read_bytes", ctypes.c_ulonglong),
        ("write_bytes", ctypes.c_ulonglong),
        ("other_bytes", ctypes.c_ulonglong),
    ]


class ProcessMemoryCountersEx(ctypes.Structure):
    _fields_ = [
        ("cb", wintypes.DWORD),
        ("page_faults", wintypes.DWORD),
        ("peak_working_set_bytes", ctypes.c_size_t),
        ("working_set_bytes", ctypes.c_size_t),
        ("quota_peak_paged_pool_bytes", ctypes.c_size_t),
        ("quota_paged_pool_bytes", ctypes.c_size_t),
        ("quota_peak_nonpaged_pool_bytes", ctypes.c_size_t),
        ("quota_nonpaged_pool_bytes", ctypes.c_size_t),
        ("pagefile_bytes", ctypes.c_size_t),
        ("peak_pagefile_bytes", ctypes.c_size_t),
        ("private_bytes", ctypes.c_size_t),
    ]


class ProcessEntry32W(ctypes.Structure):
    _fields_ = [
        ("dwSize", wintypes.DWORD),
        ("cntUsage", wintypes.DWORD),
        ("process_id", wintypes.DWORD),
        ("default_heap_id", ctypes.c_size_t),
        ("module_id", wintypes.DWORD),
        ("threads", wintypes.DWORD),
        ("parent_process_id", wintypes.DWORD),
        ("base_priority", wintypes.LONG),
        ("flags", wintypes.DWORD),
        ("exe_file", wintypes.WCHAR * 260),
    ]


class ThreadEntry32(ctypes.Structure):
    _fields_ = [
        ("dwSize", wintypes.DWORD),
        ("cntUsage", wintypes.DWORD),
        ("thread_id", wintypes.DWORD),
        ("owner_process_id", wintypes.DWORD),
        ("base_priority", wintypes.LONG),
        ("delta_priority", wintypes.LONG),
        ("flags", wintypes.DWORD),
    ]


def _file_time_ms(ft: FileTime) -> float:
    val = (int(ft.high) << 32) | int(ft.low)
    return val / 10000.0  # 100ns units to ms


class WindowsMonitor(BaseMonitor):
    SNAP_PROCESS = 0x00000002
    SNAP_THREAD = 0x00000004
    PROCESS_QUERY_INFORMATION = 0x0400
    PROCESS_VM_READ = 0x0010
    INVALID_HANDLE = ctypes.c_void_p(-1).value

    def __init__(self) -> None:
        self.kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        self.psapi = ctypes.WinDLL("psapi", use_last_error=True)

        self.kernel32.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
        self.kernel32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
        self.kernel32.Process32FirstW.argtypes = [wintypes.HANDLE, ctypes.POINTER(ProcessEntry32W)]
        self.kernel32.Process32FirstW.restype = wintypes.BOOL
        self.kernel32.Process32NextW.argtypes = [wintypes.HANDLE, ctypes.POINTER(ProcessEntry32W)]
        self.kernel32.Process32NextW.restype = wintypes.BOOL
        self.kernel32.Thread32First.argtypes = [wintypes.HANDLE, ctypes.POINTER(ThreadEntry32)]
        self.kernel32.Thread32First.restype = wintypes.BOOL
        self.kernel32.Thread32Next.argtypes = [wintypes.HANDLE, ctypes.POINTER(ThreadEntry32)]
        self.kernel32.Thread32Next.restype = wintypes.BOOL
        self.kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        self.kernel32.OpenProcess.restype = wintypes.HANDLE
        self.kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        self.kernel32.CloseHandle.restype = wintypes.BOOL
        self.kernel32.GetProcessTimes.argtypes = [
            wintypes.HANDLE,
            ctypes.POINTER(FileTime),
            ctypes.POINTER(FileTime),
            ctypes.POINTER(FileTime),
            ctypes.POINTER(FileTime),
        ]
        self.kernel32.GetProcessTimes.restype = wintypes.BOOL
        self.kernel32.GetProcessIoCounters.argtypes = [wintypes.HANDLE, ctypes.POINTER(IoCounters)]
        self.kernel32.GetProcessIoCounters.restype = wintypes.BOOL
        self.kernel32.GetProcessHandleCount.argtypes = [
            wintypes.HANDLE,
            ctypes.POINTER(wintypes.DWORD),
        ]
        self.kernel32.GetProcessHandleCount.restype = wintypes.BOOL
        self.psapi.GetProcessMemoryInfo.argtypes = [
            wintypes.HANDLE,
            ctypes.POINTER(ProcessMemoryCountersEx),
            wintypes.DWORD,
        ]
        self.psapi.GetProcessMemoryInfo.restype = wintypes.BOOL

        self._sampling = False
        self._sample_thread: threading.Thread | None = None
        self._samples: list[ResourceSnapshot] = []
        self._initial_snapshot: ResourceSnapshot | None = None
        self._monitored_roots: set[int] = set()

    def process_table(self) -> dict[int, tuple[int, str]]:
        snapshot = self.kernel32.CreateToolhelp32Snapshot(self.SNAP_PROCESS, 0)
        if snapshot == self.INVALID_HANDLE:
            return {}
        try:
            entry = ProcessEntry32W()
            entry.dwSize = ctypes.sizeof(entry)
            table = {}
            available = self.kernel32.Process32FirstW(snapshot, ctypes.byref(entry))
            while available:
                table[int(entry.process_id)] = (int(entry.parent_process_id), entry.exe_file)
                entry.dwSize = ctypes.sizeof(entry)
                available = self.kernel32.Process32NextW(snapshot, ctypes.byref(entry))
            return table
        finally:
            self.kernel32.CloseHandle(snapshot)

    def thread_counts(self, pids: set[int]) -> dict[int, int]:
        snapshot = self.kernel32.CreateToolhelp32Snapshot(self.SNAP_THREAD, 0)
        if snapshot == self.INVALID_HANDLE:
            return {p: 0 for p in pids}
        counts = {p: 0 for p in pids}
        try:
            entry = ThreadEntry32()
            entry.dwSize = ctypes.sizeof(entry)
            available = self.kernel32.Thread32First(snapshot, ctypes.byref(entry))
            while available:
                owner = int(entry.owner_process_id)
                if owner in counts:
                    counts[owner] += 1
                entry.dwSize = ctypes.sizeof(entry)
                available = self.kernel32.Thread32Next(snapshot, ctypes.byref(entry))
            return counts
        finally:
            self.kernel32.CloseHandle(snapshot)

    def find_process_tree(self, root_pid: int) -> set[int]:
        table = self.process_table()
        result = {root_pid}
        frontier = [root_pid]
        while frontier:
            current = frontier.pop()
            for pid, (parent_id, _) in table.items():
                if parent_id == current and pid not in result:
                    result.add(pid)
                    frontier.append(pid)
        return result

    def snapshot_now(self, root_pids: set[int]) -> ResourceSnapshot:
        all_pids: set[int] = set()
        for root in root_pids:
            all_pids.update(self.find_process_tree(root))

        threads_map = self.thread_counts(all_pids)
        agg = ResourceSnapshot()

        for pid in all_pids:
            handle = self.kernel32.OpenProcess(
                self.PROCESS_QUERY_INFORMATION | self.PROCESS_VM_READ, False, pid
            )
            if not handle:
                continue
            try:
                mem = ProcessMemoryCountersEx()
                mem.cb = ctypes.sizeof(mem)
                if self.psapi.GetProcessMemoryInfo(handle, ctypes.byref(mem), mem.cb):
                    agg.working_set_bytes += int(mem.working_set_bytes)
                    agg.peak_working_set_bytes += int(mem.peak_working_set_bytes)
                    agg.private_bytes += int(mem.private_bytes)
                    agg.page_faults += int(mem.page_faults)

                created, exited, kernel, user = FileTime(), FileTime(), FileTime(), FileTime()
                if self.kernel32.GetProcessTimes(
                    handle,
                    ctypes.byref(created),
                    ctypes.byref(exited),
                    ctypes.byref(kernel),
                    ctypes.byref(user),
                ):
                    agg.cpu_kernel_ms += _file_time_ms(kernel)
                    agg.cpu_user_ms += _file_time_ms(user)

                io = IoCounters()
                if self.kernel32.GetProcessIoCounters(handle, ctypes.byref(io)):
                    agg.read_operations += int(io.read_operations)
                    agg.write_operations += int(io.write_operations)
                    agg.other_operations += int(io.other_operations)
                    agg.read_bytes += int(io.read_bytes)
                    agg.write_bytes += int(io.write_bytes)
                    agg.other_bytes += int(io.other_bytes)

                handles = wintypes.DWORD()
                if self.kernel32.GetProcessHandleCount(handle, ctypes.byref(handles)):
                    agg.handles += int(handles.value)

                agg.threads += threads_map.get(pid, 0)
            finally:
                self.kernel32.CloseHandle(handle)

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
        pf_delta = final_snapshot.page_faults - init.page_faults
        io_read_delta = final_snapshot.read_bytes - init.read_bytes
        io_write_delta = final_snapshot.write_bytes - init.write_bytes
        io_other_delta = final_snapshot.other_bytes - init.other_bytes
        io_ops_delta = (
            final_snapshot.read_operations
            + final_snapshot.write_operations
            + final_snapshot.other_operations
        ) - (init.read_operations + init.write_operations + init.other_operations)

        return ResourceDelta(
            peak_working_set_bytes=peak_ws,
            peak_private_bytes=peak_priv,
            peak_handles=peak_handles,
            peak_threads=peak_threads,
            delta_page_faults=max(0, pf_delta),
            delta_cpu_ms=max(0.0, cpu_delta),
            delta_io_read_bytes=max(0, io_read_delta),
            delta_io_write_bytes=max(0, io_write_delta),
            delta_io_other_bytes=max(0, io_other_delta),
            delta_io_operations=max(0, io_ops_delta),
        )
