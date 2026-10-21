use crate::commands::run::actions::OutputValue;
use crate::config::BrowserlessAction;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::debug;

/// Endpoint used when the action does not set `url`.
const DEFAULT_URL: &str = "https://production-sfo.browserless.io/stealth/bql";

/// Executes a browserless BQL query against `action.url` (default endpoint
/// when unset).
///
/// Sessions are explicit: to keep using one browser, the query requests a
/// `reconnect` field, a `mapper` stores its `browserQLEndpoint` under a key,
/// and the next action sets `url = "{{ ctx.<id>.<key> }}"`, which is rendered
/// before this function runs.
///
/// Returns the uniform output map holding the BQL response `data` object under
/// the `data` key. `mapper` extraction is applied afterwards by
/// [`TaskContext::record_output`](super::TaskContext::record_output), like for
/// every other action.
pub(super) async fn execute_browserless_action(
    action: &BrowserlessAction,
    variables: &BTreeMap<String, String>,
) -> Result<OutputValue> {
    let base_url = action.url.as_deref().unwrap_or(DEFAULT_URL);

    let payload = run_query(
        base_url,
        &action.token,
        &action.query,
        &action.operation_name,
        variables,
    )
    .await
    .context("failed to run browserless bql query")?;
    debug!("browserless response: {payload}");

    let mut response = parse_response(&payload)?;

    // The BQL response `data` object is the action's output.
    let mut map = serde_json::Map::new();
    map.insert("data".to_string(), response["data"].take());
    Ok(OutputValue { data: map })
}

/// Runs `query` against `base_url` and returns the raw response payload.
async fn run_query(
    base_url: &str,
    token: &str,
    query: &str,
    operation_name: &str,
    variables: &BTreeMap<String, String>,
) -> Result<String> {
    let request_url = append_token(base_url, token)?;
    let body = browserless_request_body(query, operation_name, variables);

    let response = reqwest::Client::new()
        .post(request_url)
        .json(&body)
        .timeout(Duration::from_secs(180))
        .send()
        .await
        .context("failed to call browserless bql endpoint")?;

    let status = response.status();
    let payload = response
        .text()
        .await
        .context("failed to read browserless response body")?;

    ensure!(
        status.is_success(),
        "browserless bql returned {status}: {payload:#}"
    );

    Ok(payload)
}

/// Appends `token` as a query parameter without clobbering existing ones
/// (browserless reconnect URLs already carry their own query string).
fn append_token(base: &str, token: &str) -> Result<String> {
    let mut url =
        reqwest::Url::parse(base).with_context(|| format!("invalid browserless url: {base}"))?;
    url.query_pairs_mut().append_pair("token", token);
    Ok(url.to_string())
}

/// Parses a BQL response, failing on GraphQL-level errors (browserless reports
/// those with HTTP 200).
fn parse_response(payload: &str) -> Result<serde_json::Value> {
    let json: serde_json::Value =
        serde_json::from_str(payload).context("failed to parse browserless response as JSON")?;

    if let Some(errors) = json.get("errors") {
        bail!("browserless bql returned errors: {errors:#?}");
    }
    Ok(json)
}

/// Builds the GraphQL request body: `{"query": ..., "variables": ..., "operationName": ...}`.
/// `variables` is only included when the action configures some.
fn browserless_request_body(
    query: &str,
    operation_name: &str,
    variables: &BTreeMap<String, String>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "query": query,
        "operationName": operation_name,
    });
    if !variables.is_empty() {
        body["variables"] = serde_json::json!(variables);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::{append_token, browserless_request_body, parse_response};
    use std::collections::BTreeMap;

    #[test]
    fn appends_token_without_clobbering_existing_query() {
        let token = format!("tok {}&1", "en");
        let url = append_token(
            "wss://production-sfo.browserless.io/chromium/bql?session=abc&keep=1",
            &token,
        )
        .expect("url should build");
        assert_query_pair(&url, "token", &token);
        assert_query_pair(&url, "session", "abc");
        assert_query_pair(&url, "keep", "1");
    }

    /// Asserts `url` carries `key=value` as a query pair, independent of how the
    /// value happens to be percent-encoded.
    fn assert_query_pair(url: &str, key: &str, value: &str) {
        let parsed = reqwest::Url::parse(url).expect("url should parse");
        let pairs = parsed
            .query_pairs()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<Vec<_>>();
        assert!(
            pairs.contains(&(key.to_string(), value.to_string())),
            "query pairs were: {pairs:?}"
        );
    }

    #[test]
    fn builds_browserless_request_body_with_variables() {
        let variables = BTreeMap::from([(
            "url".to_string(),
            "https://example.com/dashboard".to_string(),
        )]);
        let body = browserless_request_body(
            "mutation zpr($url: String!) { goto(url: $url) { status } }",
            "zpr",
            &variables,
        );
        assert_eq!(
            body,
            serde_json::json!({
                "query": "mutation zpr($url: String!) { goto(url: $url) { status } }",
                "variables": { "url": "https://example.com/dashboard" },
                "operationName": "zpr",
            })
        );
    }

    #[test]
    fn builds_browserless_request_body_without_variables() {
        let body = browserless_request_body(
            "query { text(selector: \"body\") { text } }",
            "",
            &BTreeMap::new(),
        );
        assert_eq!(
            body,
            serde_json::json!({
                "query": "query { text(selector: \"body\") { text } }",
                "operationName": "",
            })
        );
        assert!(body.get("variables").is_none());
    }

    #[test]
    fn extracts_reconnect_url_from_response() {
        let response = r#"{
            "data": {
                "goto": { "status": 200 },
                "reconnect": {
                    "browserQLEndpoint": "wss://production-sfo.browserless.io/chromium/bql?token=abc&session=xyz"
                }
            }
        }"#;

        let json = parse_response(response).expect("should parse response");
        assert_eq!(
            json["data"]["reconnect"]["browserQLEndpoint"],
            "wss://production-sfo.browserless.io/chromium/bql?token=abc&session=xyz"
        );
    }

    #[test]
    fn fails_on_graphql_errors_in_response() {
        let response = r#"{ "errors": [{ "message": "session expired" }] }"#;
        assert!(parse_response(response).is_err());
    }

    #[test]
    fn accepts_response_without_reconnect_field() {
        let response = r#"{ "data": { "goto": { "status": 200 } } }"#;
        assert!(parse_response(response).is_ok());
    }
}
