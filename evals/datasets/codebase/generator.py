import argparse
import random
import shutil
from pathlib import Path

EXTENSIONS = [".rs", ".py", ".ts", ".js", ".json", ".md", ".toml", ".txt"]
KEYWORDS_COMMON = [
    "fn main()",
    "import os",
    "export const",
    "async def",
    "impl Handler",
]
KEYWORD_SPARSE = "CODEXSHIM_BENCHMARK_SPARSE_TARGET"
KEYWORD_DENSE = "CODEXSHIM_BENCHMARK_DENSE_MATCH"


def generate_file_content(index: int, total_files: int, rng: random.Random) -> bytes:
    lines = []
    lines.append(f"// Synthetic benchmark file {index}/{total_files}")
    lines.append("use std::collections::HashMap;")

    # About 1 in 10 files contains the sparse keyword.
    if index % max(1, total_files // 10) == 0:
        lines.append(f"// Special marker: {KEYWORD_SPARSE}")

    # Every file has some common keywords and random filler
    for line_idx in range(rng.randint(20, 150)):
        if line_idx % 15 == 0:
            lines.append(f"{rng.choice(KEYWORDS_COMMON)} // token {line_idx}")
        elif line_idx % 30 == 0:
            lines.append(f"// {KEYWORD_DENSE} line {line_idx}")
        else:
            words = [f"word_{rng.randint(1000, 9999)}" for _ in range(rng.randint(4, 12))]
            lines.append(" ".join(words))

    # Long line case on occasional files
    if index % 250 == 0:
        lines.append("// Long line: " + "X" * 16384)

    text = "\n".join(lines) + "\n"
    return text.encode("utf-8")


def generate_codebase(target_dir: Path, file_count: int, seed: int = 42) -> Path:
    rng = random.Random(seed)
    if target_dir.exists():
        shutil.rmtree(target_dir)
    target_dir.mkdir(parents=True, exist_ok=True)

    # Create root ignore files
    (target_dir / ".gitignore").write_text(
        "target/\nnode_modules/\n*.tmp\n.cache/\n", encoding="utf-8"
    )

    # Create ignored directories with dummy files
    ignored_target = target_dir / "target" / "debug"
    ignored_target.mkdir(parents=True, exist_ok=True)
    (ignored_target / "ignored_file.rs").write_bytes(b"// Should be ignored by traversal\n")

    # Determine directory structure
    dir_count = max(1, file_count // 20)
    directories = [target_dir]
    for d_idx in range(dir_count):
        depth = rng.randint(1, 4)
        parts = [f"mod_{rng.randint(1, 20)}" for _ in range(depth)]
        d_path = target_dir.joinpath(*parts)
        d_path.mkdir(parents=True, exist_ok=True)
        directories.append(d_path)

    for f_idx in range(file_count):
        chosen_dir = rng.choice(directories)
        ext = rng.choice(EXTENSIONS)
        filename = f"file_{f_idx:06d}{ext}"
        filepath = chosen_dir / filename
        filepath.write_bytes(generate_file_content(f_idx, file_count, rng))

    return target_dir


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
