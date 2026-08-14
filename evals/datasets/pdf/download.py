import hashlib
import json
import os
import sys
import urllib.request
from datetime import UTC, datetime
from pathlib import Path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def is_pdf(path: Path) -> bool:
    if not path.is_file():
        return False
    with path.open("rb") as stream:
        return b"%PDF-" in stream.read(8192)


def download(url: str, destination: Path) -> None:
    part_path = destination.with_suffix(destination.suffix + ".part")
    request = urllib.request.Request(
        url,
        headers={"User-Agent": "codexshim-evals-pdf/1.0"},
    )
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            with part_path.open("wb") as output:
                while chunk := response.read(1024 * 1024):
                    output.write(chunk)
        if not is_pdf(part_path):
            raise RuntimeError(f"downloaded content is not a PDF: {url}")
        os.replace(part_path, destination)
    finally:
        if part_path.exists():
            part_path.unlink()


def ensure_dataset(root: Path | None = None, verify_only: bool = False) -> Path:
    base_dir = Path(__file__).resolve().parent
    if root is None:
        root = base_dir / "corpus"
    manifest_path = base_dir / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    lock_path = base_dir / "manifest.lock.json"

    locked_map = {}
    if lock_path.exists():
        lock_data = json.loads(lock_path.read_text(encoding="utf-8"))
        for doc in lock_data.get("documents", []):
            locked_map[doc["id"]] = doc

    locked = []
    for document in manifest["documents"]:
        category_root = root / document["category"]
        destination = category_root / document["filename"]

        if not destination.exists():
            if verify_only:
                raise FileNotFoundError(f"Missing PDF: {destination}")
            category_root.mkdir(parents=True, exist_ok=True)
            print(f"Downloading {document['id']}: {document['title']}...", file=sys.stderr)
            download(document["url"], destination)

        digest = sha256(destination)
        expected = locked_map.get(document["id"], {}).get("sha256")
        if expected and digest != expected:
            raise ValueError(f"Checksum mismatch for {destination}: {digest} != {expected}")

        locked.append(
            document
            | {
                "path": destination.relative_to(root).as_posix(),
                "size_bytes": destination.stat().st_size,
                "sha256": digest,
            }
        )

    if not verify_only:
        lock_path.write_text(
            json.dumps(
                {
                    "version": 1,
                    "generated_at": datetime.now(UTC).isoformat(),
                    "documents": locked,
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
    return root


def main() -> None:
    verify_only = "--verify-only" in sys.argv
    corpus_root = None
    for arg in sys.argv[1:]:
        if not arg.startswith("--"):
            corpus_root = Path(arg)
            break
    ensure_dataset(root=corpus_root, verify_only=verify_only)
    print("PDF dataset ready.", file=sys.stderr)


if __name__ == "__main__":
    main()
