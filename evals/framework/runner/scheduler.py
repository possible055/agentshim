import json
import sys
import time
from collections.abc import Callable
from functools import partial
from pathlib import Path
from typing import Any

from evals.datasets.codebase.generator import load_expected

from ..adapters import SkipTargetError, TargetAdapter, get_adapter
from ..gates import evaluate_gate
from ..monitor import BaseMonitor, create_resource_monitor
from .sampler import (
    SampleResult,
    run_concurrent_calls,
    run_single_call,
    skipped_sample,
    unsupported_sample,
)

DATASETS_ROOT = Path(__file__).resolve().parents[2] / "datasets"
CODEBASE_SCENARIOS = DATASETS_ROOT / "codebase" / "scenarios.json"
COMMAND_SCENARIOS = DATASETS_ROOT / "commands" / "scenarios.json"
RUN_PROGRAM_SCENARIOS = DATASETS_ROOT / "run_program" / "scenarios.json"
PDF_MANIFEST = DATASETS_ROOT / "pdf" / "manifest.json"
RANKING_BURST_QUIET_S = 2.1


def _load_json_scenarios(path: Path) -> list[dict[str, Any]]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    scenarios = payload.get("scenarios", [])
    if not isinstance(scenarios, list):
        raise ValueError(f"{path} does not contain a scenarios array")
    return [scenario for scenario in scenarios if isinstance(scenario, dict)]


def _first_source_file(corpus_dir: Path, expected: dict[str, Any]) -> str:
    read_expected = expected.get("read")
    if isinstance(read_expected, dict):
        relative = read_expected.get("path")
        if isinstance(relative, str) and relative:
            candidate = corpus_dir / relative
            if candidate.is_file():
                return str(candidate)
    if corpus_dir.exists():
        rust_file = next((str(path) for path in corpus_dir.rglob("*.rs") if path.is_file()), None)
        if rust_file:
            return rust_file
        any_file = next((str(path) for path in corpus_dir.rglob("*") if path.is_file()), None)
        if any_file:
            return any_file
    return str(Path("Cargo.toml").resolve())


def _invoke_scenario(
    adapter: TargetAdapter,
    scenario: dict[str, Any],
    corpus_path: str,
    test_file: str,
    expected: dict[str, Any],
) -> dict[str, Any]:
    tool = str(scenario["tool"])
    if tool == "read":
        result = adapter.invoke_read(
            str(scenario.get("path", test_file)),
            start_line=scenario.get("start_line"),
            line_count=scenario.get("line_count"),
            pdf_mode=scenario.get("pdf_mode"),
            pages=scenario.get("pages"),
        )
    elif tool == "grep":
        result = adapter.invoke_grep(
            corpus_path,
            str(scenario["pattern"]),
            glob=scenario.get("glob"),
            mode=str(scenario.get("mode", "content")),
            case=str(scenario.get("case", "smart")),
            fixed_strings=bool(scenario.get("fixed_strings", False)),
            limit=scenario.get("limit"),
        )
    elif tool == "glob":
        result = adapter.invoke_glob(
            corpus_path,
            str(scenario.get("pattern", "**/*")),
            limit=scenario.get("limit"),
        )
    elif tool == "bash":
        result = adapter.invoke_bash(
            str(scenario["command"]),
            timeout_ms=scenario.get("timeout_ms"),
        )
    elif tool == "run_program":
        result = adapter.invoke_run_program(
            str(scenario["program"]),
            args=list(scenario.get("args") or []),
            timeout_ms=scenario.get("timeout_ms"),
        )
    else:
        raise ValueError(f"Unsupported tool '{tool}'")

    gate = scenario.get("gate")
    if isinstance(gate, str) and gate:
        evaluate_gate(gate, result.get("response"), expected, Path(corpus_path))
    return result


class BenchmarkRunner:
    def __init__(
        self,
        targets: list[str],
        suite: str = "compare",
        workloads: list[str] | None = None,
        scales: list[str] | None = None,
        warm_iterations: int = 7,
        concurrency_levels: list[int] | None = None,
        target_configs: dict[str, Any] | None = None,
        pdf_root: Path | None = None,
        long_pdf_limit: int = 2,
    ):
        self.target_names = targets
        self.suite = suite
        self.workloads = workloads or ["read", "grep", "glob"]
        self.scales = scales or ["1k"]
        self.warm_iterations = warm_iterations
        self.concurrency_levels = concurrency_levels or [1, 4]
        self.target_configs = target_configs or {}
        self.pdf_root = pdf_root
        self.long_pdf_limit = long_pdf_limit

        self.monitor: BaseMonitor = create_resource_monitor()
        self.adapters: dict[str, TargetAdapter] = {}
        self.skip_reasons: dict[str, str] = {}

    def setup(self) -> None:
        for target_name in self.target_names:
            config = self.target_configs.get(target_name, {})
            adapter = get_adapter(target_name, config)
            print(f"Starting target '{target_name}'...", file=sys.stderr)
            try:
                adapter.start()
            except SkipTargetError as error:
                reason = str(error)
                print(f"Skipping target '{target_name}': {reason}", file=sys.stderr)
                self.skip_reasons[target_name] = reason
                continue
            except Exception as error:
                reason = f"failed to start: {error}"
                print(f"Skipping target '{target_name}': {reason}", file=sys.stderr)
                self.skip_reasons[target_name] = reason
                adapter.stop()
                continue
            self.adapters[target_name] = adapter

    def teardown(self) -> None:
        for target_name, adapter in self.adapters.items():
            print(f"Stopping target '{target_name}'...", file=sys.stderr)
            adapter.stop()
        self.adapters.clear()

    def _compare_scenarios(self, tool: str) -> list[dict[str, Any]]:
        if tool == "bash":
            scenarios = _load_json_scenarios(COMMAND_SCENARIOS)
            return [{**scenario, "tool": "bash"} for scenario in scenarios]
        scenarios = _load_json_scenarios(CODEBASE_SCENARIOS)
        return [scenario for scenario in scenarios if scenario.get("tool") == tool]

    def _pdf_scenarios(self) -> list[dict[str, Any]]:
        if self.pdf_root is None:
            raise ValueError("pdf suite requires pdf_root")
        manifest = json.loads(PDF_MANIFEST.read_text(encoding="utf-8"))
        documents = manifest.get("documents", [])
        scenarios: list[dict[str, Any]] = []
        long_count = 0
        for document in documents:
            if not isinstance(document, dict):
                continue
            category = str(document.get("category", ""))
            if category == "ocr":
                continue
            if category == "long":
                if long_count >= self.long_pdf_limit:
                    continue
                long_count += 1
                pages = "1-2"
            else:
                pages = "1"
            destination = self.pdf_root / str(document["category"]) / str(document["filename"])
            scenarios.append(
                {
                    "id": str(document["id"]),
                    "tool": "read",
                    "path": str(destination),
                    "pdf_mode": "text",
                    "pages": pages,
                    "description": str(document.get("title", document["id"])),
                }
            )
        return scenarios

    def _run_program_scenarios(self) -> list[dict[str, Any]]:
        scenarios = _load_json_scenarios(RUN_PROGRAM_SCENARIOS)
        return [{**scenario, "tool": "run_program"} for scenario in scenarios]

    def _work_items(
        self, corpus_paths: dict[str, Path]
    ) -> list[tuple[str, str, dict[str, Any], str, str, dict[str, Any]]]:
        items: list[tuple[str, str, dict[str, Any], str, str, dict[str, Any]]] = []
        if self.suite == "pdf":
            for scenario in self._pdf_scenarios():
                items.append(
                    ("pdf", "read", scenario, str(self.pdf_root), str(scenario["path"]), {})
                )
            return items
        if self.suite == "run-program":
            for scenario in self._run_program_scenarios():
                items.append(("n/a", "run_program", scenario, ".", ".", {}))
            return items

        for scale in self.scales:
            corpus_dir = corpus_paths.get(scale, Path("."))
            expected = load_expected(corpus_dir)
            corpus_path = str(corpus_dir)
            test_file = _first_source_file(corpus_dir, expected)
            for tool in self.workloads:
                for scenario in self._compare_scenarios(tool):
                    items.append((scale, tool, scenario, corpus_path, test_file, expected))
        return items

    def _record_unsupported(
        self,
        target_name: str,
        tool: str,
        scenario: dict[str, Any],
        scale: str,
        reason: str,
        on_sample: Callable[[SampleResult], None] | None,
    ) -> SampleResult:
        result = unsupported_sample(
            target_name=target_name,
            tool_name=tool,
            scenario=str(scenario.get("id", "default")),
            scale=scale,
            reason=reason,
        )
        if on_sample:
            on_sample(result)
        return result

    def _record_skip(
        self,
        target_name: str,
        tool: str,
        scenario: dict[str, Any],
        scale: str,
        reason: str,
        on_sample: Callable[[SampleResult], None] | None,
    ) -> SampleResult:
        result = skipped_sample(
            target_name=target_name,
            tool_name=tool,
            scenario=str(scenario.get("id", "default")),
            scale=scale,
            reason=reason,
        )
        if on_sample:
            on_sample(result)
        return result

    def _eligible_targets(self, tool: str) -> list[str]:
        names = [name for name in self.target_names if name in self.adapters]
        if self.suite == "run-program":
            return [name for name in names if name == "codexshim"]
        if self.suite == "pdf":
            return [name for name in names if self.adapters[name].supports_pdf_read()]
        return [name for name in names if self.adapters[name].supports_tool(tool)]

    def run_benchmark(
        self,
        corpus_paths: dict[str, Path],
        on_sample: Callable[[SampleResult], None] | None = None,
    ) -> list[SampleResult]:
        all_results: list[SampleResult] = []
        try:
            self.setup()
            work_items = self._work_items(corpus_paths)
            for scale, tool, scenario, corpus_path, test_file, expected in work_items:
                scenario_id = str(scenario.get("id", "default"))
                print(f"  > {tool}/{scenario_id} (scale={scale})", file=sys.stderr)
                eligible = set(self._eligible_targets(tool))

                for target_name in self.target_names:
                    if target_name in self.skip_reasons:
                        all_results.append(
                            self._record_skip(
                                target_name,
                                tool,
                                scenario,
                                scale,
                                self.skip_reasons[target_name],
                                on_sample,
                            )
                        )
                        continue
                    if target_name not in self.adapters:
                        continue
                    if target_name not in eligible:
                        reason = (
                            "run_program is measured only on codexshim"
                            if self.suite == "run-program"
                            else f"{target_name} does not implement PDF read"
                            if self.suite == "pdf"
                            else f"{target_name} has no {tool} tool after tools/list"
                        )
                        all_results.append(
                            self._record_unsupported(
                                target_name,
                                tool,
                                scenario,
                                scale,
                                reason,
                                on_sample,
                            )
                        )

                iterations_total = 1 + self.warm_iterations
                for iter_idx in range(iterations_total):
                    is_warm = iter_idx > 0
                    ordered_targets = [name for name in self.target_names if name in eligible]
                    if iter_idx % 2 == 1:
                        ordered_targets.reverse()

                    for target_name in ordered_targets:
                        adapter = self.adapters[target_name]
                        if self.suite == "compare":
                            time.sleep(RANKING_BURST_QUIET_S)
                        call_fn = partial(
                            _invoke_scenario,
                            adapter,
                            scenario,
                            corpus_path,
                            test_file,
                            expected,
                        )
                        result = run_single_call(
                            adapter=adapter,
                            monitor=self.monitor,
                            call_fn=call_fn,
                            target_name=target_name,
                            tool_name=tool,
                            scenario=scenario_id,
                            scale=scale,
                            iteration=iter_idx,
                            is_warmup=not is_warm,
                        )
                        all_results.append(result)
                        if on_sample:
                            on_sample(result)

                for concurrency in self.concurrency_levels:
                    if concurrency <= 1:
                        continue
                    for target_name in self.target_names:
                        if target_name not in eligible:
                            continue
                        adapter = self.adapters[target_name]
                        if self.suite == "compare":
                            time.sleep(RANKING_BURST_QUIET_S)
                        call_fn = partial(
                            _invoke_scenario,
                            adapter,
                            scenario,
                            corpus_path,
                            test_file,
                            expected,
                        )
                        result = run_concurrent_calls(
                            adapter=adapter,
                            monitor=self.monitor,
                            call_fn=call_fn,
                            target_name=target_name,
                            tool_name=tool,
                            scenario=scenario_id,
                            scale=scale,
                            concurrency=concurrency,
                            iteration=1,
                            is_warmup=False,
                        )
                        all_results.append(result)
                        if on_sample:
                            on_sample(result)
        finally:
            self.teardown()
        return all_results
