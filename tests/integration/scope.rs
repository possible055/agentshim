use super::common::{fixtures::*, session::*};
use super::*;

#[test]
fn normal_tools_reject_unmanaged_paths_outside_the_startup_root() {
    let startup = tempfile::tempdir().expect("startup root");
    std::fs::write(startup.path().join("inside.txt"), "inside").expect("startup file");
    let unmanaged = tempfile::tempdir().expect("unmanaged fixture");
    std::fs::write(unmanaged.path().join("secret.txt"), "secret").expect("unmanaged file");
    let outside = unmanaged
        .path()
        .join("secret.txt")
        .to_string_lossy()
        .into_owned();

    let mut session = TestSession::start_normal_at(startup.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    let requests = [
        ("read", json!({ "path": outside })),
        ("grep", json!({ "path": outside, "pattern": "agentshim" })),
        ("glob", json!({ "path": outside, "pattern": "**/*" })),
    ];

    for (offset, (name, arguments)) in requests.into_iter().enumerate() {
        let id = u64::try_from(offset).expect("request offset") + 2;
        let response = session.call_tool(id, name, arguments);
        assert_eq!(response["id"], id);
        assert_eq!(response["result"]["isError"], true);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .expect("error text")
                .contains("outside the repository root"),
            "unexpected {name} response: {response}"
        );
    }

    session.close();
}

#[test]
fn unrestricted_scope_reads_searches_and_globs_outside_the_startup_root() {
    let fixture = tempfile::tempdir().expect("outside fixture");
    std::fs::write(fixture.path().join("alpha.rs"), "pub fn needle() {}\n").expect("source");
    std::fs::write(fixture.path().join("beta.txt"), "needle\n").expect("text");
    let base = fixture.path().to_string_lossy().into_owned();
    let source = fixture
        .path()
        .join("alpha.rs")
        .to_string_lossy()
        .into_owned();

    let mut session = TestSession::start_unrestricted();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let read = session.call_tool(2, "read", json!({ "path": source }));
    assert_eq!(read["result"]["isError"], false);
    assert!(
        read["result"]["content"][0]["text"]
            .as_str()
            .expect("read text")
            .contains("pub fn needle")
    );

    let grep = session.call_tool(
        3,
        "grep",
        json!({ "path": base, "pattern": "needle", "glob": "*.rs" }),
    );
    assert_eq!(grep["result"]["isError"], false);
    let grep_text = grep["result"]["content"][0]["text"]
        .as_str()
        .expect("grep text");
    assert!(grep_text.contains("alpha.rs"));
    assert!(!grep_text.contains("beta.txt"));

    let glob = session.call_tool(4, "glob", json!({ "path": base, "pattern": "*.rs" }));
    assert_eq!(glob["result"]["isError"], false);
    let glob_text = glob["result"]["content"][0]["text"]
        .as_str()
        .expect("glob text");
    assert!(glob_text.contains("alpha.rs"));
    assert!(!glob_text.contains("beta.txt"));

    session.close();
}
