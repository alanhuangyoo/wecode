use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, header::HeaderMap};
use serde::Serialize;
use serde_json::Value;

pub async fn send_json<T: Serialize + ?Sized>(
    client: &Client,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: &T,
) -> Result<Value> {
    let mut delay = Duration::from_millis(500);
    for attempt in 0..4 {
        let response = client
            .request(method.clone(), url)
            .headers(headers.clone())
            .json(body)
            .send()
            .await
            .with_context(|| format!("request to {url} failed"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read model response")?;

        if status.is_success() {
            return serde_json::from_str(&text)
                .with_context(|| format!("provider returned invalid JSON: {}", excerpt(&text)));
        }
        if attempt < 3
            && (status.as_u16() == 408
                || status.as_u16() == 409
                || status.as_u16() == 429
                || status.is_server_error())
        {
            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2);
            continue;
        }
        bail!(
            "provider returned HTTP {}: {}",
            status,
            excerpt(text.trim())
        );
    }
    unreachable!("retry loop always returns or errors")
}

fn excerpt(value: &str) -> String {
    let mut result: String = value.chars().take(2_000).collect();
    if value.chars().count() > 2_000 {
        result.push_str("...");
    }
    result
}
