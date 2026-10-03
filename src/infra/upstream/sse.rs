//! SSE framing and the silence heartbeat.

use serde_json::Value;

pub fn sse(v: &Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        v.get("type").and_then(|t| t.as_str()).unwrap_or("message"),
        v
    )
}

/// Anthropic `error` SSE frame. The Messages API allows an `error` event
/// mid-stream (after `message_start`), so a translator can report an upstream
/// failure instead of ending the stream silently.
pub fn sse_error(err_type: &str, message: &str) -> String {
    sse(&serde_json::json!({
        "type": "error",
        "error": {"type": err_type, "message": message}
    }))
}

/// Wrap a byte stream, injecting `event: ping` SSE frames whenever the
/// upstream stays silent longer than `idle`. Keeps the client's stream
/// watchdog fed during long reasoning pauses.
pub fn with_heartbeat<S>(
    stream: S,
    idle: std::time::Duration,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>>
where
    S: futures::Stream<Item = Result<Vec<u8>, std::io::Error>>,
{
    async_stream::stream! {
        let mut inner = Box::pin(stream);
        loop {
            match tokio::time::timeout(idle, futures::StreamExt::next(&mut inner)).await {
                Ok(Some(item)) => yield item,
                Ok(None) => break,
                Err(_) => {
                    yield Ok::<_, std::io::Error>(
                        "event: ping\ndata: {\"type\": \"ping\"}\n\n".as_bytes().to_vec(),
                    );
                }
            }
        }
    }
}

/// Per-part token estimate for the optional count_tokens endpoint (no
/// tokenizer: Anthropic itself documents its counts as an estimate). Text is
/// chars/4 (≈1 token per 4 ASCII chars); each message and tool adds a small
/// fixed overhead; images and base64 documents count their real size, so a
/// large attachment no longer inflates the count as if it were prose.

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn heartbeat_fires_during_silence() {
        use futures::StreamExt;
        use std::time::Duration;
        let slow = async_stream::stream! {
            tokio::time::sleep(Duration::from_millis(180)).await;
            yield Ok::<_, std::io::Error>(b"data".to_vec());
        };
        let mut hb = Box::pin(with_heartbeat(slow, Duration::from_millis(50)));
        // ~50/100/150ms: pings; then the payload.
        for _ in 0..3 {
            let item = hb.next().await.unwrap().unwrap();
            assert!(String::from_utf8_lossy(&item).contains("ping"));
        }
        let item = hb.next().await.unwrap().unwrap();
        assert_eq!(item, b"data".to_vec());
        assert!(hb.next().await.is_none());
    }
}