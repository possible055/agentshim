import argparse
import json
import random
import shutil
from pathlib import Path
from typing import Any

EXTENSIONS = [".rs", ".py", ".ts", ".js", ".json", ".md", ".toml", ".txt"]
KEYWORDS_COMMON = [
    "fn main()",
    "import os",
    "export const",
    "async def",
    "impl Handler",
]
KEYWORD_SPARSE = "AGENTSHIM_BENCHMARK_SPARSE_TARGET"
KEYWORD_DENSE = "AGENTSHIM_BENCHMARK_DENSE_MATCH"
DENSE_FILE_TARGET = 20
DENSE_HITS_PER_FILE = 4
EXPECTED_FILENAME = "expected.json"
PREFIX_STEM_PREFIX = "file_00000"


def header_marker(index: int, total_files: int) -> str:
    return f"// Synthetic benchmark file {index}/{total_files}"


def generate_file_content(
    index: int,
    total_files: int,
    rng: random.Random,
    *,
    include_sparse: bool,
    include_dense: bool,
) -> bytes:
    lines = [header_marker(index, total_files), "use std::collections::HashMap;"]

    if include_sparse:
        lines.append(f"// Special marker: {KEYWORD_SPARSE}")

    if include_dense:
        for hit_index in range(DENSE_HITS_PER_FILE):
            lines.append(f"// {KEYWORD_DENSE} dedicated {hit_index}")

    for line_idx in range(rng.randint(20, 150)):
        if line_idx % 15 == 0:
            lines.append(f"{rng.choice(KEYWORDS_COMMON)} // token {line_idx}")
        else:
            words = [f"word_{rng.randint(1000, 9999)}" for _ in range(rng.randint(4, 12))]
            lines.append(" ".join(words))

    if index % 250 == 0:
        lines.append("// Long line: " + "X" * 16384)

    return ("\n".join(lines) + "\n").encode("utf-8")


def _is_prefix_complete_name(filename: str) -> bool:
    return Path(filename).name.startswith(PREFIX_STEM_PREFIX)


def generate_codebase(target_dir: Path, file_count: int, seed: int = 42) -> Path:
    rng = random.Random(seed)
    if target_dir.exists():
        shutil.rmtree(target_dir)
    target_dir.mkdir(parents=True, exist_ok=True)

    (target_dir / ".gitignore").write_text(
        "target/\nnode_modules/\n*.tmp\n.cache/\n", encoding="utf-8"
    )

    ignored_target = target_dir / "target" / "debug"
    ignored_target.mkdir(parents=True, exist_ok=True)
    (ignored_target / "ignored_file.rs").write_bytes(b"// Should be ignored by traversal\n")

    dir_count = max(1, file_count // 20)
    directories = [target_dir]
    for _directory_index in range(dir_count):
        depth = rng.randint(1, 4)
        parts = [f"mod_{rng.randint(1, 20)}" for _ in range(depth)]
        directory_path = target_dir.joinpath(*parts)
        directory_path.mkdir(parents=True, exist_ok=True)
        directories.append(directory_path)

    sparse_step = max(1, file_count // 10)
    dense_count = min(DENSE_FILE_TARGET, file_count)
    dense_indices = {(index * file_count) // dense_count for index in range(dense_count)}
    sparse_file_count = 0
    dense_file_count = 0
    dense_hit_count = 0
    prefix_files: list[str] = []
    suffix_bench_files: list[str] = []
    read_relative_path: str | None = None
    read_marker: str | None = None

    for file_index in range(file_count):
        chosen_dir = rng.choice(directories)
        if file_index % max(1, file_count // 25) == 0:
            extension = ".bench.txt"
        else:
            extension = rng.choice(EXTENSIONS)
        filename = f"file_{file_index:06d}{extension}"
        filepath = chosen_dir / filename
        include_sparse = file_index % sparse_step == 0
        include_dense = file_index in dense_indices
        filepath.write_bytes(
            generate_file_content(
                file_index,
                file_count,
                rng,
                include_sparse=include_sparse,
                include_dense=include_dense,
            )
        )
        relative_path = filepath.relative_to(target_dir).as_posix()
        if file_index == 0:
            read_relative_path = relative_path
            read_marker = header_marker(file_index, file_count)
        if include_sparse:
            sparse_file_count += 1
        if include_dense:
            dense_file_count += 1
            dense_hit_count += DENSE_HITS_PER_FILE
        if _is_prefix_complete_name(filename):
            prefix_files.append(relative_path)
        if filename.endswith(".bench.txt"):
            suffix_bench_files.append(relative_path)

    prefix_files.sort()
    suffix_bench_files.sort()

    with (target_dir / "read-target.txt").open("w", encoding="utf-8", newline="\n") as out:
        for line in range(20_000):
            out.write(f"read-sentinel-{line:05d} " + "x" * 80 + "\n")

    with (target_dir / "large-budget-target.txt").open("w", encoding="utf-8", newline="\n") as out:
        for line in range(5_000):
            out.write(f"budget-sentinel-{line:05d} " + "y" * 90 + "\n")

    expected: dict[str, Any] = {
        "version": 1,
        "file_count": file_count,
        "regular_file_count": file_count + 4,
        "read": {
            "path": read_relative_path,
            "marker": read_marker,
        },
        "read_target": {
            "path": "read-target.txt",
            "total_lines": 20_000,
            "slice_start": 5001,
            "slice_count": 50,
            "first_sentinel": "read-sentinel-05000",
            "last_sentinel": "read-sentinel-05049",
        },
        "large_budget_target": {
            "path": "large-budget-target.txt",
            "total_lines": 5_000,
        },
        "sparse_file_count": sparse_file_count,
        "dense_file_count": dense_file_count,
        "dense_hit_count": dense_hit_count,
        "absent_token": "AGENTSHIM_ABSENT_TOKEN_404",
        "prefix_files": prefix_files,
        "suffix_bench_files": suffix_bench_files,
    }
    (target_dir / EXPECTED_FILENAME).write_text(
        json.dumps(expected, indent=2) + "\n", encoding="utf-8"
    )
    return target_dir


def load_expected(corpus_dir: Path) -> dict[str, Any]:
    expected_path = corpus_dir / EXPECTED_FILENAME
    if not expected_path.is_file():
        return {}
    payload = json.loads(expected_path.read_text(encoding="utf-8"))
    return payload if isinstance(payload, dict) else {}


def main() -> None:
    parser = argparse.ArgumentParser(description="Synthetic Codebase Benchmark Dataset Generator")
    parser.add_argument(
        "--scale",
        type=str,
        default="1k",
        choices=["1k", "10k", "100k"],
        help="Scale of files to generate (1k, 10k, 100k)",
    )
    parser.add_argument(
        "--out",
        type=str,
        default=None,
        help="Destination directory for generated codebase",
    )
    parser.add_argument("--seed", type=int, default=42, help="Random seed for reproducibility")
    args = parser.parse_args()

    scale_counts = {"1k": 1000, "10k": 10000, "100k": 100000}
    count = scale_counts[args.scale]

    base_dir = Path(__file__).resolve().parent
    out_dir = Path(args.out) if args.out else base_dir / f"codebase_{args.scale}"

    print(f"Generating synthetic codebase ({args.scale} = {count} files) at {out_dir}...")
    generate_codebase(out_dir, count, seed=args.seed)
    print("Done.")


if __name__ == "__main__":
    main()
