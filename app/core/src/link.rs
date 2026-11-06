//! App links (what Go Live hands the app) and the token exchange with the Streamroom server.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct AppLink {
    /// e.g. https://stream.eventworks.click
    pub base: String,
    pub room_id: String,
    /// Signed ticket from the server identifying you (valid 12 hours).
    pub ticket: String,
}

/// Accepts:
/// - `streamroom://open?link=<url-encoded app link>` (what the site's "Open the app" launches)
/// - `https://host/live/<room>#k=<ticket>` (Copy app link)
/// - `streamroom://host/live/<room>#k=<ticket>` (older form, https assumed)
pub fn parse(input: &str) -> Result<AppLink> {
    let input = input.trim().trim_matches('"');
    if let Some(rest) = input.strip_prefix("streamroom://") {
        if let Ok(u) = url::Url::parse(input) {
            if let Some((_, inner)) = u.query_pairs().find(|(k, _)| k == "link") {
                return parse(&inner);
            }
        }
        return parse(&format!("https://{rest}"));
    }
    let fixed = if input.contains("://") { input.to_string() } else { format!("https://{input}") };
    let url = url::Url::parse(&fixed).context("that doesn't look like a link")?;
    let host = url.host_str().ok_or_else(|| anyhow!("link has no host"))?;
    let base = match url.port() {
        Some(p) => format!("{}://{}:{}", url.scheme(), host, p),
        None => format!("{}://{}", url.scheme(), host),
    };
    let mut segs = url.path_segments().ok_or_else(|| anyhow!("link has no path"))?;
    let room_id = match (segs.next(), segs.next()) {
        (Some("live"), Some(id)) if !id.is_empty() => id.to_string(),
        (Some("watch") | Some("r"), _) => bail!("that's a room link. In the room, click Go Live, then Open the app."),
        _ => bail!("that isn't a Streamroom app link. In the room, click Go Live, then Open the app."),
    };
    let ticket = url
        .fragment()
        .and_then(|f| f.split('&').find_map(|kv| kv.strip_prefix("k=")))
        .filter(|k| !k.is_empty())
        .ok_or_else(|| anyhow!("the link is incomplete (missing the part after #k=). Copy it again."))?
        .to_string();
    Ok(AppLink { base, room_id, ticket })
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

pub struct Joined {
    pub livekit_url: String,
    pub token: String,
    pub title: String,
}

pub async fn fetch_token(link: &AppLink) -> Result<Joined> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("streamroom-live/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let resp = client
        .post(format!("{}/api/token", link.base))
        .json(&serde_json::json!({
            "roomId": link.room_id,
            "role": "streamer",
            "streamKey": link.ticket,
        }))
        .send()
        .await
        .with_context(|| format!("could not reach {}", link.base))?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        let msg = serde_json::from_str::<ErrorResponse>(&body).map(|e| e.error).unwrap_or(body);
        bail!("{msg}");
    }
    let t: TokenResponse = serde_json::from_str(&body).context("unexpected reply from the server")?;
    // Same rule as the web pages: an empty `url` means "work it out from the site address".
    // https -> same domain (Caddy forwards to LiveKit); http (local dev) -> LiveKit on port 7880.
    let livekit_url = if !t.url.is_empty() {
        t.url
    } else if let Some(rest) = link.base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else {
        let host = url::Url::parse(&link.base)?.host_str().unwrap_or("localhost").to_string();
        format!("ws://{host}:7880")
    };
    Ok(Joined { livekit_url, token: t.token, title: t.title })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_app_links() {
        let l = parse("https://stream.eventworks.click/live/AbC123#k=tick.et").unwrap();
        assert_eq!(l.base, "https://stream.eventworks.click");
        assert_eq!(l.room_id, "AbC123");
        assert_eq!(l.ticket, "tick.et");

        let l = parse("http://localhost:3000/live/x1#k=k2").unwrap();
        assert_eq!(l.base, "http://localhost:3000");

        let l = parse("streamroom://stream.eventworks.click/live/r#k=z").unwrap();
        assert_eq!(l.base, "https://stream.eventworks.click");

        let deep = "streamroom://open?link=http%3A%2F%2Flocalhost%3A3000%2Flive%2FRoom1%23k%3DeyJ.sig";
        let l = parse(deep).unwrap();
        assert_eq!((l.base.as_str(), l.room_id.as_str(), l.ticket.as_str()), ("http://localhost:3000", "Room1", "eyJ.sig"));

        assert!(parse("https://stream.eventworks.click/r/room").is_err());
        assert!(parse("https://stream.eventworks.click/live/r").is_err());
    }
}
