use super::*;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConstraintFixture {
    timeout_ceiling_ms: u64,
    cases: Vec<ConstraintCase>,
}

#[derive(Deserialize)]
struct ConstraintCase {
    id: String,
    tool: String,
    rule: String,
    field: String,
    args: serde_json::Value,
}

fn constraint_fixture() -> ConstraintFixture {
    serde_json::from_str(include_str!("../../../evals/host-constraints.json"))
        .expect("shared constraint fixture")
}

fn core_rejects_constraint(case: &ConstraintCase, timeout_ceiling_ms: u64) -> bool {
    let arguments = case.args.as_object().cloned();
    match case.tool.as_str() {
        "read" => parse_request::<crate::tools::read::ReadRequest>(arguments, "read")
            .and_then(|request| request.validate().map_err(|error| error.to_string()))
            .is_err(),
        "grep" => parse_request::<crate::tools::grep::GrepRequest>(arguments, "grep")
            .and_then(|request| request.validate().map_err(|error| error.to_string()))
            .is_err(),
        "glob" => parse_request::<crate::tools::glob::GlobRequest>(arguments, "glob")
            .and_then(|request| request.validate().map_err(|error| error.to_string()))
            .is_err(),
        "run_program" => {
            parse_request::<crate::tools::run_program::ProcessRequest>(arguments, "run_program")
                .and_then(|request| {
                    request
                        .validate(timeout_ceiling_ms)
                        .map_err(|error| error.to_string())
                })
                .is_err()
        }
        other => panic!("unknown constraint fixture tool {other}"),
    }
}

fn string_array(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .expect("string array")
        .iter()
        .map(|value| value.as_str().expect("string").to_owned())
        .collect()
}

#[test]
fn shared_constraints_match_mcp_catalog_and_core_validation() {
    let fixture = constraint_fixture();
    let root = tempfile::tempdir().expect("root");
    let server = AgentShim::from_path(root.path()).expect("server");

    for case in &fixture.cases {
        assert!(
            core_rejects_constraint(case, fixture.timeout_ceiling_ms),
            "core accepted shared invalid case {}",
            case.id
        );

        let tool = rmcp::ServerHandler::get_tool(&server, &case.tool)
            .unwrap_or_else(|| panic!("missing MCP catalog tool {}", case.tool));
        let value = serde_json::to_value(tool).expect("serialize tool");
        let schema = &value["inputSchema"];
        let property = &schema["properties"][&case.field];
        match case.rule.as_str() {
            "required" => assert!(
                string_array(&schema["required"]).contains(&case.field),
                "catalog required mismatch for {}",
                case.id
            ),
            "non_empty" => assert_eq!(
                property["minLength"],
                json!(1),
                "catalog non-empty mismatch for {}",
                case.id
            ),
            "range" | "timeout" => {
                let candidate = case.args[&case.field].as_u64().expect("range candidate");
                let below_minimum = property["minimum"]
                    .as_u64()
                    .is_some_and(|minimum| candidate < minimum);
                let above_maximum = property["maximum"]
                    .as_u64()
                    .is_some_and(|maximum| candidate > maximum);
                assert!(
                    below_minimum || above_maximum,
                    "catalog range mismatch for {}",
                    case.id
                );
            }
            "unknown" => {
                assert_eq!(schema["additionalProperties"], json!(false));
                assert!(schema["properties"].get(case.field.as_str()).is_none());
            }
            "cross_field" => {
                for field in case.args.as_object().expect("arguments").keys() {
                    assert!(
                        schema["properties"].get(field.as_str()).is_some(),
                        "catalog lost cross-field input {field} for {}",
                        case.id
                    );
                }
            }
            other => panic!("unknown constraint fixture rule {other}"),
        }
    }
}

#[test]
fn mcp_catalog_matches_host_divergence_snapshot() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../../../evals/host-divergence.json"))
            .expect("host divergence fixture");
    let root = tempfile::tempdir().expect("root");
    let server = AgentShim::from_path(root.path()).expect("server");

    let bash = rmcp::ServerHandler::get_tool(&server, "bash").expect("bash");
    let bash = serde_json::to_value(bash).expect("serialize bash");
    let actual_variants = bash["inputSchema"]["oneOf"]
        .as_array()
        .expect("bash variants")
        .iter()
        .map(|variant| {
            let mut fields = variant["properties"]
                .as_object()
                .expect("bash properties")
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            fields.sort();
            let mut required = string_array(&variant["required"]);
            required.sort();
            (fields, required)
        })
        .collect::<Vec<_>>();
    let expected_variants = expected["bash"]["mcp"]["variants"]
        .as_array()
        .expect("expected bash variants")
        .iter()
        .map(|variant| {
            let mut fields = string_array(&variant["fields"]);
            fields.sort();
            let mut required = string_array(&variant["required"]);
            required.sort();
            (fields, required)
        })
        .collect::<Vec<_>>();
    assert_eq!(actual_variants, expected_variants);

    let status = rmcp::ServerHandler::get_tool(&server, "bash_status").expect("bash_status");
    let status = serde_json::to_value(status).expect("serialize bash_status");
    let mut fields = status["inputSchema"]["properties"]
        .as_object()
        .expect("bash_status properties")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    fields.sort();
    let mut expected_fields = string_array(&expected["bash_status"]["mcp"]["fields"]);
    expected_fields.sort();
    assert_eq!(fields, expected_fields);
    assert_eq!(
        string_array(&status["inputSchema"]["required"]),
        string_array(&expected["bash_status"]["mcp"]["required"])
    );
}
