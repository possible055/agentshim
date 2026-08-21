import shutil
from pathlib import Path
from typing import Any

from .mcp import ExternalCommandAdapter, ExternalToolMap


class DshAdapter(ExternalCommandAdapter):
    def __init__(self, name: str = "dsh", config: dict[str, Any] | None = None):
        server_script = Path(__file__).parent / "dsh_server.mjs"
        node = shutil.which("node") or "node"
        default_cmd = [node, str(server_script)]
        tools = ExternalToolMap(
            read_tool="read",
            grep_tool="grep",
            glob_tool="glob",
            bash_tool="bash",
            read_start_line="start_line",
            read_line_count="line_count",
            grep_pattern="pattern",
            grep_glob="glob",
            grep_mode="mode",
            grep_limit="limit",
            glob_limit="limit",
        )
        super().__init__(name, config, default_command=default_cmd, tools=tools)
