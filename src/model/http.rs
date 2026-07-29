use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{
    Client, Method, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use serde::Serialize;
use serde_json::Value;

use crate::config::ModelConfig;

const MAX_SSE_FRAME_BYTES: usize = 16 * 1024 * 1024;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_EXPONENTIAL_DELAY: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    max_retries: usize,
    max_delay: Duration,
}

impl RetryPolicy {
    pub fn from_config(config: &ModelConfig) -> Self {
        Self {
            max_retries: config.request_max_retries,
            max_delay: Duration::from_secs(config.max_retry_delay_seconds),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

pub async fn send_json<T: Serialize + ?Sized>(
    client: &Client,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: &T,
    retry: RetryPolicy,
) -> Result<Value> {
    for attempt in 0..=retry.max_retries {
        let response = match client
            .request(method.clone(), url)
            .headers(headers.clone())
            .json(body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) if attempt < retry.max_retries && retryable_transport_error(&error) => {
                tokio::time::sleep(retry_delay(&HeaderMap::new(), attempt, retry)).await;
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("request to {url} failed"));
            }
        };
        let status = response.status();
        let response_headers = response.headers().clone();
        let text = match response.text().await {
            Ok(text) => text,
            Err(_) if attempt < retry.max_retries => {
                tokio::time::sleep(retry_delay(&response_headers, attempt, retry)).await;
                continue;
            }
            Err(error) => return Err(error).context("failed to read model response"),
        };

        if status.is_success() {
            return serde_json::from_str(&text)
                .with_context(|| format!("provider returned invalid JSON: {}", excerpt(&text)));
        }
        if attempt < retry.max_retries && retryable_status(status, &response_headers) {
            tokio::time::sleep(retry_delay(&response_headers, attempt, retry)).await;
            continue;
        }
        bail!(
            "provider request failed ({}, HTTP {}): {}",
            status_label(status),
            status,
            excerpt(text.trim())
        );
    }
    unreachable!("retry loop always returns or errors")
}

pub async fn send_sse<T, F>(
    client: &Client,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: &T,
    retry: RetryPolicy,
    mut on_event: F,
) -> Result<()>
where
    T: Serialize + ?Sized,
    F: FnMut(SseEvent) -> Result<()>,
{
    'request: for attempt in 0..=retry.max_retries {
        let response = match client
            .request(method.clone(), url)
            .headers(headers.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) if attempt < retry.max_retries && retryable_transport_error(&error) => {
                tokio::time::sleep(retry_delay(&HeaderMap::new(), attempt, retry)).await;
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("request to {url} failed"));
            }
        };
        let status = response.status();
        let response_headers = response.headers().clone();
        if status.is_success() {
            let mut decoder = SseDecoder::default();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(_) if decoder.events == 0 && attempt < retry.max_retries => {
                        tokio::time::sleep(retry_delay(&response_headers, attempt, retry)).await;
                        continue 'request;
                    }
                    Err(error) => {
                        return Err(error).context(
                            "provider event stream ended after output had already started; refusing to replay it",
                        );
                    }
                };
                decoder.push(&chunk, &mut on_event)?;
            }
            decoder.finish(&mut on_event)?;
            if decoder.events == 0 && attempt < retry.max_retries {
                tokio::time::sleep(retry_delay(&response_headers, attempt, retry)).await;
                continue;
            }
            if decoder.events == 0 {
                bail!("provider returned an empty event stream");
            }
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        if attempt < retry.max_retries && retryable_status(status, &response_headers) {
            tokio::time::sleep(retry_delay(&response_headers, attempt, retry)).await;
            continue;
        }
        bail!(
            "provider request failed ({}, HTTP {}): {}",
            status_label(status),
            status,
            excerpt(text.trim())
        );
    }
    unreachable!("retry loop always returns or errors")
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    events: usize,
}

impl SseDecoder {
    fn push<F>(&mut self, chunk: &[u8], on_event: &mut F) -> Result<()>
    where
        F: FnMut(SseEvent) -> Result<()>,
    {
        self.buffer.extend_from_slice(chunk);
        while let Some((end, delimiter_len)) = frame_boundary(&self.buffer) {
            let remaining = self.buffer.split_off(end + delimiter_len);
            let mut frame = std::mem::replace(&mut self.buffer, remaining);
            frame.truncate(end);
            if frame.len() > MAX_SSE_FRAME_BYTES {
                bail!("provider SSE frame exceeded the 16 MiB safety limit");
            }
            if let Some(event) = parse_sse_frame(&frame) {
                self.events = self.events.saturating_add(1);
                on_event(event)?;
            }
        }
        if self.buffer.len() > MAX_SSE_FRAME_BYTES {
            bail!("provider SSE frame exceeded the 16 MiB safety limit");
        }
        Ok(())
    }

    fn finish<F>(&mut self, on_event: &mut F) -> Result<()>
    where
        F: FnMut(SseEvent) -> Result<()>,
    {
        if let Some(event) = parse_sse_frame(&self.buffer) {
            self.events = self.events.saturating_add(1);
            on_event(event)?;
        }
        self.buffer.clear();
        Ok(())
    }
}

fn retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

fn retryable_status(status: StatusCode, headers: &HeaderMap) -> bool {
    match headers
        .get("x-should-retry")
        .and_then(|value| value.to_str().ok())
    {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    matches!(status.as_u16(), 408 | 409 | 425 | 429 | 529) || status.is_server_error()
}

fn retry_delay(headers: &HeaderMap, attempt: usize, retry: RetryPolicy) -> Duration {
    let requested = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_milliseconds)
        .or_else(|| {
            headers
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after)
        });
    requested
        .unwrap_or_else(|| jittered_exponential_delay(attempt))
        .min(retry.max_delay)
}

fn parse_milliseconds(value: &str) -> Option<Duration> {
    let milliseconds = value.trim().parse::<f64>().ok()?;
    (milliseconds.is_finite() && milliseconds >= 0.0)
        .then(|| Duration::from_secs_f64(milliseconds / 1_000.0))
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<f64>() {
        return (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds));
    }
    let timestamp = httpdate::parse_http_date(value).ok()?;
    Some(
        timestamp
            .duration_since(SystemTime::now())
            .unwrap_or_default(),
    )
}

fn jittered_exponential_delay(attempt: usize) -> Duration {
    let exponent = attempt.min(16) as u32;
    let delay = INITIAL_RETRY_DELAY
        .saturating_mul(2_u32.saturating_pow(exponent))
        .min(MAX_EXPONENTIAL_DELAY);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let percent = 75 + (nanos.wrapping_add(attempt as u32 * 17) % 26);
    delay.saturating_mul(percent) / 100
}

fn status_label(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "invalid request",
        401 => "authentication failed",
        403 => "permission denied",
        404 => "endpoint or model not found",
        408 => "request timeout",
        409 => "request conflict",
        413 => "context or request too large",
        422 => "unsupported request",
        425 | 429 => "rate limited",
        529 => "provider overloaded",
        _ if status.is_server_error() => "provider unavailable",
        _ => "provider error",
    }
}

fn frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(lf), Some(crlf)) if lf <= crlf => Some((lf, 2)),
        (Some(_), Some(crlf)) => Some((crlf, 4)),
        (Some(lf), None) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

fn parse_sse_frame(frame: &[u8]) -> Option<SseEvent> {
    let frame = String::from_utf8_lossy(frame);
    let mut event = None;
    let mut data = String::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    (!data.is_empty()).then_some(SseEvent { event, data })
}

fn excerpt(value: &str) -> String {
    let mut result: String = value.chars().take(2_000).collect();
    if value.chars().count() > 2_000 {
        result.push_str("...");
    }
    result
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use reqwest::header::HeaderValue;
    use serde_json::json;

    use super::*;

    #[test]
    fn decodes_split_lf_and_crlf_sse_frames() {
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        decoder
            .push(b"event: first\ndata: {\"a\"", &mut |event| {
                events.push(event);
                Ok(())
            })
            .unwrap();
        decoder
            .push(
                b":1}\n\ndata: line one\r\ndata: line two\r\n\r\n",
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
            )
            .unwrap();
        decoder
            .finish(&mut |event| {
                events.push(event);
                Ok(())
            })
            .unwrap();

        assert_eq!(
            events,
            [
                SseEvent {
                    event: Some("first".into()),
                    data: r#"{"a":1}"#.into(),
                },
                SseEvent {
                    event: None,
                    data: "line one\nline two".into(),
                },
            ]
        );
    }

    #[test]
    fn classifies_retryable_statuses_and_server_delays() {
        let mut headers = HeaderMap::new();
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS, &headers));
        assert!(retryable_status(StatusCode::SERVICE_UNAVAILABLE, &headers));
        assert!(!retryable_status(StatusCode::UNAUTHORIZED, &headers));

        headers.insert("x-should-retry", HeaderValue::from_static("false"));
        assert!(!retryable_status(StatusCode::SERVICE_UNAVAILABLE, &headers));
        headers.insert("x-should-retry", HeaderValue::from_static("true"));
        assert!(retryable_status(StatusCode::BAD_REQUEST, &headers));

        headers.clear();
        headers.insert("retry-after-ms", HeaderValue::from_static("1250"));
        assert_eq!(
            retry_delay(
                &headers,
                0,
                RetryPolicy {
                    max_retries: 1,
                    max_delay: Duration::from_secs(10),
                },
            ),
            Duration::from_millis(1_250)
        );
        headers.insert("retry-after-ms", HeaderValue::from_static("90000"));
        assert_eq!(
            retry_delay(
                &headers,
                0,
                RetryPolicy {
                    max_retries: 1,
                    max_delay: Duration::from_secs(2),
                },
            ),
            Duration::from_secs(2)
        );
    }

    #[tokio::test]
    async fn retries_transient_json_response_then_succeeds() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to bind retry test server: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4_096];
                let _ = stream.read(&mut request).unwrap();
                let response = if attempt == 0 {
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nRetry-After: 0\r\nConnection: close\r\n\r\nbusy"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}"
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let value = send_json(
            &Client::new(),
            Method::POST,
            &format!("http://{address}/model"),
            HeaderMap::new(),
            &json!({"prompt": "hello"}),
            RetryPolicy {
                max_retries: 1,
                max_delay: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();

        assert_eq!(value, json!({"ok": true}));
        server.join().unwrap();
    }
}
