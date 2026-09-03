use super::*;
use crate::output::TestCallBudget;

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("office-read-core")
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn office_read_uses_markdown_and_replayable_cursor() {
    let root = tempfile::tempdir().expect("root");
    fs::copy(fixture("sample.xlsx"), root.path().join("book.xlsx")).expect("copy fixture");
    let access = access(root.path());
    let cancellation = CancellationToken::new();
    let mut request = request("book.xlsx");
    let prepared = prepare(&access, &request, &cancellation, budgets()).expect("prepare");
    assert!(prepared.structured_document());
    let budget = TestCallBudget {
        page_bytes: 256,
        wire_bytes: 1_024,
        ceiling: 8_192,
    };
    let first = execute_prepared_with_budget(&access, &request, prepared, &cancellation, &budget)
        .expect("execute");
    let Attempt::Stable(first) = first else {
        panic!("stable fixture")
    };
    assert!(first.text.starts_with("Office: XLSX as Markdown"));
    let token = first
        .text
        .split("office_cursor=\"")
        .nth(1)
        .and_then(|tail| tail.split('"').next())
        .expect("partial response carries cursor");
    request.office_cursor = Some(token.to_owned());
    let prepared = prepare(&access, &request, &cancellation, budgets()).expect("prepare cursor");
    let next = execute_prepared_with_budget(&access, &request, prepared, &cancellation, &budget)
        .expect("continue");
    assert!(matches!(next, Attempt::Stable(_)));
}

#[test]
fn office_cursor_rejects_pdf_and_text_parameters_before_io() {
    let root = tempfile::tempdir().expect("root");
    let access = access(root.path());
    let cancellation = CancellationToken::new();
    let mut request = request("missing.docx");
    request.office_cursor = Some("2:docx:abcdef0123456789:0:0:".to_owned());
    request.start_line = Some(1);
    let error = prepare(&access, &request, &cancellation, budgets())
        .err()
        .expect("validation");
    assert!(matches!(error, ReadError::Validation(_)));
}
