from abc import ABC, abstractmethod
from dataclasses import dataclass


@dataclass
class ResourceSnapshot:
    working_set_bytes: int = 0
    peak_working_set_bytes: int = 0
    private_bytes: int = 0
    peak_private_bytes: int = 0
    page_faults: int = 0
    handles: int = 0
    threads: int = 0
    cpu_user_ms: float = 0.0
    cpu_kernel_ms: float = 0.0
    read_operations: int = 0
    write_operations: int = 0
    read_bytes: int = 0
    write_bytes: int = 0
    other_operations: int = 0
    other_bytes: int = 0


@dataclass
class ResourceDelta:
    peak_working_set_bytes: int = 0
    peak_private_bytes: int = 0
    peak_handles: int = 0
    peak_threads: int = 0
    delta_page_faults: int = 0
    delta_cpu_ms: float = 0.0
    delta_io_read_bytes: int = 0
    delta_io_write_bytes: int = 0
    delta_io_other_bytes: int = 0
    delta_io_operations: int = 0


class BaseMonitor(ABC):
    @abstractmethod
    def start_sampling(self, root_pids: set[int], interval_ms: int = 10) -> None:
        """Starts background periodic sampling for the given process trees."""

    @abstractmethod
    def stop_sampling(self) -> ResourceDelta:
        """Stops sampling and computes peak/delta resource usage."""

    @abstractmethod
    def snapshot_now(self, root_pids: set[int]) -> ResourceSnapshot:
        """Takes an immediate snapshot of the process trees."""

    @abstractmethod
    def find_process_tree(self, root_pid: int) -> set[int]:
        """Finds all descendant PIDs under the given root."""
