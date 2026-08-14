import json
import statistics
from collections import defaultdict
from dataclasses import asdict

from ..runner.sampler import SampleResult


def _percentile(values: list[float], frac: float) -> float:
    if not values:
        return 0.0
    sorted_v = sorted(values)
    idx = max(0, min(len(sorted_v) - 1, int(len(sorted_v) * frac) - 1))
    return sorted_v[idx]


class ReportGenerator:
    def __init__(self, results: list[SampleResult]):
        self.results = results

    def to_jsonl(self) -> str:
        lines = [json.dumps(asdict(result)) for result in self.results]
        return "\n".join(lines) + "\n"

    def generate_markdown_summary(self) -> str:
        grouped: dict[tuple[str, str, str, int, str], list[SampleResult]] = defaultdict(list)
        for result in self.results:
            if not result.is_warmup and result.status == "ok":
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

        md_sections = ["# Multi-Target Benchmark Comparison Summary\n"]

        md_sections.append("## 1. Single-Call Latency (Warm p50 / p95 ms)\n")
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
            md_sections.append("## 2. Concurrent Throughput (ops/s)\n")
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

        md_sections.append("## 3. Resource Usage (Peak Working Set MiB / CPU Time ms)\n")
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
            md_sections.append("## 4. Unsupported and Skipped\n")
            skip_rows = [
                "| Target | Tool | Scenario | Status | Reason |",
                "|---|---|---|---|---|",
            ]
            seen: set[tuple[str, str, str, str]] = set()
            for result in skipped:
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
            md_sections.append("\n".join(skip_rows) + "\n")

        return "\n".join(md_sections)
