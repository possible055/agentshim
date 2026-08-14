import os

from .base import BaseMonitor, ResourceDelta, ResourceSnapshot


def create_resource_monitor() -> BaseMonitor:
    if os.name == "nt":
        from .windows import WindowsMonitor

        return WindowsMonitor()
    else:
        from .linux import LinuxMonitor

        return LinuxMonitor()


__all__ = [
    "BaseMonitor",
    "ResourceSnapshot",
    "ResourceDelta",
    "create_resource_monitor",
]
