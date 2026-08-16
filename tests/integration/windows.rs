use super::common::{fixtures::*, session::*};
use super::*;

#[test]
fn cargo_multicall_proxy_reports_identity_on_nonzero_exit() {
    let mut session = TestSession::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    let mut call = empty_params();
    call.insert("name".to_owned(), json!("run_program"));
    call.insert(
        "arguments".to_owned(),
        json!({
            "program": "cargo",
            "args": ["agentshim-definitely-not-a-command"],
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "timeout_ms": 30_000
        }),
    );
    session.send(&modern_request(2, "tools/call", call));

    let response = session.receive();
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["isError"], false);
    let output = response["result"]["content"][0]["text"]
        .as_str()
        .expect("process output");
    let resolved = output
        .lines()
        .find_map(|line| line.strip_prefix("Resolved program: "))
        .expect("resolved program line");
    assert_eq!(
        std::path::Path::new(resolved)
            .file_stem()
            .and_then(|name| name.to_str()),
        Some("cargo")
    );
    assert!(output.contains("Launcher: native"));
    assert!(output.contains("Exit code: 101"));
    session.close();
}
