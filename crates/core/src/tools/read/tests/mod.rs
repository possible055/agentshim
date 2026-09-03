use std::{fs, sync::Arc};

use base64::Engine as _;
use encoding_rs::{BIG5, GB18030, GBK};
use tokio_util::sync::CancellationToken;

use crate::path::{FileAccess, ReadScope, RepositoryRoot};
use crate::runtime::DEFAULT_PDF_TEXT_MEMORY_BYTES;
use crate::tools::read::test_support::*;
use crate::tools::read::{
    AFTER_READ_HOOK, Attempt, BEFORE_READ_HOOK, DecodeError, DocumentMemoryBudgets,
    MAX_IMAGE_BASE64_BYTES, MAX_LINE_COUNT, PdfMode, ReadError, ReadRequest,
    TEXT_READ_MEMORY_BYTES, execute, execute_output, execute_prepared_with_budget, prepare,
};

fn budgets() -> DocumentMemoryBudgets {
    DocumentMemoryBudgets::defaults()
}

fn access(path: &std::path::Path) -> Arc<FileAccess> {
    access_with_scope(path, ReadScope::Normal)
}

fn access_with_scope(path: &std::path::Path, scope: ReadScope) -> Arc<FileAccess> {
    Arc::new(FileAccess::new(
        Arc::new(RepositoryRoot::open(path).expect("root")),
        scope,
    ))
}

fn request(path: &str) -> ReadRequest {
    ReadRequest {
        path: path.to_owned(),
        start_line: None,
        line_count: None,
        encoding: None,
        pdf_mode: None,
        pages: None,
        pdf_cursor: None,
        office_cursor: None,
    }
}

/// The 16-hex-digit shape a real `source_id` has, for tests that only need a
/// well-formed token rather than one matching a specific fixture.
fn sample_cursor(text_offset: Option<usize>) -> String {
    match text_offset {
        Some(offset) => format!("abcdef0123456789.{offset}"),
        None => "abcdef0123456789".to_owned(),
    }
}

#[test]
fn reads_numbered_utf8_crlf_and_utf16_pages() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("utf8.txt"), "alpha\r\nbeta\n").expect("utf8");
    fs::write(fixture.path().join("utf8-bom.txt"), b"\xEF\xBB\xBFbom\n").expect("utf8 bom");
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "one\ntwo\nthree".encode_utf16() {
        utf16.extend(unit.to_le_bytes());
    }
    fs::write(fixture.path().join("utf16.txt"), utf16).expect("utf16");
    let mut utf16be = vec![0xFE, 0xFF];
    for unit in "big\nend".encode_utf16() {
        utf16be.extend(unit.to_be_bytes());
    }
    fs::write(fixture.path().join("utf16be.txt"), utf16be).expect("utf16be");
    fs::write(fixture.path().join("latin.txt"), [0x63, 0x61, 0x66, 0xE9]).expect("windows-1252");
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();

    let utf8 = execute(&root, &request("utf8.txt"), &cancellation).expect("read utf8");
    assert_eq!(utf8, "1\talpha\n2\tbeta");
    assert!(!utf8.contains("Path:"));
    let bom = execute(&root, &request("utf8-bom.txt"), &cancellation).expect("utf8 bom");
    assert!(bom.contains("1\tbom"));

    let mut page = request("utf16.txt");
    page.start_line = Some(2);
    page.line_count = Some(1);
    let utf16 = execute(&root, &page, &cancellation).expect("read utf16");
    assert!(utf16.contains("Encoding: UTF-16LE\n2\ttwo"));
    assert!(utf16.ends_with("Partial: next_start_line=3. (line_count)"));
    let be = execute(&root, &request("utf16be.txt"), &cancellation).expect("utf16be");
    assert!(be.contains("Encoding: UTF-16BE\n1\tbig"));
    let mut latin = request("latin.txt");
    latin.encoding = Some("windows-1252".to_owned());
    let latin = execute(&root, &latin, &cancellation).expect("explicit encoding");
    assert!(latin.contains("Encoding: windows-1252\n1\tcafé"));
}

#[test]
fn known_files_inside_denied_directories_remain_readable() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::create_dir_all(fixture.path().join("node_modules/pkg")).expect("node_modules");
    fs::write(
        fixture.path().join("node_modules/pkg/index.js"),
        "module.exports = 1;\n",
    )
    .expect("pkg");
    let root = access(fixture.path());
    let output = execute(
        &root,
        &request("node_modules/pkg/index.js"),
        &CancellationToken::new(),
    )
    .expect("read inside denied directory");
    assert!(output.contains("module.exports = 1;"));
}

mod assessment;
mod continuation;
mod office;
mod pdf;
mod text;
