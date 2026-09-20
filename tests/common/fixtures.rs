use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::{Map, Value, json};

pub use agentshim_test_support::pdf::{minimal_pdf, pdf_full_page_image};

pub fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {
            "name": "agentshim-wire-test",
            "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

pub fn modern_request(id: u64, method: &str, mut params: Map<String, Value>) -> Value {
    params.insert("_meta".to_owned(), modern_meta());
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    })
}

pub fn empty_params() -> Map<String, Value> {
    Map::new()
}

pub fn response_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool response text")
}

pub fn pdf_with_text() -> Vec<u8> {
    minimal_pdf(b"BT /F1 18 Tf 20 150 Td (PDF image block) Tj ET")
}

pub fn jsonl_paths(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect()
}

pub fn records(directory: &Path) -> Vec<Value> {
    jsonl_paths(directory)
        .iter()
        .flat_map(|path| {
            BufReader::new(fs::File::open(path).expect("log"))
                .lines()
                .map(|line| {
                    serde_json::from_str::<Value>(&line.expect("complete line"))
                        .expect("complete JSON")
                })
                .collect::<Vec<_>>()
        })
        .collect()
}
