use std::{collections::BTreeSet, fs};

use serde_json::Value;

fn json_lines(path: &str) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("read eval corpus")
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("invalid JSON at {path}:{}: {error}", index + 1))
        })
        .collect()
}

#[test]
fn tool_selection_corpus_has_unique_ids_and_fixed_expectations() {
    let cases = json_lines("evals/tool_selection.jsonl");
    assert!(cases.len() >= 25);
    let allowed = BTreeSet::from([
        "read",
        "grep",
        "glob",
        "run_process",
        "apply_patch",
        "unsupported",
        "unsupported_or_split",
    ]);
    let mut ids = BTreeSet::new();
    for case in cases {
        let id = case["id"].as_str().expect("string id");
        assert!(ids.insert(id.to_owned()), "duplicate id {id}");
        let expected = case["expected"].as_str().expect("expected selection");
        assert!(
            allowed.contains(expected),
            "unexpected selection {expected}"
        );
        if expected == "run_process" {
            let program = case["arguments"]["program"]
                .as_str()
                .expect("run_process program");
            assert!(!program.contains(' '), "command string in program for {id}");
            assert!(case["arguments"]["args"].is_array());
        }
    }
}

#[test]
fn command_coverage_is_structured_and_above_fifty_percent() {
    let cases = json_lines("evals/command_coverage.jsonl");
    assert!(cases.len() >= 30);
    let supported = BTreeSet::from(["read-only", "native", "cmd-compat"]);
    let tools = BTreeSet::from(["read", "grep", "glob", "run_process"]);
    let mut ids = BTreeSet::new();
    let mut supported_count = 0_usize;
    for case in &cases {
        let id = case["id"].as_str().expect("string id");
        assert!(ids.insert(id.to_owned()), "duplicate id {id}");
        let category = case["category"].as_str().expect("category");
        if supported.contains(category) {
            supported_count += 1;
            let tool = case["tool"].as_str().expect("supported case tool");
            assert!(tools.contains(tool), "unknown tool {tool}");
            assert!(case["replacement"].is_object());
        } else {
            assert_eq!(category, "unsupported");
            assert!(case["reason"].is_string());
        }
    }
    assert!(supported_count * 2 >= cases.len());
}
