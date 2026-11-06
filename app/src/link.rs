//! Streamer links and tokens from the Streamroom server.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct StreamerLink {
    /// e.g. https://stream.eventworks.click
    pub base: String,
    pub room_id: String,
    pub stream_key: String,
}

/// Accepts `https://host/live/<room>#k=<key>` (the streamer link from the web app),
/// or the same with a `streamroom://` scheme, which maps to https.
pub fn parse(input: &str) -> Result<StreamerLink> {
    let input = input.trim().trim_matches('"');
    let fixed = if let Some(rest) = input.strip_prefix("streamroom://") {
        format!("https://{rest}")
    } else if !input.contains("://") {
        format!("https://{input}")
    } else {
        input.to_string()
    };
    let url = url::Url::parse(&fixed).context("that doesn't look like a link")?;
    let host = url.host_str().ok_or_else(|| anyhow!("link has no host"))?;
    let base = match url.port() {
        Some(p) => format!("{}://{}:{}", url.scheme(), host, p),
        None => format!("{}://{}", url.scheme(), host),
    };
    let mut segs = url.path_segments().ok_or_else(|| anyhow!("link has no path"))?;
    let room_id = match (segs.next(), segs.next()) {
        (Some("live"), Some(id)) if !id.is_empty() => id.to_string(),
        (Some("watch") | Some("r"), _) => bail!("that's a room link. In the room, click Go Live, then Copy app link."),
        _ => bail!("that isn't a Streamroom app link. In the room, click Go Live, then Copy app link."),
    };
    let stream_key = url
        .fragment()
        .and_then(|f| f.split('&').find_map(|kv| kv.strip_prefix("k=")))
        .filter(|k| !k.is_empty())
        .ok_or_else(|| anyhow!("the link is incomplete (missing the part after #k=). Copy it again with Copy app link."))?
        .to_string();
    Ok(StreamerLink { base, room_id, stream_key })
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
}

pub struct Session {
    pub livekit_url: String,
    pub token: String,
    pub title: String,
}

pub async fn fetch_token(link: &StreamerLink, name: &str) -> Result<Session> {
    let client = reqwest::Client::builder().user_agent(concat!("streamroom-live/", env!("CARGO_PKG_VERSION"))).build()?;
    let resp = client
        .post(format!("{}/api/token", link.base))
        .json(&serde_json::json!({
            "roomId": link.room_id,
            "role": "streamer",
            "streamKey": link.stream_key,
            "name": name,
        }))
        .send()
        .await
        .with_context(|| format!("could not reach {}", link.base))?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        let msg = serde_json::from_str::<ErrorResponse>(&body).map(|e| e.error).unwrap_or(body);
        bail!("server said: {msg}");
    }
    let t: TokenResponse = serde_json::from_str(&body).context("unexpected reply from server")?;
    // Same rule as the web pages: an empty `url` means "work it out from the page address".
    // https -> same domain (Caddy forwards to LiveKit); http (local dev) -> LiveKit on port 7880.
    let livekit_url = if !t.url.is_empty() {
        t.url
    } else if let Some(rest) = link.base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        let host = url::Url::parse(&link.base)?.host_str().unwrap_or("localhost").to_string();
        format!("ws://{host}:7880")
    };
    Ok(Session { livekit_url, token: t.token, title: t.title })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_streamer_links() {
        let l = parse("https://stream.eventworks.click/live/AbC123#k=secretKey").unwrap();
        assert_eq!(l.base, "https://stream.eventworks.click");
        assert_eq!(l.room_id, "AbC123");
        assert_eq!(l.stream_key, "secretKey");

        let l = parse("http://localhost:3000/live/x1#k=k2").unwrap();
        assert_eq!(l.base, "http://localhost:3000");

        let l = parse("streamroom://stream.eventworks.click/live/r#k=z").unwrap();
        assert_eq!(l.base, "https://stream.eventworks.click");

        assert!(parse("https://stream.eventworks.click/watch/r").is_err());
        assert!(parse("https://stream.eventworks.click/live/r").is_err());
    }
}
