use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::{Map, Value, json};

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

pub fn minimal_pdf(content: &[u8]) -> Vec<u8> {
    agentshim_core::tools::read::minimal_pdf(content)
}

pub fn pdf_with_text() -> Vec<u8> {
    minimal_pdf(b"BT /F1 18 Tf 20 150 Td (PDF image block) Tj ET")
}

pub fn pdf_full_page_image() -> Vec<u8> {
    let pixels = vec![0x80_u8; 80 * 80 * 3];
    let mut image = format!(
        "<< /Type /XObject /Subtype /Image /Width 80 /Height 80 /ColorSpace /DeviceRGB \
         /BitsPerComponent 8 /Length {} >>\nstream\n",
        pixels.len()
    )
    .into_bytes();
    image.extend_from_slice(&pixels);
    image.extend_from_slice(b"\nendstream");
    let operations = b"q 200 0 0 200 0 0 cm /Im0 Do Q";
    let mut content = format!("<< /Length {} >>\nstream\n", operations.len()).into_bytes();
    content.extend_from_slice(operations);
    content.extend_from_slice(b"\nendstream");

    agentshim_core::tools::read::assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources \
          << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
            .to_vec(),
        image,
        content,
    ])
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
