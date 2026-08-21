import shutil
from pathlib import Path
from typing import Any

from .mcp import PI_TOOLS, ExternalCommandAdapter


class PiAdapter(ExternalCommandAdapter):
    def __init__(self, name: str = "pi", config: dict[str, Any] | None = None):
        server_script = Path(__file__).parent / "pi_server.mjs"
        node = shutil.which("node") or "node"
        if server_script.exists():
            default_cmd = [node, str(server_script)]
        else:
            default_cmd = ["pi", "mcp"]
        super().__init__(name, config, default_command=default_cmd, tools=PI_TOOLS)
