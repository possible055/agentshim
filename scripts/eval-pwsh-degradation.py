import argparse
import hashlib
import json
import os
import platform
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CASES = ROOT / "evals" / "pwsh_degradation.jsonl"
RESPONSE_SCHEMA = ROOT / "evals" / "pwsh_degradation_response.schema.json"
TOOL_CATALOG_SNAPSHOT = ROOT / "tests" / "snapshots" / "tools_list.json"


def read_jsonl(path: Path) -> list[dict]:
    records = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not line.strip():
            continue
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError as error:
            raise SystemExit(f"{path}:{line_number}: {error}") from error
    return records


def current_tool_catalog() -> list[dict]:
    snapshot = json.loads(TOOL_CATALOG_SNAPSHOT.read_text(encoding="utf-8"))
    tools = snapshot.get("tools")
    if not isinstance(tools, list):
        raise RuntimeError(f"{TOOL_CATALOG_SNAPSHOT} does not contain a tools array")
    return tools


def selection_prompt(catalog: list[dict], cases: list[dict]) -> str:
    visible_catalog = visible_tool_catalog(catalog)
    visible_cases = [{"id": case["id"], "prompt": case["prompt"]} for case in cases]
    return (
        "Evaluate tool selection using only the supplied current tool catalog. "
        "Do not call tools and do not perform the requested work. For each case, "
        "choose exactly one catalog tool and provide the arguments you would send. "
        "Use `unsupported` with an empty arguments object only when no catalog tool "
        "can perform the request. Return every case exactly once in the supplied order.\n\n"
        f"Tool catalog:\n{json.dumps(visible_catalog, ensure_ascii=False)}\n\n"
        f"Cases:\n{json.dumps(visible_cases, ensure_ascii=False)}"
    )


def visible_tool_catalog(catalog: list[dict]) -> list[dict]:
    return [
        {
            "name": tool["name"],
            "description": tool.get("description", ""),
            "inputSchema": tool.get("inputSchema", {}),
        }
        for tool in catalog
    ]


def tool_catalog_sha256(catalog: list[dict]) -> str:
    encoded = json.dumps(
        visible_tool_catalog(catalog),
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def collect_round(
    round_number: int,
    catalog: list[dict],
    cases: list[dict],
    model: str | None,
    reasoning_effort: str,
) -> list[dict]:
    with tempfile.TemporaryDirectory(prefix="pwsh-eval-") as directory:
        directory_path = Path(directory)
        output_path = directory_path / "decision.json"
        command = [
            "codex",
            "exec",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--config",
            f'model_reasoning_effort="{reasoning_effort}"',
            "--output-schema",
            str(RESPONSE_SCHEMA),
            "--output-last-message",
            str(output_path),
        ]
        if model:
            command.extend(["--model", model])
        command.append("-")
        completed = subprocess.run(
            command,
            cwd=directory_path,
            input=selection_prompt(catalog, cases),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=600,
        )
        if completed.returncode != 0:
            stderr = re.sub(
                r"\bpak_[A-Za-z0-9*]+\b",
                "<redacted-api-key>",
                completed.stderr[-4000:],
            )
            raise RuntimeError(
                f"Codex eval round {round_number} failed with {completed.returncode}: {stderr}"
            )
        response = json.loads(output_path.read_text(encoding="utf-8"))
        decisions = response.get("decisions")
        if not isinstance(decisions, list):
            raise RuntimeError(f"Codex eval round {round_number} omitted decisions")
        return decisions


def first_token(command: str) -> tuple[str, str]:
    command = command.lstrip()
    if not command:
        return "", ""
    if command[0] in {"'", '"'}:
        end = command.find(command[0], 1)
        if end < 0:
            return "", ""
        token = command[1:end]
        remainder = command[end + 1 :]
    else:
        parts = command.split(None, 1)
        token = parts[0]
        remainder = parts[1] if len(parts) == 2 else ""
    file_name = token.replace("\\", "/").rsplit("/", 1)[-1].lower()
    stem = file_name.rsplit(".", 1)[0]
    return stem, remainder


def pwsh_policy(case: dict, decision: dict) -> tuple[bool, bool, str]:
    if decision["tool"] != "bash":
        return True, False, "not a bash call"
    command = decision.get("arguments", {}).get("command")
    if not isinstance(command, str):
        return False, False, "bash decision has no string command"
    delegate, remainder = first_token(command)
    if delegate not in {"pwsh", "powershell"}:
        return True, False, "bash does not delegate to pwsh"
    options = remainder.lower().split()
    if "-file" in options:
        return True, False, "pwsh -File is an allowed script-as-program form"
    if "-command" in options and case["windows_domain"]:
        return True, False, "pwsh -Command is allowed for this Windows-only domain"
    return False, True, "pwsh command evaluation is not allowed for this case"


def score_round(round_number: int, cases: list[dict], decisions: list[dict]) -> list[dict]:
    by_id: dict[str, dict] = {}
    for decision in decisions:
        decision_id = decision.get("id")
        if not isinstance(decision_id, str):
            raise RuntimeError(f"round {round_number} invalid decision id {decision_id!r}")
        if decision_id in by_id:
            raise RuntimeError(f"round {round_number} duplicated case {decision_id!r}")
        by_id[decision_id] = decision
    expected_ids: set[str] = {str(case["id"]) for case in cases}
    if set(by_id) != expected_ids:
        missing = sorted(expected_ids - set(by_id))
        extra = sorted(set(by_id) - expected_ids)
        raise RuntimeError(
            f"round {round_number} decision IDs differ: missing={missing}, extra={extra}"
        )
    results = []
    for case in cases:
        decision = by_id[case["id"]]
        tool = decision.get("tool")
        arguments = decision.get("arguments")
        if not isinstance(arguments, dict):
            raise RuntimeError(
                f"round {round_number} case {case['id']} arguments must be an object"
            )
        tool_passed = tool in case["allowed_tools"]
        policy_passed, degraded, policy_reason = pwsh_policy(case, decision)
        results.append(
            {
                "round": round_number,
                "id": case["id"],
                "prompt": case["prompt"],
                "allowed_tools": case["allowed_tools"],
                "windows_domain": case["windows_domain"],
                "decision": decision,
                "tool_passed": tool_passed,
                "policy_passed": policy_passed,
                "passed": tool_passed and policy_passed,
                "pwsh_degraded": degraded,
                "reason": policy_reason,
            }
        )
    return results


def summarize(results: list[dict], metadata: dict) -> dict:
    total = len(results)
    passed = sum(result["passed"] for result in results)
    shell_calls = sum(result["decision"]["tool"] == "bash" for result in results)
    degraded = sum(result["pwsh_degraded"] for result in results)
    return {
        "benchmark": "pwsh-degradation",
        "metadata": metadata,
        "cases": total,
        "passed": passed,
        "failed": total - passed,
        "pass_rate": passed / total if total else None,
        "shell_calls": shell_calls,
        "noncompliant_pwsh_calls": degraded,
        "pwsh_degradation_rate": degraded / shell_calls if shell_calls else None,
        "failures": [
            {
                "round": result["round"],
                "id": result["id"],
                "tool": result["decision"]["tool"],
                "tool_passed": result["tool_passed"],
                "policy_passed": result["policy_passed"],
                "reason": result["reason"],
            }
            for result in results
            if not result["passed"]
        ],
    }


def write_jsonl(path: Path, records: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(json.dumps(record, ensure_ascii=False) + "\n" for record in records),
        encoding="utf-8",
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Collect and automatically score the offline pwsh degradation eval."
    )
    parser.add_argument("--cases", type=Path, default=DEFAULT_CASES)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--model", default=os.environ.get("CODEXSHIM_EVAL_MODEL"))
    parser.add_argument(
        "--reasoning-effort",
        default=os.environ.get("CODEXSHIM_EVAL_REASONING_EFFORT", "low"),
    )
    parser.add_argument(
        "--responses",
        type=Path,
        help="Re-score a prior normalized JSONL transcript without calling Codex.",
    )
    parser.add_argument("--transcript", type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--minimum-pass-rate", type=float)
    parser.add_argument("--print-tool-catalog-sha256", action="store_true")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.rounds < 1:
        raise SystemExit("--rounds must be at least 1")
    if args.print_tool_catalog_sha256:
        print(tool_catalog_sha256(current_tool_catalog()))
        return
    if not args.responses and not args.model:
        raise SystemExit("--model or CODEXSHIM_EVAL_MODEL is required for a reproducible eval")
    cases = read_jsonl(args.cases)
    case_ids = [case["id"] for case in cases]
    if len(set(case_ids)) != len(case_ids):
        raise SystemExit("eval case IDs must be unique")

    if args.responses:
        prior = read_jsonl(args.responses)
        pinned_hashes = {result.get("tool_catalog_sha256") for result in prior}
        if None in pinned_hashes or len(pinned_hashes) != 1:
            raise SystemExit("response transcript must pin exactly one tool_catalog_sha256")
        pinned_hash = pinned_hashes.pop()
        catalog = current_tool_catalog()
        current_hash = tool_catalog_sha256(catalog)
        if pinned_hash != current_hash:
            raise SystemExit(
                "response transcript tool catalog is stale: "
                f"expected {pinned_hash}, current {current_hash}"
            )
        rounds = sorted({result["round"] for result in prior})
        results = []
        for round_number in rounds:
            decisions = [
                result.get("decision", result)
                for result in prior
                if result["round"] == round_number
            ]
            scored = score_round(round_number, cases, decisions)
            for result in scored:
                result["tool_catalog_sha256"] = current_hash
            results.extend(scored)
        metadata = {
            "mode": "rescore",
            "responses": str(args.responses),
            "rounds": rounds,
            "tool_catalog_sha256": current_hash,
        }
    else:
        catalog = current_tool_catalog()
        catalog_hash = tool_catalog_sha256(catalog)
        results = []
        for round_number in range(1, args.rounds + 1):
            print(
                f"pwsh degradation eval round {round_number}/{args.rounds}",
                file=sys.stderr,
                flush=True,
            )
            decisions = collect_round(
                round_number,
                catalog,
                cases,
                args.model,
                args.reasoning_effort,
            )
            scored = score_round(round_number, cases, decisions)
            for result in scored:
                result["tool_catalog_sha256"] = catalog_hash
            results.extend(scored)
        metadata = {
            "mode": "collect-and-score",
            "rounds": args.rounds,
            "model": args.model,
            "reasoning_effort": args.reasoning_effort,
            "tool_catalog_sha256": catalog_hash,
            "codex_version": subprocess.run(
                ["codex", "--version"],
                check=True,
                capture_output=True,
                text=True,
                encoding="utf-8",
            ).stdout.strip(),
            "platform": platform.platform(),
            "tool_catalog": visible_tool_catalog(catalog),
        }

    timestamp = time.strftime("%Y%m%d-%H%M%S")
    transcript = args.transcript or (
        ROOT / "local" / "perf" / "out" / f"pwsh-degradation-{timestamp}.jsonl"
    )
    report_path = args.report or (
        ROOT / "local" / "perf" / "out" / f"pwsh-degradation-{timestamp}.json"
    )
    write_jsonl(transcript, results)
    report = summarize(results, metadata)
    report["transcript"] = str(transcript)
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print(json.dumps(report, ensure_ascii=False, indent=2))
    print(f"report={report_path}", file=sys.stderr)
    if args.minimum_pass_rate is not None and report["pass_rate"] < args.minimum_pass_rate:
        raise SystemExit(
            f"pass rate {report['pass_rate']:.6f} is below {args.minimum_pass_rate:.6f}"
        )


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.TimeoutExpired) as error:
        raise SystemExit(f"eval failed: {error}") from None
