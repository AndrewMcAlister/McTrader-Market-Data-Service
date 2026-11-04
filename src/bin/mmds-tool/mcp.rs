//! `check-mcp`: start the real MCP server, list its tools, and call every one.
//!
//! This is the acceptance check for the MCP surface, not a test. It launches the sibling
//! `mmds-mcp` binary over stdio, so the handshake, the advertised schemas and the proxy are
//! exercised exactly as a client would exercise them. The tools forward to the running REST
//! service, and some of them spend vendor units when the cache is cold -- which is why this is
//! an operator command.
//!
//! A missing tool is a failure; an extra tool is reported but not fatal, because a later
//! server may add one without making this command wrong.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde_json::{json, Map, Value};

use crate::http::{entries, show};
use crate::ToolResult;

/// The five tools the source server advertised.
const EXPECTED_TOOLS: [&str; 5] = [
    "get_atr",
    "get_bars",
    "get_price",
    "get_prices",
    "get_service_status",
];

/// A tool that never answers must not hang the operator's terminal.
const CONVERSATION_TIMEOUT: Duration = Duration::from_secs(300);

/// One request per tool, the same shapes the source tool checked.
fn calls() -> [(&'static str, Value); 5] {
    [
        ("get_price", json!({"symbol": "AAPL", "venue": "US"})),
        (
            "get_prices",
            json!({"symbols": ["AAPL", "MSFT"], "venue": "US"}),
        ),
        ("get_atr", json!({"symbols": ["AAPL"], "period": 14})),
        ("get_bars", json!({"symbols": ["AAPL"], "sessions": 5})),
        ("get_service_status", json!({})),
    ]
}

pub(crate) async fn check_mcp(base: &str) -> ToolResult<()> {
    let program = mcp_binary()?;
    println!("MCP server: {}", program.display());
    println!("REST base:  {base}");
    tokio::time::timeout(CONVERSATION_TIMEOUT, conversation(&program, base))
        .await
        .map_err(|_| {
            format!(
                "the MCP conversation did not finish within {} s",
                CONVERSATION_TIMEOUT.as_secs()
            )
        })?
}

async fn conversation(program: &Path, base: &str) -> ToolResult<()> {
    let transport = TokioChildProcess::new(command(program, base))?;
    let client = ().serve(transport).await?;

    let tools = client.list_all_tools().await?;
    let mut names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
    names.sort();
    print_tools(&names);
    require_expected(&names)?;

    println!("\nproxy check:");
    let mut failures = 0;
    for (name, arguments) in calls() {
        match client
            .call_tool(CallToolRequestParams::new(name).with_arguments(arguments_body(&arguments)))
            .await
        {
            Ok(result) => failures += report_call(name, &result),
            Err(error) => {
                println!("  {name:<20} MCP ERROR {error}");
                failures += 1;
            }
        }
    }

    client.cancel().await?;
    if failures > 0 {
        return Err(format!("{failures} tool call(s) failed").into());
    }
    println!("\nPASS: the MCP client listed and called every tool");
    Ok(())
}

/// Force stdio: an ambient `MDS_MCP_TRANSPORT` must not turn the child into an HTTP server
/// that never answers the handshake.
fn command(program: &Path, base: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command
        .env("MDS_MCP_TRANSPORT", "stdio")
        .env("MDS_MCP_BASE_URL", base);
    command
}

/// The binary Cargo put next to this one.
fn mcp_binary() -> ToolResult<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .ok_or("the tool executable has no parent directory")?;
    let candidate = directory.join(format!("mmds-mcp{}", std::env::consts::EXE_SUFFIX));
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(format!(
        "{} not found; build it with `cargo build --release`",
        candidate.display()
    )
    .into())
}

fn arguments_body(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(object) => object.clone(),
        _ => Map::new(),
    }
}

fn print_tools(names: &[String]) {
    println!("MCP tools registered ({}):", names.len());
    for name in names {
        println!("  - {name}");
    }
}

/// Every expected tool must be there. Extras are the server's business, not this check's.
fn require_expected(names: &[String]) -> ToolResult<()> {
    let missing: Vec<&str> = EXPECTED_TOOLS
        .iter()
        .copied()
        .filter(|expected| !names.iter().any(|name| name == expected))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!("MCP server does not advertise: {}", missing.join(", ")).into())
}

/// Print one call's outcome; return 1 when it failed.
fn report_call(name: &str, result: &CallToolResult) -> usize {
    match failure(result) {
        Some(error) => {
            println!("  {name:<20} ERROR {error}");
            1
        }
        None => {
            let payload = result.structured_content.as_ref().unwrap_or(&Value::Null);
            println!("  {name:<20} ok, {}", summary(name, payload));
            0
        }
    }
}

/// The reason a tool call failed, in the source tool's `ERROR <reason>: <detail>` shape.
fn failure(result: &CallToolResult) -> Option<String> {
    if result.is_error == Some(true) {
        return Some("the tool reported is_error".to_string());
    }
    let payload = result.structured_content.as_ref()?;
    let error = payload.get("error")?.as_str()?;
    match payload.get("detail") {
        Some(detail) if !detail.is_null() => Some(format!("{error}: {}", detail_text(detail))),
        _ => Some(error.to_string()),
    }
}

/// The detail as an operator reads it: raw text for a string, JSON otherwise, clipped so one
/// runaway body cannot fill the terminal.
fn detail_text(detail: &Value) -> String {
    let text = match detail {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    text.chars().take(90).collect()
}

/// A one-line, symbol-agnostic summary of the payload the service returned.
fn summary(name: &str, payload: &Value) -> String {
    match name {
        "get_price" => format!(
            "provider={} price={}",
            show(payload, "provider"),
            show(payload, "price")
        ),
        "get_prices" => format!(
            "venue={} route={} quotes={} missing={}",
            show(payload, "venue"),
            show(payload, "route"),
            entries(payload, &["quotes"]).count(),
            show(payload, "missing")
        ),
        "get_atr" => {
            let reading = first_entry(payload, "atr");
            format!(
                "value={} method={} as_of={}",
                show(&reading, "value"),
                show(&reading, "method"),
                show(&reading, "as_of_session")
            )
        }
        "get_bars" => {
            let entry = first_entry(payload, "bars");
            format!(
                "sessions={} as_of={}",
                show(&entry, "sessions"),
                show(&entry, "as_of_session")
            )
        }
        "get_service_status" => {
            let vendor = payload.get("vendor").cloned().unwrap_or(Value::Null);
            format!(
                "database_ok={} units={}",
                show(payload, "database_ok"),
                show(&vendor, "units")
            )
        }
        _ => "answered".to_string(),
    }
}

/// The first symbol's entry under `key`, or null when the service sent none.
fn first_entry(payload: &Value, key: &str) -> Value {
    entries(payload, &[key])
        .next()
        .map(|(_, entry)| entry.clone())
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn a_missing_tool_fails_the_check_and_an_extra_tool_does_not() {
        assert!(require_expected(&names(&EXPECTED_TOOLS)).is_ok());
        assert!(require_expected(&[
            "get_atr".into(),
            "get_bars".into(),
            "get_price".into(),
            "get_prices".into(),
            "get_service_status".into(),
            "get_future".into(),
        ])
        .is_ok());
        let error = require_expected(&names(&["get_price"])).unwrap_err();
        assert!(error.to_string().contains("get_atr"));
    }

    #[test]
    fn a_payload_with_an_error_is_a_failure() {
        let result = CallToolResult::structured(json!({
            "error": "service-unreachable",
            "detail": "connection refused"
        }));
        assert_eq!(
            failure(&result).unwrap(),
            "service-unreachable: connection refused"
        );
    }

    #[test]
    fn a_successful_payload_is_not_a_failure_even_with_a_null_error_field() {
        let result = CallToolResult::structured(json!({"price": 231.4, "error": null}));
        assert_eq!(failure(&result), None);
    }

    #[test]
    fn a_non_string_detail_is_rendered_as_json_and_clipped() {
        let result = CallToolResult::structured(json!({
            "error": "http-error",
            "detail": {"body": "x".repeat(200)}
        }));
        let message = failure(&result).unwrap();
        assert!(message.starts_with("http-error: {"));
        assert_eq!(message.chars().count(), "http-error: ".len() + 90);
    }

    #[test]
    fn a_tool_level_error_is_a_failure() {
        let result = CallToolResult::structured_error(json!({"reason": "bad argument"}));
        assert!(failure(&result).is_some());
    }

    #[test]
    fn summaries_read_the_contract_fields() {
        let price = json!({"provider": "eodhd", "price": 231.4});
        assert_eq!(summary("get_price", &price), "provider=eodhd price=231.4");

        let bars = json!({"bars": {"AAPL": {"sessions": 5, "as_of_session": "2026-11-03"}}});
        assert_eq!(summary("get_bars", &bars), "sessions=5 as_of=2026-11-03");
    }

    #[test]
    fn an_unknown_tool_still_gets_a_summary() {
        assert_eq!(summary("get_future", &json!({})), "answered");
    }
}
