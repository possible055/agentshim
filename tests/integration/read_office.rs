use super::common::{fixtures::*, session::*};
use super::*;

fn office_fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates")
        .join("office-read-core")
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn six_office_formats_return_markdown_over_real_stdio() {
    let fixture = tempfile::tempdir().expect("fixture");
    for name in [
        "sample.docx",
        "sample.xlsx",
        "sample.pptx",
        "sample.doc",
        "sample.xls",
        "sample.ppt",
    ] {
        fs::copy(office_fixture(name), fixture.path().join(name)).expect("copy fixture");
    }
    let mut session = TestSession::start_at(fixture.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    for (id, name, label, expected) in [
        (2, "sample.docx", "DOCX", "Office read fixture"),
        (3, "sample.xlsx", "XLSX", "Alpha"),
        (4, "sample.pptx", "PPTX", "Slide body"),
        (5, "sample.doc", "DOC", "Office read fixture"),
        (6, "sample.xls", "XLS", "Alpha"),
        (7, "sample.ppt", "PPT", "Slide body"),
    ] {
        let response = session.call_tool(id, "read", json!({ "path": name }));
        assert_eq!(response["result"]["isError"], false, "{name}: {response}");
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("text response");
        assert!(text.starts_with(&format!("Office: {label} as Markdown")));
        assert!(text.contains(expected), "{name}: {text}");
    }
    session.close();
}

#[test]
fn supported_extension_with_invalid_container_is_office_invalid() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("broken.docx"), "not a package").expect("fixture");
    let mut session = TestSession::start_at(fixture.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    let response = session.call_tool(2, "read", json!({ "path": "broken.docx" }));
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "office_invalid"
    );
    session.close();
}
