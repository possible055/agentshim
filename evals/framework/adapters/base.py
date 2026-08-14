from abc import ABC, abstractmethod
from typing import Any


class UnsupportedError(RuntimeError):
    """The target does not implement this tool or workload."""


class SkipTargetError(RuntimeError):
    """The target cannot be started for this run and should be skipped."""


class TargetAdapter(ABC):
    def __init__(self, name: str, config: dict[str, Any] | None = None):
        self.name = name
        self.config = config or {}

    @abstractmethod
    def start(self) -> None:
        """Starts the server/runtime process."""

    @abstractmethod
    def stop(self) -> None:
        """Gracefully stops and terminates all associated processes."""

    @abstractmethod
    def get_root_pids(self) -> set[int]:
        """Returns the root PID(s) of this target for resource tracking."""

    def supports_pdf_read(self) -> bool:
        return False

    def supports_tool(self, tool: str) -> bool:
        return tool != "run_program"

    def invoke_run_program(
        self,
        program: str,
        args: list[str] | None = None,
        timeout_ms: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        raise UnsupportedError(f"{self.name} does not implement run_program")

    @abstractmethod
    def invoke_read(
        self,
        path: str,
        start_line: int | None = None,
        line_count: int | None = None,
        pdf_mode: str | None = None,
        pages: str | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        """Invokes the file read tool."""

    @abstractmethod
    def invoke_grep(
        self,
        path: str,
        pattern: str,
        glob: str | None = None,
        mode: str = "content",
        case: str = "smart",
        fixed_strings: bool = False,
        limit: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        """Invokes the file search/grep tool."""

    @abstractmethod
    def invoke_glob(
        self,
        path: str,
        pattern: str = "**/*",
        limit: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        """Invokes the file pattern matching/find tool."""

    @abstractmethod
    def invoke_bash(
        self,
        command: str,
        timeout_ms: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        """Invokes the shell/command execution tool."""
