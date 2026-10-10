use crate::config::BrowserlessAction;
use anyhow::{Context, Result, ensure};
use std::time::Duration;
use tracing::debug;

const DEFAULT_BROWSERLESS_URL: &str = "https://production-sfo.browserless.io/stealth/bql";

/// Executes a browserless browser action by POSTing a GraphQL (BQL) request to Browserless.
///
/// Request shape (mirrors the documented curl example):
///   POST <url>?token=<token>&blockConsentModals=true&emulationOs=android&emulatedDevice=pixel-8
///   Content-Type: application/json
///   {"query": ..., "variables": ..., "operationName": ...}
pub(super) async fn execute_browserless_action(
    action: &BrowserlessAction,
    url_from_button: Option<String>,
) -> Result<()> {
    debug!("{:?}", url_from_button);

    let url = browserless_request_url(
        action.url.as_deref().unwrap_or(DEFAULT_BROWSERLESS_URL),
        &action.token,
    )?;

    let body = browserless_request_body(action, url_from_button);

    let response = reqwest::Client::new()
        .post(url)
        .json(&body)
        .timeout(Duration::from_mins(3))
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

    debug!("browserless return {}", payload);

    Ok(())
}

/// Builds the GraphQL request body: `{"query": ..., "variables": ..., "operationName": ...}`.
/// `variables.url` is only included when a preceding `click` opened a URL button.
fn browserless_request_body(
    action: &BrowserlessAction,
    url_from_button: Option<String>,
) -> serde_json::Value {
    match url_from_button {
        Some(url) => serde_json::json!({
            "query": action.query,
            "variables": { "url": url },
            "operationName": action.operation_name,
        }),
        None => serde_json::json!({
            "query": action.query,
            "operationName": action.operation_name,
        }),
    }
}

/// Builds `<base>?token=<token>&blockConsentModals=true&emulationOs=android&emulatedDevice=pixel-8`.
fn browserless_request_url(base: &str, token: &str) -> Result<String> {
    let mut url =
        reqwest::Url::parse(base).with_context(|| format!("invalid browserless url: {base}"))?;
    url.query_pairs_mut()
        .append_pair("token", token)
        .append_pair("blockConsentModals", "true")
        .append_pair("emulationOs", "android")
        .append_pair("emulatedDevice", "pixel-8");
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::{browserless_request_body, browserless_request_url, execute_browserless_action};
    use crate::config::BrowserlessAction;

    #[test]
    fn builds_default_browserless_request_url() {
        let url = browserless_request_url(
            "https://production-sfo.browserless.io/stealth/bql",
            "my-token",
        )
        .expect("url should build");
        assert_eq!(
            url,
            "https://production-sfo.browserless.io/stealth/bql?token=my-token&blockConsentModals=true&emulationOs=android&emulatedDevice=pixel-8"
        );
    }

    #[test]
    fn encodes_token_in_browserless_request_url() {
        let url = browserless_request_url(
            "https://production-sfo.browserless.io/stealth/bql",
            "tok en&1",
        )
        .expect("url should build");
        assert!(url.contains("token=tok+en%261"), "url was: {url}");
    }

    #[test]
    fn builds_browserless_request_body_with_url_from_button() {
        let action = BrowserlessAction {
            token: "tok".to_string(),
            query: "mutation zpr($url: String!) { goto(url: $url) { status } }".to_string(),
            operation_name: "zpr".to_string(),
            url: None,
        };

        let body =
            browserless_request_body(&action, Some("https://example.com/dashboard".to_string()));
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
    fn builds_browserless_request_body_without_url() {
        let action = BrowserlessAction {
            token: "tok".to_string(),
            query: "query { text(selector: \"body\") { text } }".to_string(),
            operation_name: String::new(),
            url: None,
        };

        let body = browserless_request_body(&action, None);
        assert_eq!(
            body,
            serde_json::json!({
                "query": "query { text(selector: \"body\") { text } }",
                "operationName": "",
            })
        );
        assert!(body.get("variables").is_none());
    }

    #[tokio::test]
    async fn posts_browserless_request_to_bql_endpoint() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            // read one request
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buffer).to_string();
                let headers_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                let content_length = text
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-length"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().parse::<usize>().ok());
                if content_length.is_some_and(|len| buffer.len() >= headers_end + len) {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buffer).to_string();
            let response =
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
            socket.write_all(response).await.unwrap();
            socket.flush().await.unwrap();
            request
        });

        let action = BrowserlessAction {
            token: "my-secret-token".to_string(),
            query: "mutation zpr($url: String!) { goto(url: $url) { status } }".to_string(),
            operation_name: "zpr".to_string(),
            url: Some(format!("http://{addr}/stealth/bql")),
        };

        execute_browserless_action(&action, Some("https://example.com/dashboard".to_string()))
            .await
            .expect("browserless action should succeed");

        let request = server.await.unwrap();
        let request_line = request.lines().next().unwrap();
        assert!(
            request_line.starts_with("POST /stealth/bql?token=my-secret-token&blockConsentModals=true&emulationOs=android&emulatedDevice=pixel-8 HTTP/"),
            "request line was: {request_line}"
        );
        assert!(
            request.contains("content-type: application/json")
                || request.contains("Content-Type: application/json")
        );
        assert!(request.contains("\"operationName\":\"zpr\""));
        assert!(request.contains("\"variables\":{\"url\":\"https://example.com/dashboard\"}"));
    }
}
