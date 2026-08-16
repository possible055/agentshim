import argparse
import json
import sys
from dataclasses import asdict
from datetime import datetime
from pathlib import Path
from typing import Any

PROJECT_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(PROJECT_ROOT))

from evals.datasets.codebase.generator import generate_codebase  # noqa: E402
from evals.datasets.pdf.download import ensure_dataset  # noqa: E402
from evals.framework import BenchmarkRunner, ReportGenerator, SampleResult  # noqa: E402

COMPARE_TARGETS = ["agentshim", "fastctx", "pi", "opencode"]
SCALE_COUNTS = {"1k": 1000, "10k": 10000, "100k": 100000}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Agentshim evals: rank read/grep/glob against fastctx/pi/opencode, "
            "plus extra PDF read and a agentshim-only run_program suite. "
            "bash is available but not part of the ranking compare default."
        )
    )
    parser.add_argument(
        "--suite",
        choices=["compare", "pdf", "run-program"],
        default="compare",
        help="Which suite to run (default: compare)",
    )
    parser.add_argument(
        "--targets",
        nargs="+",
        default=COMPARE_TARGETS,
        help="Targets to evaluate (default: agentshim fastctx pi opencode)",
    )
    parser.add_argument(
        "--scales",
        nargs="+",
        default=["1k"],
        choices=["1k", "10k", "100k"],
        help="Compare-suite corpus scales (default: 1k)",
    )
    parser.add_argument(
        "--tools",
        nargs="+",
        default=["read", "grep", "glob"],
        help="Compare-suite tools (default: read grep glob). bash is optional and not ranked.",
    )
    parser.add_argument(
        "--warm",
        type=int,
        default=3,
        help="Warm iterations per scenario (default: 3)",
    )
    parser.add_argument(
        "--concurrency",
        nargs="+",
        type=int,
        default=[1, 4],
        help="Concurrency levels (default: 1 4)",
    )
    parser.add_argument(
        "--out",
        type=str,
        default=None,
        help="Output directory (default: local/perf/out)",
    )
    parser.add_argument(
        "--binary-path",
        type=str,
        default=None,
        help="Path to a prebuilt agentshim binary",
    )
    parser.add_argument(
        "--fastctx-root",
        type=str,
        default=None,
        help="fastctx checkout that contains target/release/fastctx",
    )
    parser.add_argument(
        "--fastctx-binary",
        type=str,
        default=None,
        help="Path to a prebuilt fastctx binary",
    )
    parser.add_argument(
        "--pdf-root",
        type=str,
        default=None,
        help="PDF corpus root (default: evals/datasets/pdf/corpus)",
    )
    parser.add_argument(
        "--long-pdf-limit",
        type=int,
        default=2,
        help="How many long PDFs to sample in the pdf suite (default: 2)",
    )
    return parser.parse_args()


def _target_configs(args: argparse.Namespace) -> dict[str, dict[str, Any]]:
    configs: dict[str, dict[str, Any]] = {
        "agentshim": {
            "root_dir": str(PROJECT_ROOT),
            "binary_path": args.binary_path,
        },
        "fastctx": {},
        "pi": {"command": ["pi", "mcp"]},
        "opencode": {"command": ["opencode", "mcp"]},
        "baseline_cli": {},
    }
    if args.fastctx_root:
        configs["fastctx"]["root_dir"] = args.fastctx_root
    if args.fastctx_binary:
        configs["fastctx"]["binary_path"] = args.fastctx_binary
    return configs


def _prepare_compare_corpus(scales: list[str]) -> dict[str, Path]:
    corpus_base = PROJECT_ROOT / "evals" / "datasets" / "codebase"
    corpus_paths: dict[str, Path] = {}
    for scale in scales:
        corpus_dir = corpus_base / f"codebase_{scale}"
        expected_path = corpus_dir / "expected.json"
        if not corpus_dir.exists() or not expected_path.is_file():
            print(f"Generating synthetic {scale} corpus at {corpus_dir}...", file=sys.stderr)
            generate_codebase(corpus_dir, SCALE_COUNTS[scale])
        corpus_paths[scale] = corpus_dir
    return corpus_paths


def main() -> None:
    args = parse_args()
    targets: list[str] = list(args.targets)
    scales: list[str] = list(args.scales)
    tools: list[str] = list(args.tools)
    warm: int = int(args.warm)
    concurrency: list[int] = [int(value) for value in args.concurrency]
    out_dir = Path(args.out) if args.out else PROJECT_ROOT / "local" / "perf" / "out"
    out_dir.mkdir(parents=True, exist_ok=True)

    corpus_paths: dict[str, Path] = {}
    pdf_root: Path | None = None
    if args.suite == "compare":
        corpus_paths = _prepare_compare_corpus(scales)
    elif args.suite == "pdf":
        requested_root = Path(args.pdf_root) if args.pdf_root else None
        pdf_root = ensure_dataset(root=requested_root)

    runner = BenchmarkRunner(
        targets=targets,
        suite=args.suite,
        workloads=tools,
        scales=scales,
        warm_iterations=warm,
        concurrency_levels=concurrency,
        target_configs=_target_configs(args),
        pdf_root=pdf_root,
        long_pdf_limit=int(args.long_pdf_limit),
    )

    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    jsonl_path = out_dir / f"benchmark-{args.suite}-{timestamp}.jsonl"
    md_path = out_dir / f"benchmark-{args.suite}-{timestamp}.md"
    print(f"\nStarting {args.suite} suite across targets: {targets}...", file=sys.stderr)

    with jsonl_path.open("w", encoding="utf-8") as jsonl_file:

        def _on_sample(sample: SampleResult) -> None:
            jsonl_file.write(json.dumps(asdict(sample)) + "\n")
            jsonl_file.flush()

        results = runner.run_benchmark(corpus_paths, on_sample=_on_sample)

    reporter = ReportGenerator(results)
    summary_md = reporter.generate_markdown_summary()
    md_path.write_text(summary_md, encoding="utf-8")

    print("\n" + summary_md)
    print(f"Saved traces to: {jsonl_path}", file=sys.stderr)
    print(f"Saved report to: {md_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
