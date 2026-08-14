import re
import shutil
import subprocess
import time
from pathlib import Path
from typing import Any

from .base import TargetAdapter, UnsupportedError


class BaselineCliAdapter(TargetAdapter):
    def __init__(self, name: str = "baseline_cli", config: dict[str, Any] | None = None):
        super().__init__(name, config)
        self.rg_path = self.config.get("rg_path") or shutil.which("rg") or shutil.which("rg.exe")

    def start(self) -> None:
        return

    def stop(self) -> None:
        return

    def get_root_pids(self) -> set[int]:
        return set()

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
            raise UnsupportedError("baseline_cli does not implement PDF read")
        start = time.perf_counter()
        with open(path, encoding="utf-8", errors="replace") as handle:
            lines = handle.readlines()
        offset = (start_line - 1) if (start_line and start_line > 0) else 0
        limit = line_count if line_count is not None else len(lines)
        sliced = lines[offset : offset + limit]
        duration_ms = (time.perf_counter() - start) * 1000.0
        return {"response": {"text": "".join(sliced)}, "duration_ms": duration_ms}

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
        start = time.perf_counter()

        if self.rg_path:
            cmd = [self.rg_path]
            if mode == "files":
                cmd.append("--files-with-matches")
            elif mode == "count":
                cmd.append("--count-matches")

            if case == "insensitive":
                cmd.append("-i")
            elif case == "sensitive":
                cmd.append("-s")
            else:
                cmd.append("-S")

            if fixed_strings:
                cmd.append("-F")

            if glob:
                cmd.extend(["-g", glob])

            cmd.extend([pattern, path])

            res = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=timeout_s,
                check=False,
            )
            duration_ms = (time.perf_counter() - start) * 1000.0
            return {
                "response": {"stdout": res.stdout, "exit_code": res.returncode},
                "duration_ms": duration_ms,
            }

        flags = 0
        if case == "insensitive" or (case == "smart" and pattern.islower()):
            flags |= re.IGNORECASE
        regex = re.compile(re.escape(pattern) if fixed_strings else pattern, flags)

        matches = []
        root_p = Path(path)
        files = list(root_p.rglob(glob or "*")) if root_p.is_dir() else [root_p]
        for candidate in files:
            if candidate.is_file():
                try:
                    content = candidate.read_text(encoding="utf-8", errors="ignore")
                    for idx, line in enumerate(content.splitlines(), 1):
                        if regex.search(line):
                            matches.append(f"{candidate}:{idx}:{line}")
                            if mode == "files":
                                break
                except Exception:
                    continue
        duration_ms = (time.perf_counter() - start) * 1000.0
        return {
            "response": {"stdout": "\n".join(matches), "count": len(matches)},
            "duration_ms": duration_ms,
        }

    def invoke_glob(
        self,
        path: str,
        pattern: str = "**/*",
        limit: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        start = time.perf_counter()
        if self.rg_path:
            cmd = [self.rg_path, "--files", path]
            if pattern and pattern != "**/*":
                cmd.extend(["-g", pattern])
            res = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=timeout_s,
                check=False,
            )
            duration_ms = (time.perf_counter() - start) * 1000.0
            stdout = res.stdout
            if limit is not None:
                stdout = "\n".join(stdout.splitlines()[:limit])
            return {
                "response": {"stdout": stdout, "exit_code": res.returncode},
                "duration_ms": duration_ms,
            }

        root_p = Path(path)
        found = [str(candidate) for candidate in root_p.glob(pattern) if candidate.is_file()]
        if limit is not None:
            found = found[:limit]
        duration_ms = (time.perf_counter() - start) * 1000.0
        return {
            "response": {"stdout": "\n".join(found), "count": len(found)},
            "duration_ms": duration_ms,
        }

    def invoke_bash(
        self,
        command: str,
        timeout_ms: int | None = None,
        timeout_s: float = 60.0,
    ) -> dict[str, Any]:
        t_limit = (timeout_ms / 1000.0) if timeout_ms else timeout_s
        start = time.perf_counter()
        res = subprocess.run(
            ["bash", "-c", command] if shutil.which("bash") else ["cmd.exe", "/c", command],
            capture_output=True,
            text=True,
            timeout=t_limit,
            check=False,
        )
        duration_ms = (time.perf_counter() - start) * 1000.0
        return {
            "response": {"stdout": res.stdout, "exit_code": res.returncode},
            "duration_ms": duration_ms,
        }
