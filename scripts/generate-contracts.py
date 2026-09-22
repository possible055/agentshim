#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

TOOLS = ["read", "grep", "glob", "run_program", "bash", "bash_status"]
ERROR_CODES = [
    "INVALID_ARGS",
    "AGENTSHIM_CANCELLED",
    "AGENTSHIM_TIMEOUT",
    "AGENTSHIM_RESOURCE_BUSY",
    "AGENTSHIM_OUTCOME_UNCERTAIN",
    "AGENTSHIM_CAPTURE_FAILED",
]


def canonical_json(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def load_json(path: Path) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SystemExit(f"contract generator: cannot read {path}: {error}") from error


def validate(contract: object) -> dict[str, object]:
    if not isinstance(contract, dict):
        raise SystemExit("contract generator: contract root must be an object")
    if contract.get("version") != 1:
        raise SystemExit("contract generator: only contract version 1 is supported")
    if contract.get("toolNames") != TOOLS:
        raise SystemExit(f"contract generator: toolNames must be {TOOLS!r}")
    limits = contract.get("limits")
    expected_limits = {
        "globPatternCount": 32,
        "globPatternChars": 1024,
        "grepPatternChars": 8192,
        "contextLines": 20,
        "resultLimit": 1000,
        "stdinChars": 1048576,
    }
    if limits != expected_limits:
        raise SystemExit(f"contract generator: limits must be {expected_limits!r}")
    if contract.get("errorCodes") != ERROR_CODES:
        raise SystemExit("contract generator: errorCodes are out of order or incomplete")
    if contract.get("canonicalCodePolicy") != {
        "mapped": "Use one of errorCodes for shared cross-adapter semantics.",
        "unmappedMcp": (
            "Preserve the legacy MCP error class in canonicalCode until a versioned mapping is"
            " added."
        ),
    }:
        raise SystemExit("contract generator: canonicalCodePolicy is invalid")
    if contract.get("adapterErrorCodes") != {"dsh": ["SANDBOX_UNAVAILABLE"]}:
        raise SystemExit("contract generator: adapter error codes are invalid")
    semantics = contract.get("semantics")
    if not isinstance(semantics, dict):
        raise SystemExit("contract generator: semantics must be an object")
    read_scope = semantics.get("readScope")
    if not isinstance(read_scope, dict) or read_scope.get("default") != "unrestricted":
        raise SystemExit("contract generator: read scope must default to unrestricted")
    artifacts = contract.get("artifacts")
    if artifacts != {
        "mcpSchema": "generated/mcp-tools-v1.json",
        "dshSchema": "generated/dsh-tools-v1.json",
    }:
        raise SystemExit("contract generator: generated schema artifact paths are invalid")
    mappings = contract.get("wireMappings")
    if not isinstance(mappings, dict) or set(mappings) != {"mcp", "dsh"}:
        raise SystemExit("contract generator: both MCP and DSH wire mappings are required")
    guidance = contract.get("modelGuidance")
    if not isinstance(guidance, dict) or list(guidance) != TOOLS:
        raise SystemExit(
            "contract generator: modelGuidance must describe all six tools in catalog order"
        )
    for name in TOOLS:
        entry = guidance.get(name)
        if (
            not isinstance(entry, dict)
            or not isinstance(entry.get("description"), str)
            or not entry["description"].strip()
        ):
            raise SystemExit(f"contract generator: modelGuidance.{name}.description is required")
        if not isinstance(entry.get("defaults", {}), dict):
            raise SystemExit(f"contract generator: modelGuidance.{name}.defaults must be an object")
    return contract


def bounded_pattern(maximum: int, reject_bare_negation: bool = False) -> dict[str, object]:
    schema = {
        "type": "string",
        "minLength": 1,
        "maxLength": maximum,
    }
    if reject_bare_negation:
        schema["not"] = {"const": "!"}
    return schema


def glob_schema(limits: dict[str, int]) -> dict[str, object]:
    pattern = bounded_pattern(limits["globPatternChars"], True)
    return {
        "anyOf": [
            pattern,
            {
                "type": "array",
                "items": pattern,
                "minItems": 1,
                "maxItems": limits["globPatternCount"],
            },
        ]
    }


def guidance(contract: dict[str, object], name: str) -> dict[str, object]:
    model_guidance = contract["modelGuidance"]
    assert isinstance(model_guidance, dict)
    entry = model_guidance[name]
    assert isinstance(entry, dict)
    return entry


def annotate(
    schema: dict[str, object],
    contract: dict[str, object],
    name: str,
    default_keys: set[str] | None = None,
) -> dict[str, object]:
    entry = guidance(contract, name)
    schema["description"] = entry["description"]
    defaults = entry.get("defaults", {})
    assert isinstance(defaults, dict)
    properties = schema.get("properties")
    if isinstance(properties, dict):
        for key, value in defaults.items():
            if default_keys is not None and key not in default_keys:
                continue
            property_schema = properties.get(key)
            if isinstance(property_schema, dict):
                property_schema["default"] = value
    return schema


def mcp_schemas(contract: dict[str, object]) -> dict[str, object]:
    limits = contract["limits"]
    assert isinstance(limits, dict)
    read = {
        "type": "object",
        "additionalProperties": False,
        "required": ["path"],
        "properties": {
            "encoding": {"type": "string"},
            "line_count": {"type": "integer", "minimum": 1, "maximum": 2000},
            "pages": {"type": "string", "pattern": r"^[1-9][0-9]*(-[1-9][0-9]*)?$"},
            "path": {"type": "string", "minLength": 1},
            "pdf_mode": {"type": "string", "enum": ["auto", "text", "image"]},
            "pdf_cursor": {"type": "string", "minLength": 1},
            "office_cursor": {"type": "string", "minLength": 1},
            "start_line": {"type": "integer", "minimum": 1},
        },
    }
    grep = {
        "type": "object",
        "additionalProperties": False,
        "required": ["pattern"],
        "properties": {
            "case": {"type": "string", "enum": ["smart", "sensitive", "insensitive"]},
            "pattern": bounded_pattern(limits["grepPatternChars"]),
            "glob": glob_schema(limits),
            "context_lines": {"type": "integer", "minimum": 0, "maximum": limits["contextLines"]},
            "encoding": {"type": "string"},
            "fallback_encoding": {"type": "string"},
            "fixed_strings": {"type": "boolean"},
            "include_ignored": {"type": "boolean"},
            "limit": {"type": "integer", "minimum": 1, "maximum": limits["resultLimit"]},
            "mode": {"type": "string", "enum": ["content", "files", "count"]},
            "offset": {"type": "integer", "minimum": 0},
            "path": {"type": "string"},
            "type": {"type": "string"},
        },
    }
    glob = {
        "type": "object",
        "additionalProperties": False,
        "required": ["pattern"],
        "properties": {
            "include_ignored": {"type": "boolean"},
            "pattern": glob_schema(limits),
            "limit": {"type": "integer", "minimum": 1, "maximum": limits["resultLimit"]},
            "offset": {"type": "integer", "minimum": 0},
            "path": {"type": "string"},
            "type": {"type": "string", "enum": ["file", "directory", "any"]},
        },
    }
    run_program = {
        "type": "object",
        "additionalProperties": False,
        "required": ["program"],
        "properties": {
            "program": {"type": "string", "minLength": 1},
            "args": {"type": "array", "items": {"type": "string"}},
            "cwd": {"type": "string"},
            "env": {"type": "object", "additionalProperties": {"type": "string"}},
            "unset_env": {"type": "array", "items": {"type": "string"}},
            "stdin": {
                "oneOf": [{"type": "string"}, {"type": "null"}],
                "maxLength": limits["stdinChars"],
            },
            "timeout_ms": {"type": "integer", "minimum": 1},
        },
    }
    bash_run = {
        "type": "object",
        "additionalProperties": False,
        "required": ["command"],
        "properties": {
            "command": {"type": "string", "minLength": 1},
            "cwd": {"type": "string"},
            "detach": {"type": "boolean", "const": False},
            "msys_argument_conversion": {"type": "string", "enum": ["default", "disabled"]},
            "timeout_ms": {"type": "integer", "minimum": 1},
        },
    }
    bash_detached = {
        "type": "object",
        "additionalProperties": False,
        "required": ["command", "detach", "log_path"],
        "properties": {
            "command": {"type": "string", "minLength": 1},
            "cwd": {"type": "string"},
            "detach": {"type": "boolean", "const": True},
            "log_path": {"type": "string", "minLength": 1},
            "msys_argument_conversion": {"type": "string", "enum": ["default", "disabled"]},
            "timeout_ms": {"type": "integer", "minimum": 1},
        },
    }
    bash_status = {
        "type": "object",
        "additionalProperties": False,
        "required": ["job_id"],
        "properties": {
            "job_id": {"type": "string", "minLength": 1},
            "tail_bytes": {"type": "integer", "minimum": 0, "maximum": 16384},
            "cursor": {"type": "integer", "minimum": 0},
            "max_bytes": {"type": "integer", "minimum": 0, "maximum": 16384},
            "wait_ms": {"type": "integer", "minimum": 0, "maximum": 1000},
        },
    }
    bash_terminate = {
        "type": "object",
        "additionalProperties": False,
        "required": ["action", "job_id"],
        "properties": {
            "action": {"type": "string", "const": "terminate"},
            "job_id": {"type": "string", "minLength": 1},
        },
    }
    annotate(read, contract, "read")
    annotate(grep, contract, "grep")
    annotate(glob, contract, "glob")
    annotate(run_program, contract, "run_program")
    annotate(bash_run, contract, "bash", {"detach", "msys_argument_conversion"})
    annotate(bash_detached, contract, "bash", {"msys_argument_conversion"})
    annotate(bash_status, contract, "bash_status")
    bash = {
        "oneOf": [bash_run, bash_detached, bash_terminate],
        "description": guidance(contract, "bash")["description"],
    }
    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://agentshim.dev/contracts/generated/mcp-tools-v1.json",
        "version": 1,
        "dialect": "mcp",
        "tools": {
            "read": read,
            "grep": grep,
            "glob": glob,
            "run_program": run_program,
            "bash": bash,
            "bash_status": bash_status,
        },
    }


def dsh_schemas(contract: dict[str, object]) -> dict[str, object]:
    limits = contract["limits"]
    assert isinstance(limits, dict)

    def dsh_pattern(
        maximum: int, required: bool = False, reject_bare_negation: bool = False
    ) -> dict[str, object]:
        schema = {
            "type": "string",
            "minLength": 1,
            "maxLength": maximum,
            **({"required": True} if required else {}),
        }
        if reject_bare_negation:
            schema["not"] = {"const": "!"}
        return schema

    dsh_glob = {
        "oneOf": [
            dsh_pattern(limits["globPatternChars"], reject_bare_negation=True),
            {
                "type": "array",
                "items": dsh_pattern(limits["globPatternChars"], reject_bare_negation=True),
                "minItems": 1,
                "maxItems": limits["globPatternCount"],
            },
        ],
    }
    descriptions = {name: guidance(contract, name)["description"] for name in TOOLS}
    result = {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://agentshim.dev/contracts/generated/dsh-tools-v1.json",
        "version": 1,
        "dialect": "dsh",
        "descriptions": descriptions,
        "parameters": {
            "read": {
                "path": {"type": "string", "minLength": 1, "required": True},
                "artifact_offset": {"type": "integer", "minimum": 0},
                "encoding": {"type": "string"},
                "line_count": {"type": "integer", "minimum": 1, "maximum": 2000},
                "pages": {"type": "string"},
                "pdf_mode": {"type": "string", "enum": ["auto", "text", "image"]},
                "pdf_cursor": {"type": "string", "minLength": 1},
                "office_cursor": {"type": "string", "minLength": 1},
                "start_line": {"type": "integer", "minimum": 1},
            },
            "grep": {
                "pattern": dsh_pattern(limits["grepPatternChars"], required=True),
                "case": {"type": "string", "enum": ["smart", "sensitive", "insensitive"]},
                "context_lines": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": limits["contextLines"],
                },
                "encoding": {"type": "string"},
                "fallback_encoding": {"type": "string"},
                "fixed_strings": {"type": "boolean"},
                "glob": dsh_glob,
                "include_ignored": {"type": "boolean"},
                "limit": {"type": "integer", "minimum": 1, "maximum": limits["resultLimit"]},
                "mode": {"type": "string", "enum": ["content", "files", "count"]},
                "offset": {"type": "integer", "minimum": 0},
                "path": {"type": "string"},
                "type": {"type": "string"},
            },
            "glob": {
                "pattern": {**dsh_glob, "required": True},
                "include_ignored": {"type": "boolean"},
                "limit": {"type": "integer", "minimum": 1, "maximum": limits["resultLimit"]},
                "offset": {"type": "integer", "minimum": 0},
                "path": {"type": "string"},
                "type": {"type": "string", "enum": ["file", "directory", "any"]},
            },
            "run_program": {
                "program": {"type": "string", "required": True, "minLength": 1},
                "args": {"type": "array", "items": {"type": "string"}},
                "cwd": {"type": "string"},
                "env": {"type": "object", "additionalProperties": {"type": "string"}},
                "unset_env": {"type": "array", "items": {"type": "string"}},
                "stdin": {
                    "oneOf": [
                        {"type": "string", "maxLength": limits["stdinChars"]},
                        {"type": "null"},
                    ]
                },
                "timeout_ms": {"type": "integer", "minimum": 1},
            },
            "bash": {
                "command": {"type": "string", "required": True, "minLength": 1},
                "description": {"type": "string", "required": True, "minLength": 1},
                "workdir": {"type": "string"},
                "timeoutMs": {"type": "integer", "minimum": 1},
                "run_in_background": {"type": "boolean"},
                "msys_argument_conversion": {"type": "string", "enum": ["default", "disabled"]},
            },
            "bash_status": {"job_id": {"type": "string", "required": True}},
        },
        "outputs": {
            "process": {
                "text": {"type": "string", "required": True},
                "exitCode": {"oneOf": [{"type": "string"}, {"type": "null"}], "required": True},
                "stdout": {"type": "object", "required": True},
                "stderr": {"type": "object", "required": True},
                "limitExceeded": {"type": "boolean", "required": True},
                "outcomeUncertain": {"type": "boolean", "required": True},
                "sandbox": {"type": "object"},
            },
            "bash_status": {
                "status": {"type": "string", "required": True},
                "label": {"type": "string", "required": True},
                "detail": {"type": "string"},
                "exitCode": {"oneOf": [{"type": "string"}, {"type": "null"}]},
                "failure": {"type": "object"},
                "artifacts": {"type": "array"},
                "limitExceeded": {"type": "boolean"},
                "sandbox": {"type": "object"},
                "denied": {"type": "boolean"},
                "runnerFailed": {"type": "boolean"},
            },
        },
    }
    parameters = result["parameters"]
    assert isinstance(parameters, dict)
    for name in TOOLS:
        properties = parameters[name]
        assert isinstance(properties, dict)
        defaults = guidance(contract, name).get("defaults", {})
        assert isinstance(defaults, dict)
        for key, value in defaults.items():
            property_schema = properties.get(key)
            if isinstance(property_schema, dict):
                property_schema["default"] = value
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="verify checked-in projections")
    args = parser.parse_args()

    root = Path(__file__).resolve().parent.parent
    source_path = root / "contracts" / "tool-contract-v1.json"
    snapshot_path = root / "contracts" / "tool-contract-v1.snapshot.json"
    generated_dir = root / "contracts" / "generated"
    contract = validate(load_json(source_path))
    expected = canonical_json(contract)
    projections = {
        generated_dir / "mcp-tools-v1.json": mcp_schemas(contract),
        generated_dir / "dsh-tools-v1.json": dsh_schemas(contract),
    }

    if args.check:
        if not snapshot_path.is_file():
            print(f"contract generator: missing {snapshot_path}", file=sys.stderr)
            return 1
        actual = snapshot_path.read_text(encoding="utf-8")
        if actual != expected:
            print(
                f"contract generator: {snapshot_path} is stale; run scripts/generate-contracts.py",
                file=sys.stderr,
            )
            return 1
        for path, value in projections.items():
            if not path.is_file() or path.read_text(encoding="utf-8") != canonical_json(value):
                print(
                    f"contract generator: {path} is stale; run scripts/generate-contracts.py",
                    file=sys.stderr,
                )
                return 1
        return 0

    generated_dir.mkdir(parents=True, exist_ok=True)
    snapshot_path.write_text(expected, encoding="utf-8")
    for path, value in projections.items():
        path.write_text(canonical_json(value), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
