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
    run_parallel_different_calls,
    run_single_call,
    skipped_sample,
    unsupported_sample,
)

DATASETS_ROOT = Path(__file__).resolve().parents[2] / "datasets"
CODEBASE_SCENARIOS = DATASETS_ROOT / "codebase" / "scenarios.json"
COMMAND_SCENARIOS = DATASETS_ROOT / "commands" / "scenarios.json"
RUN_PROGRAM_SCENARIOS = DATASETS_ROOT / "run_program" / "scenarios.json"
PDF_MANIFEST = DATASETS_ROOT / "pdf" / "manifest.json"
MACRO_SCENARIOS = DATASETS_ROOT / "macro" / "scenarios.json"
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


def _resolve_macro_repo_path(repo_name: str) -> Path:
    project_root = Path(__file__).resolve().parents[3]
    candidates = [
        project_root / "local" / "perf" / "repos" / repo_name,
        project_root / "repos" / repo_name,
    ]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    raise FileNotFoundError(
        f"macro repo '{repo_name}' not found under local/perf/repos/ or repos/. "
        f"Clone it first: git clone <url> repos/{repo_name}"
    )


def _invoke_scenario(
    adapter: TargetAdapter,
    scenario: dict[str, Any],
    corpus_path: str,
    test_file: str,
    expected: dict[str, Any],
) -> dict[str, Any]:
    tool = str(scenario["tool"])
    if tool == "read":
        target_path = str(scenario.get("path", test_file))
        if (
            "target_key" in scenario
            and isinstance(expected, dict)
            and scenario["target_key"] in expected
        ):
            rel = expected[scenario["target_key"]].get("path")
            if rel:
                target_path = str(Path(corpus_path) / rel)
        elif "path" in scenario:
            p = Path(scenario["path"])
            target_path = str(Path(corpus_path) / p) if not p.is_absolute() else str(p)
        result = adapter.invoke_read(
            target_path,
            start_line=scenario.get("start_line"),
            line_count=scenario.get("line_count"),
            pdf_mode=scenario.get("pdf_mode"),
            pages=scenario.get("pages"),
        )
    elif tool == "grep":
        grep_path = corpus_path
        if "path" in scenario:
            sub = Path(corpus_path) / scenario["path"]
            if sub.exists():
                grep_path = str(sub)
        result = adapter.invoke_grep(
            grep_path,
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
        burst_quiet_seconds: float | None = None,
        trajectories: list[str] | None = None,
    ):
        self.target_names = targets
        self.suite = suite
        self.workloads = workloads or ["read", "grep", "glob", "bash"]
        self.scales = scales or ["1k"]
        self.warm_iterations = warm_iterations
        self.concurrency_levels = concurrency_levels or [1, 4]
        self.target_configs = target_configs or {}
        self.pdf_root = pdf_root
        self.long_pdf_limit = long_pdf_limit
        self.trajectories = trajectories
        if burst_quiet_seconds is not None:
            self.burst_quiet_seconds = burst_quiet_seconds
        else:
            self.burst_quiet_seconds = 0.0 if suite in ("macro", "agentic") else 2.1

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
                adapter.stop()
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

    def _macro_scenarios(self) -> list[dict[str, Any]]:
        if not MACRO_SCENARIOS.is_file():
            return []
        payload = json.loads(MACRO_SCENARIOS.read_text(encoding="utf-8"))
        trajectories = payload.get("trajectories", [])
        scenarios: list[dict[str, Any]] = []
        for traj in trajectories:
            traj_id = traj.get("id")
            repo = traj.get("repository")
            if (
                self.trajectories
                and traj_id not in self.trajectories
                and repo not in self.trajectories
            ):
                continue
            traj_type = traj.get("type")
            if traj_type == "multi_turn_grep":
                for turn_info in traj.get("turns", []):
                    turn_num = turn_info.get("turn")
                    for q_idx, q in enumerate(turn_info.get("queries", [])):
                        scenarios.append(
                            {
                                "id": f"{traj_id}_t{turn_num}_q{q_idx + 1}",
                                "trajectory_id": traj_id,
                                "turn": turn_num,
                                "repository": repo,
                                "tool": "grep",
                                **q,
                            }
                        )
            elif traj_type == "warp_grep_storm":
                pool = traj.get("query_pool", [])
                q_per_round = int(traj.get("queries_per_round", 8))
                for round_count in traj.get("rounds_config", [4]):
                    for round_num in range(round_count):
                        round_key = f"{traj_id}_r{round_count}_round{round_num}"
                        for q_idx in range(q_per_round):
                            pool_idx = (round_num * q_per_round + q_idx) % len(pool) if pool else 0
                            q = pool[pool_idx] if pool else {}
                            scenarios.append(
                                {
                                    "id": f"{round_key}_q{q_idx + 1}",
                                    "trajectory_id": traj_id,
                                    "repository": repo,
                                    "tool": "grep",
                                    "warp_round_key": round_key,
                                    "warp_round_count": round_count,
                                    "warp_round_num": round_num,
                                    "warp_queries_per_round": q_per_round,
                                    **q,
                                }
                            )
            elif traj_type == "actual_bash":
                for cmd in traj.get("commands", []):
                    scenarios.append(
                        {
                            "id": f"{traj_id}_{cmd.get('id', 'default')}",
                            "trajectory_id": traj_id,
                            "repository": repo,
                            "tool": "bash",
                            "command": str(cmd.get("command", "")),
                            "description": str(cmd.get("description", "")),
                        }
                    )
            elif traj_type == "grep_to_read_chain":
                scenarios.append(
                    {
                        "id": f"{traj_id}_grep",
                        "trajectory_id": traj_id,
                        "repository": repo,
                        "tool": "grep",
                        **traj.get("grep_step", {}),
                    }
                )
                for r_idx, r in enumerate(traj.get("read_slices", [])):
                    scenarios.append(
                        {
                            "id": f"{traj_id}_slice_{r_idx + 1}",
                            "trajectory_id": traj_id,
                            "repository": repo,
                            "tool": "read",
                            **r,
                        }
                    )
            elif traj_type == "parallel_glob":
                for g_idx, g in enumerate(traj.get("globs", [])):
                    scenarios.append(
                        {
                            "id": f"{traj_id}_glob_{g_idx + 1}",
                            "trajectory_id": traj_id,
                            "repository": repo,
                            "tool": "glob",
                            **g,
                        }
                    )
        return scenarios

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
        if self.suite in ("macro", "agentic"):
            for scenario in self._macro_scenarios():
                repo_name = scenario.get("repository", "transformers")
                try:
                    repo_path = _resolve_macro_repo_path(repo_name)
                except FileNotFoundError as error:
                    print(f"Skipping macro scenario '{scenario['id']}': {error}", file=sys.stderr)
                    continue
                items.append((repo_name, scenario["tool"], scenario, str(repo_path), ".", {}))
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
            return [name for name in names if name == "agentshim"]
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

            warp_groups: dict[
                str, list[tuple[str, str, dict[str, Any], str, str, dict[str, Any]]]
            ] = {}
            for item in work_items:
                scenario = item[2]
                round_key = scenario.get("warp_round_key")
                if round_key:
                    warp_groups.setdefault(round_key, []).append(item)
            processed_round_keys: set[str] = set()

            for scale, tool, scenario, corpus_path, test_file, expected in work_items:
                round_key = scenario.get("warp_round_key")
                if round_key:
                    if round_key in processed_round_keys:
                        continue
                    processed_round_keys.add(round_key)
                    group = warp_groups[round_key]
                    round_count = scenario.get("warp_round_count", 0)
                    round_num = scenario.get("warp_round_num", 0)
                    q_per_round = scenario.get("warp_queries_per_round", 8)
                    scenario_id = (
                        f"{round_key} ({q_per_round}x parallel, "
                        f"round {round_num + 1}/{round_count})"
                    )
                    print(f"  > {tool}/{scenario_id} (scale={scale})", file=sys.stderr)
                    eligible = set(self._eligible_targets(tool))

                    for target_name in self.target_names:
                        if target_name in self.skip_reasons:
                            all_results.append(
                                self._record_skip(
                                    target_name,
                                    tool,
                                    {"id": round_key},
                                    scale,
                                    self.skip_reasons[target_name],
                                    on_sample,
                                )
                            )
                            continue
                        if target_name not in self.adapters:
                            continue
                        if target_name not in eligible:
                            reason = f"{target_name} has no {tool} tool after tools/list"
                            all_results.append(
                                self._record_unsupported(
                                    target_name,
                                    tool,
                                    {"id": round_key},
                                    scale,
                                    reason,
                                    on_sample,
                                )
                            )
                            continue

                        adapter = self.adapters[target_name]
                        if self.burst_quiet_seconds > 0:
                            time.sleep(self.burst_quiet_seconds)
                        call_fns = [
                            partial(
                                _invoke_scenario,
                                adapter,
                                gi[2],
                                gi[3],
                                gi[4],
                                gi[5],
                            )
                            for gi in group
                        ]
                        results = run_parallel_different_calls(
                            adapter=adapter,
                            monitor=self.monitor,
                            call_fns=call_fns,
                            target_name=target_name,
                            tool_name=tool,
                            scenario=round_key,
                            scale=scale,
                            iteration=round_num,
                            is_warmup=False,
                        )
                        all_results.extend(results)
                        if on_sample:
                            for result in results:
                                on_sample(result)
                    continue

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
                            "run_program is measured only on agentshim"
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
                        if self.burst_quiet_seconds > 0:
                            time.sleep(self.burst_quiet_seconds)
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
                        if self.burst_quiet_seconds > 0:
                            time.sleep(self.burst_quiet_seconds)
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
