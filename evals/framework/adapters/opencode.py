import shutil
from pathlib import Path
from typing import Any

from .mcp import OPENCODE_TOOLS, ExternalCommandAdapter


class OpencodeAdapter(ExternalCommandAdapter):
    def __init__(self, name: str = "opencode", config: dict[str, Any] | None = None):
        server_script = Path(__file__).parent / "opencode_server.ts"
        project_root = Path(__file__).resolve().parents[3]
        bun_candidates = [
            project_root
            / "local"
            / "perf"
            / "runtime"
            / "node_modules"
            / "@oven"
            / "bun-windows-x64"
            / "bin"
            / "bun.exe",
            shutil.which("bun"),
        ]
        bun_path = next((str(p) for p in bun_candidates if p and Path(p).exists()), "bun")
        if server_script.exists():
            default_cmd = [bun_path, "run", str(server_script)]
        else:
            default_cmd = ["opencode", "mcp"]
        super().__init__(name, config, default_command=default_cmd, tools=OPENCODE_TOOLS)
