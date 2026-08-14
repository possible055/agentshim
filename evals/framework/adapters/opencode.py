from typing import Any

from .mcp import OPENCODE_TOOLS, ExternalCommandAdapter


class OpencodeAdapter(ExternalCommandAdapter):
    def __init__(self, name: str = "opencode", config: dict[str, Any] | None = None):
        super().__init__(name, config, default_command=["opencode", "mcp"], tools=OPENCODE_TOOLS)
