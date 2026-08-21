from typing import Any

from .agentshim import AgentshimAdapter
from .base import SkipTargetError, TargetAdapter, UnsupportedError
from .baseline_cli import BaselineCliAdapter
from .dsh import DshAdapter
from .fastctx import FastctxAdapter
from .opencode import OpencodeAdapter
from .pi import PiAdapter

_ADAPTER_REGISTRY: dict[str, type[TargetAdapter]] = {
    "agentshim": AgentshimAdapter,
    "agentshim_dsh": DshAdapter,
    "dsh": DshAdapter,
    "fastctx": FastctxAdapter,
    "pi": PiAdapter,
    "opencode": OpencodeAdapter,
    "baseline_cli": BaselineCliAdapter,
}


def get_adapter(name: str, config: dict[str, Any] | None = None) -> TargetAdapter:
    cls = _ADAPTER_REGISTRY.get(name.lower())
    if not cls:
        raise ValueError(f"Unknown adapter '{name}'. Available: {list(_ADAPTER_REGISTRY.keys())}")
    return cls(name=name, config=config)


__all__ = [
    "TargetAdapter",
    "UnsupportedError",
    "SkipTargetError",
    "AgentshimAdapter",
    "DshAdapter",
    "FastctxAdapter",
    "PiAdapter",
    "OpencodeAdapter",
    "BaselineCliAdapter",
    "get_adapter",
]
