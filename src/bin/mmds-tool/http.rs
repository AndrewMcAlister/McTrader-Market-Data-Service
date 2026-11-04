//! The one HTTP client the probe commands share.
//!
//! Every command that talks to a running service goes through [`Api`], so a timeout, a
//! transport failure and a non-JSON body are rendered the same way everywhere. The client never
//! follows the service's own quirks: it returns the status and the body verbatim and lets each
//! command decide what a good answer looks like.

use std::time::Duration;

use reqwest::Method;
use serde_json::{Map, Value};

use crate::ToolResult;

/// One answer from the running service, with the body still unparsed.
#[derive(Debug, Clone)]
pub(crate) struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body, clipped so a stray HTML error page cannot fill the terminal.
    pub fn excerpt(&self) -> String {
        let body = self.body.trim();
        if body.chars().count() <= 160 {
            return body.to_string();
        }
        let clipped: String = body.chars().take(160).collect();
        format!("{clipped}...")
    }
}

/// A client pinned to one base URL.
#[derive(Clone)]
pub(crate) struct Api {
    base: String,
    client: reqwest::Client,
}

impl Api {
    pub fn new(base: impl Into<String>) -> ToolResult<Self> {
        Ok(Self {
            base: base.into(),
            client: reqwest::Client::builder().build()?,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub async fn get(&self, path: &str, timeout: Duration) -> Result<Reply, String> {
        self.request(Method::GET, path, None, timeout).await
    }

    pub async fn post(&self, path: &str, body: &Value, timeout: Duration) -> Result<Reply, String> {
        self.request(Method::POST, path, Some(body), timeout).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Reply, String> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .timeout(timeout);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|error| error.without_url().to_string())?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|error| error.without_url().to_string())?;
        Ok(Reply { status, body })
    }
}

/// A field as an operator wants to read it: a missing or null field says `null`.
pub(crate) fn show(value: &Value, key: &str) -> String {
    match value.get(key) {
        None | Some(Value::Null) => "null".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// A nested field, or `None` when any step on the path is missing.
pub(crate) fn at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

/// The entries of a nested object in the order serde_json keeps them. A missing object is an
/// empty map rather than an error: the probes only summarise what the service sent.
pub(crate) fn entries<'a>(
    value: &'a Value,
    path: &[&str],
) -> impl Iterator<Item = (&'a String, &'a Value)> {
    at(value, path)
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(Map::iter)
}

/// The status code as the operator reads it in a probe table.
pub(crate) fn status_text(reply: &Reply) -> String {
    reply.status.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.to_string(),
        }
    }

    #[test]
    fn a_field_renders_scalars_and_says_null_when_absent() {
        let value = serde_json::json!({"price": 231.4, "symbol": "AAPL", "missing": null});
        assert_eq!(show(&value, "price"), "231.4");
        assert_eq!(show(&value, "symbol"), "AAPL");
        assert_eq!(show(&value, "missing"), "null");
        assert_eq!(show(&value, "absent"), "null");
    }

    #[test]
    fn a_nested_path_walks_or_reports_nothing() {
        let value = serde_json::json!({"router": {"outcomes": {"eodhd": 3}}});
        assert_eq!(
            at(&value, &["router", "outcomes", "eodhd"]),
            Some(&serde_json::json!(3))
        );
        assert_eq!(at(&value, &["router", "nope"]), None);
    }

    #[test]
    fn a_missing_object_has_no_entries() {
        let value = serde_json::json!({"router": {}});
        assert_eq!(entries(&value, &["router", "outcomes"]).count(), 0);
    }

    #[test]
    fn a_long_body_is_clipped_for_display() {
        let body = "x".repeat(400);
        let excerpt = reply(&body).excerpt();
        assert!(excerpt.ends_with("..."));
        assert_eq!(excerpt.chars().count(), 163);
    }
}
