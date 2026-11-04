//! The MCP binary, driven the way a client drives it: spawn over stdio, list, call.
//!
//! The REST service is a `wiremock` server, so this test proves the protocol surface -- the
//! handshake, the five advertised tools, their argument schemas and the structured payloads --
//! without a database, without a vendor key and without spending a unit.
//!
//! A client that could list the tools but not call them would still be broken, so every tool is
//! called and its payload checked. The proxy's own failure shapes are covered by the unit tests
//! in `src/mcp.rs`.

use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde_json::{json, Map, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The five tools the source server advertised, sorted.
const TOOLS: [&str; 5] = [
    "get_atr",
    "get_bars",
    "get_price",
    "get_prices",
    "get_service_status",
];

#[tokio::test]
async fn a_client_lists_and_calls_every_tool() {
    let server = MockServer::start().await;
    let calls = [
        (method("GET"), "/price/AAPL", "get_price"),
        (method("POST"), "/prices", "get_prices"),
        (method("POST"), "/atr", "get_atr"),
        (method("POST"), "/bars", "get_bars"),
        (method("GET"), "/stats", "get_service_status"),
    ];
    for (verb, route, tool) in calls {
        Mock::given(verb)
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"tool": tool, "price": 231.4})),
            )
            .mount(&server)
            .await;
    }

    let transport = TokioChildProcess::builder(child_command(&server.uri()))
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
        .0;
    let client = ().serve(transport).await.unwrap();

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, TOOLS);

    for (name, arguments) in tool_calls() {
        let result = client
            .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{name} reported an error");
        let payload = result
            .structured_content
            .unwrap_or_else(|| panic!("{name} returned no structured content"));
        assert_eq!(payload["tool"], name, "{name} reached the wrong endpoint");
    }

    client.cancel().await.unwrap();
}

/// The sibling binary Cargo built for this test.
fn child_command(base_url: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_mmds-mcp"));
    command
        .env("MDS_MCP_TRANSPORT", "stdio")
        .env("MDS_MCP_BASE_URL", base_url)
        .env_remove("MDS_MCP_TIMEOUT");
    command
}

/// One call per tool, with the shape the source tool used.
fn tool_calls() -> [(&'static str, Map<String, Value>); 5] {
    [
        ("get_price", body(json!({"symbol": "AAPL", "venue": "US"}))),
        (
            "get_prices",
            body(json!({"symbols": ["AAPL", "MSFT"], "venue": "US"})),
        ),
        ("get_atr", body(json!({"symbols": ["AAPL"], "period": 14}))),
        (
            "get_bars",
            body(json!({"symbols": ["AAPL"], "sessions": 5})),
        ),
        ("get_service_status", body(json!({}))),
    ]
}

fn body(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(object) => object,
        _ => Map::new(),
    }
}
