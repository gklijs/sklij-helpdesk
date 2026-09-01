//! A thin GraphQL client - one function, since this app only ever needs
//! `submitCommand` and `projection`. No typed schema/codegen: the
//! backend's own GraphQL surface is generated at runtime from whatever's
//! registered (see `../Cargo.toml`'s own doc comment on that), so a
//! generated client would need to regenerate itself against a running
//! server anyway - not worth it for an app this size.

use crate::config::GRAPHQL_URL;
use serde_json::Value;

pub async fn graphql(token: &str, query: &str) -> Result<Value, String> {
    let body = serde_json::json!({ "query": query }).to_string();
    let response = gloo_net::http::Request::post(GRAPHQL_URL)
        .header("authorization", &format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| format!("network error talking to {GRAPHQL_URL}: {e}"))?;

    let json: Value = response
        .json()
        .await
        .map_err(|e| format!("couldn't decode the response as JSON: {e}"))?;

    if let Some(errors) = json.get("errors") {
        return Err(format!("GraphQL returned errors: {errors}"));
    }
    Ok(json)
}

/// `submitCommand(boundedContext, commandTypeName, payload)` -
/// `payload` is a raw JSON *string* argument (see `skilj-graphql`'s own
/// `submit_command_field` doc comment - a wire-shape choice on skilj's
/// side, not this app's), so this double-encodes it the same way
/// `tests/support/mod.rs::submit_command_mutation` does on the backend's
/// own test side.
pub async fn submit_command(
    token: &str,
    bounded_context: &str,
    command_type_name: &str,
    payload: &Value,
) -> Result<Value, String> {
    let payload_json = serde_json::to_string(payload).map_err(|e| e.to_string())?;
    let payload_literal = serde_json::to_string(&payload_json).map_err(|e| e.to_string())?;
    let query = format!(
        "mutation {{ submitCommand(boundedContext: {bounded_context:?}, commandTypeName: {command_type_name:?}, payload: {payload_literal}) {{ accepted rejectionReason rejectionKind }} }}"
    );
    let response = graphql(token, &query).await?;
    let result = &response["data"]["submitCommand"];
    if result["accepted"].as_bool() == Some(true) {
        Ok(result.clone())
    } else {
        Err(result["rejectionReason"]
            .as_str()
            .unwrap_or("rejected, no reason given")
            .to_string())
    }
}

/// `projection(boundedContext, name, key)` - the result comes back as
/// an opaque, JSON-encoded *string* rather than a typed object (found
/// while wiring this up: `schemars`-derived shapes like the enum/map
/// fields these projections use don't match what `skilj-graphql`'s
/// mapper recognises as a scalar - its own documented fallback, not a
/// bug to work around here), so this parses it a second time.
pub async fn query_projection(
    token: &str,
    bounded_context: &str,
    name: &str,
    key: &str,
    graphql_type: &str,
    field: &str,
) -> Result<Value, String> {
    let query = format!(
        "query {{ projection(boundedContext: {bounded_context:?}, name: {name:?}, key: {key:?}) {{ ... on {graphql_type} {{ {field} }} }} }}"
    );
    let response = graphql(token, &query).await?;
    let raw = response["data"]["projection"][field]
        .as_str()
        .ok_or_else(|| format!("no {field:?} field in the projection response: {response}"))?;
    serde_json::from_str(raw).map_err(|e| format!("couldn't parse the projection's own JSON: {e}"))
}
