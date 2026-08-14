import os
import shutil
import subprocess
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ..protocol import McpClient
from .base import SkipTargetError, TargetAdapter, UnsupportedError

_STDERR_TAIL_BYTES = 8192


class McpStdioAdapter(TargetAdapter):
    def __init__(self, name: str, config: dict[str, Any] | None = None):
        super().__init__(name, config)
        self.process: subprocess.Popen[bytes] | None = None
        self.client: McpClient | None = None
        self._stderr_thread: threading.Thread | None = None
        self._stderr_tail = bytearray()
        self._stderr_lock = threading.Lock()

    def build_command(self) -> list[str]:
        raise NotImplementedError

    def working_directory(self) -> str | None:
        return None

    def process_env(self) -> dict[str, str]:
        return os.environ.copy()

    def client_name(self) -> str:
        return f"{self.name}-benchmark"

    def initialize_timeout_s(self) -> float:
        return 60.0

    def start(self) -> None:
        if self.process is not None:
            return

        self.process = subprocess.Popen(
            self.build_command(),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=self.working_directory(),
            env=self.process_env(),
            bufsize=0,
        )
        self._start_stderr_drain()
        try:
            self.client = McpClient(self.process)
            self.client.initialize(self.client_name(), timeout_s=self.initialize_timeout_s())
        except Exception as error:
            preview = self.stderr_preview()
            self.stop()
            if preview:
                raise RuntimeError(
                    f"{self.name} failed to start: {error}\nstderr:\n{preview}"
                ) from error
            raise

    def stop(self) -> None:
        if self.client:
            self.client.close()
            self.client = None
        if self.process:
            if self.process.poll() is None:
                try:
                    self.process.terminate()
                    self.process.wait(timeout=2.0)
                except Exception:
                    self.process.kill()
            self.process = None
        if self._stderr_thread:
            self._stderr_thread.join(timeout=1.0)
            self._stderr_thread = None

    def get_root_pids(self) -> set[int]:
        if self.process and self.process.poll() is None:
            return {self.process.pid}
        return set()

    def require_client(self) -> McpClient:
        if not self.client:
            raise RuntimeError(f"{self.name} target is not running")
        return self.client

    def stderr_preview(self) -> str:
        with self._stderr_lock:
            return bytes(self._stderr_tail).decode("utf-8", errors="replace")

    def _start_stderr_drain(self) -> None:
        if self.process is None or self.process.stderr is None:
            return
        stderr = self.process.stderr

        def _drain() -> None:
            try:
                while True:
                    chunk = stderr.read(4096)
                    if not chunk:
                        break
                    with self._stderr_lock:
                        self._stderr_tail.extend(chunk)
                        overflow = len(self._stderr_tail) - _STDERR_TAIL_BYTES
                        if overflow > 0:
                            del self._stderr_tail[:overflow]
            except Exception:
                return

        self._stderr_thread = threading.Thread(target=_drain, daemon=True)
        self._stderr_thread.start()


class CargoMcpAdapter(McpStdioAdapter):
    def __init__(self, name: str, config: dict[str, Any] | None = None):
        super().__init__(name, config)
        configured_root = self.config.get("root_dir")
        self.root_dir = Path(configured_root) if configured_root else Path.cwd()
        self.binary_path = self.config.get("binary_path")
        self._uses_cargo_fallback = False

    def working_directory(self) -> str:
        return str(self.root_dir)

    def process_env(self) -> dict[str, str]:
        env = super().process_env()
        if "RUST_LOG" not in env:
            env["RUST_LOG"] = "warn"
        return env

    def initialize_timeout_s(self) -> float:
        return 300.0 if self._uses_cargo_fallback else 60.0

    def _resolved_binary(self) -> Path | None:
        if self.binary_path:
            candidate = Path(self.binary_path)
            return candidate if candidate.exists() else None
        release_dir = self.root_dir / "target" / "release"
        for filename in (self.name, f"{self.name}.exe"):
            candidate = release_dir / filename
            if candidate.exists():
                return candidate
        return None

    def build_command(self) -> list[str]:
        resolved = self._resolved_binary()
        if resolved is not None:
            self._uses_cargo_fallback = False
            return [str(resolved), "serve"]
        if self.name != "codexshim":
            raise SkipTargetError(
                f"{self.name} binary not found under {self.root_dir / 'target' / 'release'}"
            )
        self._uses_cargo_fallback = True
        return ["cargo", "run", "--release", "--locked", "--", "serve"]


@dataclass(frozen=True)
class ExternalToolMap:
    read_tool: str
    grep_tool: str
    glob_tool: str
    bash_tool: str
    read_path: str = "path"
    read_start_line: str | None = "start_line"
    read_line_count: str | None = "line_count"
    grep_path: str = "path"
    grep_pattern: str = "pattern"
    grep_glob: str | None = "glob"
    glob_path: str = "path"
    bash_timeout: str | None = "timeout_ms"


class ExternalCommandAdapter(McpStdioAdapter):
    def __init__(
        self,
        name: str,
        config: dict[str, Any] | None,
        *,
        default_command: list[str],
        tools: ExternalToolMap,
    ):
        super().__init__(name, config)
        self.default_command = default_command
        self.tools = tools

    def build_command(self) -> list[str]:
        command = self.config.get("command", self.default_command)
        parts = [str(part) for part in command] if isinstance(command, list) else [str(command)]
        if parts and shutil.which(parts[0]) is None:
            raise SkipTargetError(f"{self.name} command not found on PATH: {parts[0]}")
        return parts

    def invoke_read(
        self,
        path: str,
        start_line: int | None = None,
        line_count: int | None = None,
        pdf_mode: str | None = None,
        pages: str | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        if pdf_mode is not None or pages is not None:
            raise UnsupportedError(f"{self.name} does not implement PDF read")
        args: dict[str, Any] = {self.tools.read_path: path}
        if start_line is not None and self.tools.read_start_line:
            args[self.tools.read_start_line] = start_line
        if line_count is not None and self.tools.read_line_count:
            args[self.tools.read_line_count] = line_count
        return self.require_client().call_tool(self.tools.read_tool, args, timeout_s=timeout_s)

    def invoke_grep(
        self,
        path: str,
        pattern: str,
        glob: str | None = None,
        mode: str = "content",
        case: str = "smart",
        fixed_strings: bool = False,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {
            self.tools.grep_path: path,
            self.tools.grep_pattern: pattern,
        }
        if glob is not None and self.tools.grep_glob:
            args[self.tools.grep_glob] = glob
        return self.require_client().call_tool(self.tools.grep_tool, args, timeout_s=timeout_s)

    def invoke_glob(
        self,
        path: str,
        pattern: str = "**/*",
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        args = {self.tools.glob_path: path, "pattern": pattern}
        return self.require_client().call_tool(self.tools.glob_tool, args, timeout_s=timeout_s)

    def invoke_bash(
        self,
        command: str,
        timeout_ms: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {"command": command}
        if timeout_ms is not None and self.tools.bash_timeout:
            args[self.tools.bash_timeout] = timeout_ms
        return self.require_client().call_tool(self.tools.bash_tool, args, timeout_s=timeout_s)


PI_TOOLS = ExternalToolMap(
    read_tool="read",
    grep_tool="grep",
    glob_tool="glob",
    bash_tool="bash",
)

OPENCODE_TOOLS = ExternalToolMap(
    read_tool="read_file",
    grep_tool="grep_search",
    glob_tool="find_files",
    bash_tool="execute_command",
    read_path="filePath",
    read_start_line="offset",
    read_line_count="limit",
    grep_path="directory",
    grep_pattern="query",
    grep_glob="pattern",
    glob_path="directory",
    bash_timeout="timeout",
)
