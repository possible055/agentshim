use codexshim::server::CodexShim;

fn assert_snapshot(actual: impl serde::Serialize, expected: &str) {
    let actual = serde_json::to_value(actual).expect("serialize snapshot");
    let expected: serde_json::Value = serde_json::from_str(expected).expect("parse snapshot");
    assert_eq!(actual, expected);
}

#[test]
fn server_discover_snapshot() {
    assert_snapshot(
        CodexShim::discovery_result(),
        include_str!("snapshots/server_discover.json"),
    );
}

#[test]
fn tools_list_snapshot() {
    assert_snapshot(
        CodexShim::tools_result(),
        include_str!("snapshots/tools_list.json"),
    );
}
