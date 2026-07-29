use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Method, header::HeaderMap};
use serde::Serialize;
use serde_json::Value;

const MAX_SSE_FRAME_BYTES: usize = 16 * 1024 * 1024;

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

pub async fn send_sse<T, F>(
    client: &Client,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: &T,
    mut on_event: F,
) -> Result<()>
where
    T: Serialize + ?Sized,
    F: FnMut(SseEvent) -> Result<()>,
{
    let mut delay = Duration::from_millis(500);
    for attempt in 0..4 {
        let response = client
            .request(method.clone(), url)
            .headers(headers.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(body)
            .send()
            .await
            .with_context(|| format!("request to {url} failed"))?;
        let status = response.status();
        if status.is_success() {
            let mut decoder = SseDecoder::default();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("failed to read provider event stream")?;
                decoder.push(&chunk, &mut on_event)?;
            }
            decoder.finish(&mut on_event)?;
            return Ok(());
        }
        let text = response
            .text()
            .await
            .context("failed to read model response")?;
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

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
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
            on_event(event)?;
        }
        self.buffer.clear();
        Ok(())
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
}
