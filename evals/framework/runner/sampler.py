import concurrent.futures
import time
from collections.abc import Callable
from dataclasses import asdict, dataclass
from typing import Any, Literal

from ..adapters.base import TargetAdapter, UnsupportedError
from ..monitor.base import BaseMonitor

SampleStatus = Literal["ok", "error", "unsupported", "skipped"]


@dataclass
class SampleResult:
    target: str
    tool: str
    scenario: str
    scale: str
    concurrency: int
    iteration: int
    is_warmup: bool
    duration_ms: float
    success: bool
    status: SampleStatus
    error: str | None
    resource_delta: dict[str, Any] | None
    output_bytes: int


def _status_for_error(error: BaseException) -> SampleStatus:
    if isinstance(error, UnsupportedError):
        return "unsupported"
    return "error"


def _terminal_sample(
    target_name: str,
    tool_name: str,
    scenario: str,
    scale: str,
    reason: str,
    status: SampleStatus,
    concurrency: int = 1,
    iteration: int = 0,
) -> SampleResult:
    return SampleResult(
        target=target_name,
        tool=tool_name,
        scenario=scenario,
        scale=scale,
        concurrency=concurrency,
        iteration=iteration,
        is_warmup=False,
        duration_ms=0.0,
        success=False,
        status=status,
        error=reason,
        resource_delta=None,
        output_bytes=0,
    )


def skipped_sample(
    target_name: str,
    tool_name: str,
    scenario: str,
    scale: str,
    reason: str,
    concurrency: int = 1,
    iteration: int = 0,
) -> SampleResult:
    return _terminal_sample(
        target_name,
        tool_name,
        scenario,
        scale,
        reason,
        "skipped",
        concurrency=concurrency,
        iteration=iteration,
    )


def unsupported_sample(
    target_name: str,
    tool_name: str,
    scenario: str,
    scale: str,
    reason: str,
    concurrency: int = 1,
    iteration: int = 0,
) -> SampleResult:
    return _terminal_sample(
        target_name,
        tool_name,
        scenario,
        scale,
        reason,
        "unsupported",
        concurrency=concurrency,
        iteration=iteration,
    )


def run_single_call(
    adapter: TargetAdapter,
    monitor: BaseMonitor,
    call_fn: Callable[[], dict[str, Any]],
    target_name: str,
    tool_name: str,
    scenario: str,
    scale: str,
    iteration: int,
    is_warmup: bool,
    monitor_interval_ms: int = 10,
) -> SampleResult:
    root_pids = adapter.get_root_pids()
    if root_pids:
        monitor.start_sampling(root_pids, interval_ms=monitor_interval_ms)

    start_time = time.perf_counter()
    success = True
    status: SampleStatus = "ok"
    error_msg = None
    output_bytes = 0

    try:
        res = call_fn()
        duration_ms = res.get("duration_ms", (time.perf_counter() - start_time) * 1000.0)
        resp_obj = res.get("response", {})
        output_bytes = len(str(resp_obj).encode("utf-8"))
    except Exception as ex:
        duration_ms = (time.perf_counter() - start_time) * 1000.0
        success = False
        status = _status_for_error(ex)
        error_msg = str(ex)

    resource_delta_dict = None
    if root_pids:
        delta = monitor.stop_sampling()
        resource_delta_dict = asdict(delta)

    return SampleResult(
        target=target_name,
        tool=tool_name,
        scenario=scenario,
        scale=scale,
        concurrency=1,
        iteration=iteration,
        is_warmup=is_warmup,
        duration_ms=duration_ms,
        success=success,
        status=status,
        error=error_msg,
        resource_delta=resource_delta_dict,
        output_bytes=output_bytes,
    )


def run_concurrent_calls(
    adapter: TargetAdapter,
    monitor: BaseMonitor,
    call_fn: Callable[[], dict[str, Any]],
    target_name: str,
    tool_name: str,
    scenario: str,
    scale: str,
    concurrency: int,
    iteration: int,
    is_warmup: bool,
    monitor_interval_ms: int = 10,
) -> SampleResult:
    root_pids = adapter.get_root_pids()
    if root_pids:
        monitor.start_sampling(root_pids, interval_ms=monitor_interval_ms)

    start_time = time.perf_counter()
    success = True
    status: SampleStatus = "ok"
    error_msg = None
    output_bytes = 0

    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
        futures = [executor.submit(call_fn) for _ in range(concurrency)]
        for future in concurrent.futures.as_completed(futures):
            try:
                res = future.result()
                resp_obj = res.get("response", {})
                output_bytes += len(str(resp_obj).encode("utf-8"))
            except Exception as ex:
                success = False
                status = _status_for_error(ex)
                error_msg = str(ex)

    duration_ms = (time.perf_counter() - start_time) * 1000.0

    resource_delta_dict = None
    if root_pids:
        delta = monitor.stop_sampling()
        resource_delta_dict = asdict(delta)

    return SampleResult(
        target=target_name,
        tool=tool_name,
        scenario=scenario,
        scale=scale,
        concurrency=concurrency,
        iteration=iteration,
        is_warmup=is_warmup,
        duration_ms=duration_ms,
        success=success,
        status=status,
        error=error_msg,
        resource_delta=resource_delta_dict,
        output_bytes=output_bytes,
    )
