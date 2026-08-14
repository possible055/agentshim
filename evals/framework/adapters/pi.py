from typing import Any

from .mcp import PI_TOOLS, ExternalCommandAdapter


class PiAdapter(ExternalCommandAdapter):
    def __init__(self, name: str = "pi", config: dict[str, Any] | None = None):
        super().__init__(name, config, default_command=["pi", "mcp"], tools=PI_TOOLS)
