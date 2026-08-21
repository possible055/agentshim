import json
import re
import statistics
from collections import defaultdict
from dataclasses import asdict

from ..runner.sampler import SampleResult

_WARP_SCENARIO_RE = re.compile(r"^(?P<round_key>.+_r\d+_round\d+)_q\d+$")
_WARP_ROUND_KEY_RE = re.compile(r"^(warp_grep_storm_.+_r\d+_round\d+)$")


def _percentile(values: list[float], frac: float) -> float:
    if not values:
        return 0.0
    sorted_v = sorted(values)
    idx = max(0, min(len(sorted_v) - 1, int(len(sorted_v) * frac) - 1))
    return sorted_v[idx]


def _warp_round_key(scenario: str) -> str | None:
    match = _WARP_SCENARIO_RE.match(scenario)
    return match.group("round_key") if match else None


class ReportGenerator:
    def __init__(self, results: list[SampleResult]):
        self.results = results

    def to_jsonl(self) -> str:
        lines = [json.dumps(asdict(result)) for result in self.results]
        return "\n".join(lines) + "\n"

    def generate_markdown_summary(self) -> str:
        grouped: dict[tuple[str, str, str, int, str], list[SampleResult]] = defaultdict(list)
        warp_results: list[SampleResult] = []
        for result in self.results:
            if result.is_warmup:
                continue
            if result.status != "ok":
                continue
            round_key = _warp_round_key(result.scenario)
            if round_key and result.concurrency > 1:
                warp_results.append(result)
                continue
            key = (
                result.scale,
                result.tool,
                result.scenario,
                result.concurrency,
                result.target,
            )
            grouped[key].append(result)

        scales = sorted({key[0] for key in grouped})
        tools = sorted({key[1] for key in grouped})
        scenarios = sorted({key[2] for key in grouped})
        concurrencies = sorted({key[3] for key in grouped})
        targets = sorted({result.target for result in self.results})

        md_sections = [
            "# Ranking Benchmark: read / grep / glob / bash\n",
            "Warm single-call latency is ranked only for samples that passed the same-work gate.",
            (
                "Concurrent throughput is retained for diagnosis "
                "and is sensitive to admission and shared runtimes.\n"
            ),
        ]

        md_sections.append("## 1. Ranked Single-Call Latency (Warm p50 / p95 ms)\n")
        header = (
            "| Scale | Tool | Scenario | "
            + " | ".join(f"{target} (p50 / p95)" for target in targets)
            + " |"
        )
        sep = "|---|---|---|" + "|".join(["---:"] * len(targets)) + "|"
        rows = [header, sep]

        for scale in scales:
            for tool in tools:
                for scenario in scenarios:
                    row_parts = [scale, tool, scenario]
                    has_row = False
                    for target in targets:
                        samples = grouped.get((scale, tool, scenario, 1, target), [])
                        if samples:
                            has_row = True
                            durations = [sample.duration_ms for sample in samples]
                            p50 = statistics.median(durations)
                            p95 = _percentile(durations, 0.95)
                            row_parts.append(f"{p50:.2f} / {p95:.2f}")
                        else:
                            row_parts.append("-")
                    if has_row:
                        rows.append("| " + " | ".join(row_parts) + " |")

        md_sections.append("\n".join(rows) + "\n")

        multi_concs = [concurrency for concurrency in concurrencies if concurrency > 1]
        if multi_concs:
            md_sections.append(
                "## 2. Concurrent Throughput (ops/s, sensitive to admission / shared runtime)\n"
            )
            for concurrency in multi_concs:
                md_sections.append(f"### Concurrency Level: {concurrency}\n")
                c_header = (
                    "| Scale | Tool | Scenario | "
                    + " | ".join(f"{target} (ops/s)" for target in targets)
                    + " |"
                )
                c_sep = "|---|---|---|" + "|".join(["---:"] * len(targets)) + "|"
                c_rows = [c_header, c_sep]
                for scale in scales:
                    for tool in tools:
                        for scenario in scenarios:
                            c_row_parts = [scale, tool, scenario]
                            has_row = False
                            for target in targets:
                                samples = grouped.get(
                                    (scale, tool, scenario, concurrency, target), []
                                )
                                if samples:
                                    has_row = True
                                    avg_dur_s = (
                                        sum(sample.duration_ms for sample in samples) / len(samples)
                                    ) / 1000.0
                                    ops_s = concurrency / avg_dur_s if avg_dur_s > 0 else 0.0
                                    c_row_parts.append(f"{ops_s:.2f}")
                                else:
                                    c_row_parts.append("-")
                            if has_row:
                                c_rows.append("| " + " | ".join(c_row_parts) + " |")
                md_sections.append("\n".join(c_rows) + "\n")

        if warp_results:
            md_sections.append(
                "## 3. Warp Grep Storm (parallel x8, round-level wall time / success rate)\n"
            )
            warp_grouped: dict[tuple[str, str, str], list[SampleResult]] = defaultdict(list)
            for result in warp_results:
                round_key = _warp_round_key(result.scenario)
                if round_key is None:
                    continue
                warp_grouped[(result.scale, round_key, result.target)].append(result)

            warp_round_keys = sorted({key[1] for key in warp_grouped})
            warp_targets = sorted({key[2] for key in warp_grouped})
            w_header = (
                "| Scale | Round | "
                + " | ".join(f"{target} (wall ms / ok)" for target in warp_targets)
                + " |"
            )
            w_sep = "|---|---|" + "|".join(["---:"] * len(warp_targets)) + "|"
            w_rows = [w_header, w_sep]
            for round_key in warp_round_keys:
                round_scales = sorted({key[0] for key in warp_grouped if key[1] == round_key})
                for scale in round_scales:
                    w_parts = [scale, round_key]
                    has_row = False
                    for target in warp_targets:
                        samples = warp_grouped.get((scale, round_key, target), [])
                        if samples:
                            has_row = True
                            wall_ms = samples[0].duration_ms
                            ok_count = sum(1 for s in samples if s.success)
                            total = len(samples)
                            w_parts.append(f"{wall_ms:.2f} / {ok_count}/{total}")
                        else:
                            w_parts.append("-")
                    if has_row:
                        w_rows.append("| " + " | ".join(w_parts) + " |")
            md_sections.append("\n".join(w_rows) + "\n")

        md_sections.append("## 4. Resource Usage (Peak Working Set MiB / CPU Time ms)\n")
        res_header = (
            "| Scale | Tool | Scenario | "
            + " | ".join(f"{target} (Mem / CPU)" for target in targets)
            + " |"
        )
        res_sep = "|---|---|---|" + "|".join(["---:"] * len(targets)) + "|"
        res_rows = [res_header, res_sep]

        for scale in scales:
            for tool in tools:
                for scenario in scenarios:
                    r_parts = [scale, tool, scenario]
                    has_row = False
                    for target in targets:
                        samples = grouped.get((scale, tool, scenario, 1, target), [])
                        res_deltas = [
                            sample.resource_delta for sample in samples if sample.resource_delta
                        ]
                        if res_deltas:
                            has_row = True
                            peak_ws_mib = max(
                                delta.get("peak_working_set_bytes", 0) for delta in res_deltas
                            ) / (1024 * 1024)
                            avg_cpu_ms = sum(
                                delta.get("delta_cpu_ms", 0.0) for delta in res_deltas
                            ) / len(res_deltas)
                            r_parts.append(f"{peak_ws_mib:.1f} MiB / {avg_cpu_ms:.1f}ms")
                        else:
                            r_parts.append("-")
                    if has_row:
                        res_rows.append("| " + " | ".join(r_parts) + " |")

        md_sections.append("\n".join(res_rows) + "\n")

        skipped = [
            result
            for result in self.results
            if result.status in {"unsupported", "skipped"} and not result.is_warmup
        ]
        if skipped:
            md_sections.append("## 5. Unsupported and Skipped\n")
            skip_rows = [
                "| Target | Tool | Scenario | Status | Reason |",
                "|---|---|---|---|---|",
            ]
            seen: set[tuple[str, str, str, str]] = set()
            warp_collapsed: dict[tuple[str, str, str, str], int] = defaultdict(int)
            for result in skipped:
                is_warp = _warp_round_key(result.scenario) is not None
                if not is_warp:
                    is_warp = _WARP_ROUND_KEY_RE.match(result.scenario) is not None
                if is_warp:
                    collapse_key = (
                        result.target,
                        result.tool,
                        str(result.status),
                        result.error or "",
                    )
                    warp_collapsed[collapse_key] += 1
                    continue
                skip_key = (
                    result.target,
                    result.tool,
                    result.scenario,
                    str(result.status),
                )
                if skip_key in seen:
                    continue
                seen.add(skip_key)
                reason = (result.error or "").replace("|", "\\|")
                skip_rows.append(
                    f"| {result.target} | {result.tool} | {result.scenario} "
                    f"| {result.status} | {reason} |"
                )
            for (target, tool, status, error), count in sorted(
                warp_collapsed.items(), key=lambda item: (item[0][0], item[0][1], item[0][2])
            ):
                reason = error.replace("|", "\\|")
                skip_rows.append(
                    f"| {target} | {tool} | warp_grep_storm ({count} rounds) "
                    f"| {status} | {reason} |"
                )
            md_sections.append("\n".join(skip_rows) + "\n")

        return "\n".join(md_sections)
