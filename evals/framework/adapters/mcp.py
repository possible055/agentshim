import os
import shutil
import subprocess
import threading
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any

from ..process_snapshot import pids_named
from ..protocol import McpClient
from .base import SkipTargetError, TargetAdapter, UnsupportedError

_STDERR_TAIL_BYTES = 8192
_READ_ALIASES = ("read", "inspect_local_file", "read_file")
_GREP_ALIASES = ("grep", "grep_search")
_GLOB_ALIASES = ("glob", "find_files")
_BASH_ALIASES = ("bash", "run", "execute_command")
_READ_PATH_KEYS = ("path", "filePath", "file_path")
_READ_START_KEYS = ("start_line", "offset")
_READ_COUNT_KEYS = ("line_count", "limit")
_GREP_PATH_KEYS = ("path", "directory")
_GREP_PATTERN_KEYS = ("pattern", "query")
_GREP_GLOB_KEYS = ("glob", "include", "filePattern")
_GREP_LIMIT_KEYS = ("limit", "head_limit", "max_results")
_GREP_SUMMARY_KEYS = ("summary_only", "summary")
_GLOB_PATH_KEYS = ("path", "directory")
_GLOB_LIMIT_KEYS = ("limit", "head_limit", "max_results")
_BASH_TIMEOUT_KEYS = ("timeout_ms", "timeout")


@dataclass
class ExternalToolMap:
    read_tool: str | None = None
    grep_tool: str | None = None
    glob_tool: str | None = None
    bash_tool: str | None = None
    read_path: str = "path"
    read_start_line: str | None = "start_line"
    read_line_count: str | None = "line_count"
    grep_path: str = "path"
    grep_pattern: str = "pattern"
    grep_glob: str | None = "glob"
    grep_mode: str | None = "mode"
    grep_limit: str | None = "limit"
    grep_summary: str | None = None
    glob_path: str = "path"
    glob_limit: str | None = "limit"
    bash_timeout: str | None = "timeout_ms"
    missing: set[str] = field(default_factory=set)


PI_TOOLS = ExternalToolMap(
    read_tool="read",
    grep_tool="grep",
    glob_tool="glob",
    bash_tool="bash",
)

OPENCODE_TOOLS = ExternalToolMap(
    read_tool="read",
    grep_tool="grep",
    glob_tool="glob",
    bash_tool="bash",
    read_start_line="offset",
    read_line_count="limit",
)


def _schema_properties(tool: dict[str, Any]) -> set[str]:
    schema = tool.get("inputSchema") or tool.get("input_schema") or {}
    if not isinstance(schema, dict):
        return set()
    properties = schema.get("properties")
    if not isinstance(properties, dict):
        return set()
    return {str(key) for key in properties}


def _first_present(properties: set[str], candidates: tuple[str, ...]) -> str | None:
    for candidate in candidates:
        if candidate in properties:
            return candidate
    return None


def _pick_tool(
    listed: dict[str, dict[str, Any]], aliases: tuple[str, ...]
) -> dict[str, Any] | None:
    for alias in aliases:
        if alias in listed:
            return listed[alias]
    return None


def bind_tools(listed_tools: list[dict[str, Any]], fallback: ExternalToolMap) -> ExternalToolMap:
    listed = {
        str(tool.get("name")): tool for tool in listed_tools if isinstance(tool.get("name"), str)
    }
    bound = replace(fallback, missing=set())
    missing: set[str] = set()

    read_tool = _pick_tool(listed, _READ_ALIASES)
    if read_tool is None:
        missing.add("read")
        bound.read_tool = None
    else:
        properties = _schema_properties(read_tool)
        bound.read_tool = str(read_tool["name"])
        bound.read_path = _first_present(properties, _READ_PATH_KEYS) or fallback.read_path
        bound.read_start_line = _first_present(properties, _READ_START_KEYS)
        bound.read_line_count = _first_present(properties, _READ_COUNT_KEYS)

    grep_tool = _pick_tool(listed, _GREP_ALIASES)
    if grep_tool is None:
        missing.add("grep")
        bound.grep_tool = None
    else:
        properties = _schema_properties(grep_tool)
        bound.grep_tool = str(grep_tool["name"])
        bound.grep_path = _first_present(properties, _GREP_PATH_KEYS) or fallback.grep_path
        bound.grep_pattern = _first_present(properties, _GREP_PATTERN_KEYS) or fallback.grep_pattern
        bound.grep_glob = _first_present(properties, _GREP_GLOB_KEYS)
        bound.grep_mode = _first_present(properties, ("mode",))
        bound.grep_limit = _first_present(properties, _GREP_LIMIT_KEYS)
        bound.grep_summary = _first_present(properties, _GREP_SUMMARY_KEYS)

    glob_tool = _pick_tool(listed, _GLOB_ALIASES)
    if glob_tool is None:
        missing.add("glob")
        bound.glob_tool = None
    else:
        properties = _schema_properties(glob_tool)
        bound.glob_tool = str(glob_tool["name"])
        bound.glob_path = _first_present(properties, _GLOB_PATH_KEYS) or fallback.glob_path
        bound.glob_limit = _first_present(properties, _GLOB_LIMIT_KEYS)

    bash_tool = _pick_tool(listed, _BASH_ALIASES)
    if bash_tool is None:
        missing.add("bash")
        bound.bash_tool = None
    else:
        properties = _schema_properties(bash_tool)
        bound.bash_tool = str(bash_tool["name"])
        bound.bash_timeout = _first_present(properties, _BASH_TIMEOUT_KEYS)

    bound.missing = missing
    return bound


class McpStdioAdapter(TargetAdapter):
    def __init__(self, name: str, config: dict[str, Any] | None = None):
        super().__init__(name, config)
        self.process: subprocess.Popen[bytes] | None = None
        self.client: McpClient | None = None
        self.tools = ExternalToolMap()
        self._stderr_thread: threading.Thread | None = None
        self._stderr_tail = bytearray()
        self._stderr_lock = threading.Lock()
        self._extra_root_pids: set[int] = set()

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

    def fallback_tools(self) -> ExternalToolMap:
        return ExternalToolMap()

    def extra_process_names(self) -> set[str]:
        return set()

    def start(self) -> None:
        if self.process is not None:
            return

        extra_names = self.extra_process_names()
        before_pids = pids_named(extra_names) if extra_names else set()
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
            listed = self.client.list_tools(timeout_s=self.initialize_timeout_s())
            self.tools = bind_tools(listed, self.fallback_tools())
        except Exception as error:
            preview = self.stderr_preview()
            self.stop()
            if preview:
                raise RuntimeError(
                    f"{self.name} failed to start: {error}\nstderr:\n{preview}"
                ) from error
            raise
        if extra_names:
            after_pids = pids_named(extra_names)
            self._extra_root_pids = after_pids - before_pids

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
        self._extra_root_pids.clear()

    def get_root_pids(self) -> set[int]:
        pids: set[int] = set()
        if self.process and self.process.poll() is None:
            pids.add(self.process.pid)
        pids.update(self._extra_root_pids)
        return pids

    def require_client(self) -> McpClient:
        if not self.client:
            raise RuntimeError(f"{self.name} target is not running")
        return self.client

    def supports_tool(self, tool: str) -> bool:
        if tool == "read":
            return self.tools.read_tool is not None
        if tool == "grep":
            return self.tools.grep_tool is not None
        if tool == "glob":
            return self.tools.glob_tool is not None
        if tool == "bash":
            return self.tools.bash_tool is not None
        return True

    def unsupported_reason(self, tool: str) -> str:
        return f"{self.name} has no {tool} tool after tools/list"

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
        if not self.tools.read_tool:
            raise UnsupportedError(self.unsupported_reason("read"))
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
        limit: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        if not self.tools.grep_tool:
            raise UnsupportedError(self.unsupported_reason("grep"))
        if mode in {"count", "files"} and not self.tools.grep_mode and not self.tools.grep_summary:
            raise UnsupportedError(f"{self.name} cannot express full-scan grep mode={mode}")
        args: dict[str, Any] = {
            self.tools.grep_path: path,
            self.tools.grep_pattern: pattern,
        }
        if glob is not None and self.tools.grep_glob:
            args[self.tools.grep_glob] = glob
        if self.tools.grep_mode:
            args[self.tools.grep_mode] = mode
        if mode in {"count", "files"} and self.tools.grep_summary:
            args[self.tools.grep_summary] = True
        if limit is not None and self.tools.grep_limit:
            args[self.tools.grep_limit] = limit
        if case != "smart":
            args.setdefault("case", case)
        if fixed_strings:
            args.setdefault("fixed_strings", True)
        return self.require_client().call_tool(self.tools.grep_tool, args, timeout_s=timeout_s)

    def invoke_glob(
        self,
        path: str,
        pattern: str = "**/*",
        limit: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        if not self.tools.glob_tool:
            raise UnsupportedError(self.unsupported_reason("glob"))
        args: dict[str, Any] = {self.tools.glob_path: path, "pattern": pattern}
        if limit is not None and self.tools.glob_limit:
            args[self.tools.glob_limit] = limit
        return self.require_client().call_tool(self.tools.glob_tool, args, timeout_s=timeout_s)

    def invoke_bash(
        self,
        command: str,
        timeout_ms: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        if not self.tools.bash_tool:
            raise UnsupportedError(self.unsupported_reason("bash"))
        args: dict[str, Any] = {"command": command}
        if timeout_ms is not None and self.tools.bash_timeout:
            args[self.tools.bash_timeout] = timeout_ms
        return self.require_client().call_tool(self.tools.bash_tool, args, timeout_s=timeout_s)


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
        # Ranking measures tool work, not a tight client burst window.
        env.setdefault("CODEXSHIM_BURST_TOKENS", "8192")
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
        self._fallback_tools = tools

    def fallback_tools(self) -> ExternalToolMap:
        return self._fallback_tools

    def build_command(self) -> list[str]:
        command = self.config.get("command", self.default_command)
        parts = [str(part) for part in command] if isinstance(command, list) else [str(command)]
        if parts and shutil.which(parts[0]) is None:
            raise SkipTargetError(f"{self.name} command not found on PATH: {parts[0]}")
        return parts
