import json
import re
from pathlib import Path
from typing import Any

from .adapters.base import UnsupportedError

_COUNT_LINE = re.compile(r"^(?P<path>.+):(?P<count>\d+)\s*$")
_SKIP_PREFIXES = (
    "partial:",
    "skipped",
    "no matches",
    "no results",
    "wall time:",
    "output:",
)
_FILE_COUNT_KEYS = (
    "file_count",
    "files_with_matches",
    "matching_files",
    "files",
)
_HIT_COUNT_KEYS = (
    "dense_hit_count",
    "hit_count",
    "match_count",
    "matches",
    "occurrences",
    "total",
    "count",
)


class GateError(RuntimeError):
    """The tool response did not satisfy the ranking contract."""


def mcp_error_message(response: Any) -> str | None:
    if not isinstance(response, dict):
        return "MCP response is not an object"
    error = response.get("error")
    if isinstance(error, dict):
        message = error.get("message") or error.get("code") or "MCP error"
        return str(message)
    if isinstance(error, str) and error:
        return error
    result = response.get("result")
    if isinstance(result, dict) and result.get("isError"):
        text = response_text(response).strip()
        return text or "MCP tool result isError=true"
    return None


def raise_if_mcp_error(response: Any) -> None:
    message = mcp_error_message(response)
    if message:
        raise RuntimeError(message)


def response_text(response: Any) -> str:
    mcp_text = _mcp_content_text(response)
    if mcp_text is not None:
        return mcp_text
    chunks = list(_collect_strings(response))
    return "\n".join(chunk for chunk in chunks if chunk)


def normalize_path(path: str, corpus_root: Path) -> str:
    cleaned = path.strip().strip('"').replace("\\", "/")
    if cleaned.startswith("file://"):
        cleaned = cleaned[7:]
        if re.match(r"^/[A-Za-z]:/", cleaned):
            cleaned = cleaned[1:]
    candidate = Path(cleaned)
    corpus = corpus_root.resolve()
    if candidate.is_absolute():
        attempts = [candidate]
    else:
        attempts = [Path.cwd() / candidate, corpus / candidate]
    for attempt in attempts:
        try:
            return attempt.resolve().relative_to(corpus).as_posix()
        except (OSError, RuntimeError, ValueError):
            continue
    return Path(cleaned).as_posix().lstrip("./")


def evaluate_gate(
    gate: str,
    response: Any,
    expected: dict[str, Any],
    corpus_root: Path,
) -> None:
    raise_if_mcp_error(response)
    text = response_text(response)
    if gate != "glob_first_page" and ("\nPartial:" in text or text.startswith("Partial:")):
        raise GateError("response is paginated or truncated; ranking requires one complete page")
    if gate == "read_marker":
        _gate_read_marker(text, expected)
        return
    if gate == "grep_file_count":
        _gate_grep_file_count(response, text, expected)
        return
    if gate == "grep_hit_total":
        _gate_grep_hit_total(response, text, expected)
        return
    if gate == "glob_path_set":
        _gate_glob_path_set(text, expected, corpus_root)
        return
    if gate == "glob_first_page":
        _gate_glob_first_page(text, expected, corpus_root)
        return
    raise GateError(f"unknown ranking gate '{gate}'")


def _gate_read_marker(text: str, expected: dict[str, Any]) -> None:
    read_expected = expected.get("read")
    if not isinstance(read_expected, dict):
        raise UnsupportedError("expected.json is missing read.marker")
    marker = read_expected.get("marker")
    if not isinstance(marker, str) or not marker:
        raise UnsupportedError("expected.json is missing read.marker")
    if marker not in text:
        raise GateError(f"read response does not contain marker {marker!r}")


def _gate_grep_file_count(response: Any, text: str, expected: dict[str, Any]) -> None:
    wanted = expected.get("sparse_file_count")
    if not isinstance(wanted, int):
        raise UnsupportedError("expected.json is missing sparse_file_count")
    parsed = _parse_grep_summary(response, text)
    if parsed["file_count"] is None:
        raise UnsupportedError("could not parse grep file count from response")
    if parsed["file_count"] != wanted:
        raise GateError(f"grep file count {parsed['file_count']} != expected {wanted}")


def _gate_grep_hit_total(response: Any, text: str, expected: dict[str, Any]) -> None:
    wanted = expected.get("dense_hit_count")
    if not isinstance(wanted, int):
        raise UnsupportedError("expected.json is missing dense_hit_count")
    parsed = _parse_grep_summary(response, text)
    if parsed["hit_count"] is None:
        raise UnsupportedError("could not parse grep hit total from response")
    if parsed["hit_count"] != wanted:
        raise GateError(f"grep hit total {parsed['hit_count']} != expected {wanted}")


def _gate_glob_path_set(text: str, expected: dict[str, Any], corpus_root: Path) -> None:
    wanted_raw = expected.get("prefix_files")
    if not isinstance(wanted_raw, list) or not wanted_raw:
        raise UnsupportedError("expected.json is missing prefix_files")
    wanted = {normalize_path(str(path), corpus_root) for path in wanted_raw}
    parsed = _parse_path_lines(text, corpus_root)
    if parsed is None:
        raise UnsupportedError("could not parse glob path set from response")
    if parsed != wanted:
        raise GateError(f"glob path set {sorted(parsed)} != expected {sorted(wanted)}")


def _gate_glob_first_page(text: str, expected: dict[str, Any], corpus_root: Path) -> None:
    file_count = expected.get("file_count")
    if not isinstance(file_count, int) or file_count < 50:
        raise UnsupportedError("expected.json file_count is missing or below 50")
    parsed = _parse_path_lines(text, corpus_root)
    if parsed is None:
        raise UnsupportedError("could not parse glob first-page paths from response")
    if len(parsed) != 50:
        raise GateError(f"glob first page returned {len(parsed)} paths, expected 50")
    for relative in parsed:
        candidate = corpus_root / relative
        if not candidate.is_file():
            raise GateError(f"glob first page returned non-file path {relative}")


def _parse_grep_summary(response: Any, text: str) -> dict[str, int | None]:
    from_lines = _parse_count_lines(text)
    from_json = _json_counts(response) or _json_counts(_maybe_json(text))
    file_count = None
    hit_count = None
    if from_lines is not None:
        file_count = len(from_lines)
        hit_count = sum(from_lines.values())
    if from_json:
        if file_count is None:
            file_count = from_json.get("file_count")
        if hit_count is None:
            hit_count = from_json.get("hit_count")
    return {"file_count": file_count, "hit_count": hit_count}


def _parse_count_lines(text: str) -> dict[str, int] | None:
    parsed: dict[str, int] = {}
    saw_count_line = False
    for line in _content_lines(text):
        match = _COUNT_LINE.match(line)
        if match is None:
            continue
        saw_count_line = True
        parsed[match.group("path")] = int(match.group("count"))
    return parsed if saw_count_line else None


def _parse_path_lines(text: str, corpus_root: Path) -> set[str] | None:
    paths: set[str] = set()
    saw_path = False
    for line in _content_lines(text):
        match = _COUNT_LINE.match(line)
        raw = match.group("path") if match else line
        if not raw:
            continue
        saw_path = True
        paths.add(normalize_path(raw, corpus_root))
    return paths if saw_path else None


def _content_lines(text: str) -> list[str]:
    lines: list[str] = []
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        lowered = line.lower()
        if any(lowered.startswith(prefix) for prefix in _SKIP_PREFIXES):
            continue
        lines.append(line)
    return lines


def _json_counts(payload: Any) -> dict[str, int] | None:
    if not isinstance(payload, dict):
        return None
    found: dict[str, int] = {}
    file_count = _first_int(payload, _FILE_COUNT_KEYS)
    hit_count = _first_int(payload, _HIT_COUNT_KEYS)
    if file_count is not None:
        found["file_count"] = file_count
    if hit_count is not None:
        found["hit_count"] = hit_count
    return found or None


def _first_int(payload: dict[str, Any], keys: tuple[str, ...]) -> int | None:
    for key in keys:
        if key not in payload:
            continue
        value = payload[key]
        if isinstance(value, bool):
            continue
        if isinstance(value, int):
            return value
        if isinstance(value, list):
            return len(value)
        if isinstance(value, dict) and "count" in value and isinstance(value["count"], int):
            return value["count"]
    return None


def _maybe_json(text: str) -> Any:
    stripped = text.strip()
    if not stripped or stripped[0] not in "[{":
        return None
    try:
        return json.loads(stripped)
    except json.JSONDecodeError:
        return None


def _mcp_content_text(response: Any) -> str | None:
    if not isinstance(response, dict):
        return None
    result = response.get("result")
    if isinstance(result, dict):
        content = result.get("content")
        if isinstance(content, list):
            texts = [
                item["text"]
                for item in content
                if isinstance(item, dict) and isinstance(item.get("text"), str)
            ]
            if texts:
                return "\n".join(texts)
        if isinstance(result.get("text"), str):
            return str(result["text"])
    if isinstance(response.get("stdout"), str):
        return str(response["stdout"])
    if isinstance(response.get("text"), str):
        return str(response["text"])
    return None


def _collect_strings(value: Any) -> list[str]:
    found: list[str] = []
    if isinstance(value, str):
        found.append(value)
        return found
    if isinstance(value, dict):
        text = value.get("text")
        if isinstance(text, str):
            found.append(text)
        for key in ("content", "result", "stdout", "output", "message"):
            if key in value:
                found.extend(_collect_strings(value[key]))
        return found
    if isinstance(value, list):
        for item in value:
            found.extend(_collect_strings(item))
    return found
