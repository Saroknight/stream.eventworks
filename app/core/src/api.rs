//! The app's own sign-in and the Streamroom server API it uses.
//!
//! "Open Streamroom Live" on the site launches `streamroom://open?server=…&room=…&pair=…`.
//! The pair code is single-use and short-lived; the app trades it once for a device key
//! (a revocable server session) and from then on calls the API with that key: list rooms,
//! start or end a room, and get a pass to stream into one.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::link::{self, AppLink, Joined, TokenResponse};

/// What a pasted or launched link asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Launch {
    /// New links: which server, optionally which room, optionally a sign-in code.
    Open { server: String, room: Option<String>, pair: Option<String> },
    /// Older app links with a 12-hour ticket.
    Legacy(AppLink),
}

impl Launch {
    pub fn server(&self) -> &str {
        match self {
            Launch::Open { server, .. } => server,
            Launch::Legacy(l) => &l.base,
        }
    }
}

/// Accepts `streamroom://open?server=…&room=…&pair=…`, plus everything [`link::parse`] accepts.
pub fn parse_launch(input: &str) -> Result<Launch> {
    let input = input.trim().trim_matches('"');
    // Some browsers or Windows add a slash: streamroom://open/?…
    let normalized = input.replacen("streamroom://open/?", "streamroom://open?", 1);
    let input = normalized.as_str();
    if input.starts_with("streamroom://open?") {
        let u = url::Url::parse(input).context("that link is damaged. Copy it again.")?;
        let get = |k: &str| u.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned()).filter(|v| !v.is_empty());
        if let Some(server) = get("server") {
            let s = url::Url::parse(&server).map_err(|_| anyhow!("that link names a server I can't read"))?;
            if !matches!(s.scheme(), "https" | "http") || s.host_str().is_none() {
                bail!("that link names a server I can't read");
            }
            let server = server.trim_end_matches('/').to_string();
            let room = get("room").filter(|r| r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
            return Ok(Launch::Open { server, room, pair: get("pair") });
        }
    }
    link::parse(input).map(Launch::Legacy)
}

/// Saved after pairing. The desktop app stores it encrypted for the Windows user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub server: String,
    pub user_id: String,
    pub user_name: String,
    #[serde(default)]
    pub is_admin: bool,
    pub device_key: String,
}

/// The server no longer accepts this PC's key (signed out from the admin page, or removed).
#[derive(Debug)]
pub struct SignedOut;
impl std::fmt::Display for SignedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This PC was signed out. In a room, click Go Live, then Open Streamroom Live to sign in again.")
    }
}
impl std::error::Error for SignedOut {}

pub struct Api {
    pub server: String,
    key: Option<String>,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct PairReply {
    #[serde(rename = "deviceKey")]
    device_key: String,
    user: UserReply,
}
#[derive(Deserialize)]
struct UserReply {
    id: String,
    name: String,
    #[serde(default, rename = "isAdmin")]
    is_admin: bool,
}

impl Api {
    pub fn new(server: &str, key: Option<String>) -> Result<Self> {
        Ok(Api {
            server: server.trim_end_matches('/').to_string(),
            key,
            http: reqwest::Client::builder()
                .user_agent(concat!("streamroom-live/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(20))
                .build()?,
        })
    }

    pub fn for_account(a: &Account) -> Result<Self> {
        Self::new(&a.server, Some(a.device_key.clone()))
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value> {
        let mut req = self.http.request(method.clone(), format!("{}/api{}", self.server, path));
        if let Some(k) = &self.key {
            req = req.bearer_auth(k);
        }
        if method != reqwest::Method::GET {
            req = req.json(&body.unwrap_or_else(|| json!({})));
        }
        let resp = req.send().await.with_context(|| format!("couldn't reach {}", self.server))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED && self.key.is_some() {
            return Err(SignedOut.into());
        }
        if !status.is_success() {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or_else(|| format!("the server answered {status}"));
            bail!("{msg}");
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).context("unexpected reply from the server")
    }

    /// Trade a pair code for this PC's key. Pass the current key so the server keeps it when
    /// it's the same person (no duplicate devices).
    pub async fn pair(&self, code: &str, current: Option<&Account>, device_name: &str) -> Result<Account> {
        let existing = current.filter(|a| a.server == self.server).map(|a| a.device_key.clone());
        let v = self
            .call(reqwest::Method::POST, "/app/pair", Some(json!({ "code": code, "deviceKey": existing, "deviceName": device_name })))
            .await?;
        let r: PairReply = serde_json::from_value(v).context("unexpected reply from the server")?;
        Ok(Account { server: self.server.clone(), user_id: r.user.id, user_name: r.user.name, is_admin: r.user.is_admin, device_key: r.device_key })
    }

    /// Checks the key still works; returns the (possibly renamed) user.
    pub async fn me(&self) -> Result<(String, String, bool)> {
        let v = self.call(reqwest::Method::GET, "/app/me", None).await?;
        let u: UserReply = serde_json::from_value(v["user"].clone()).context("unexpected reply from the server")?;
        Ok((u.id, u.name, u.is_admin))
    }

    /// The lobby: `[{id, title, createdAt, creator{name}, isCreator, canEnd, live[{name,kind}], here}]`.
    pub async fn rooms(&self) -> Result<Value> {
        Ok(self.call(reqwest::Method::GET, "/rooms", None).await?["rooms"].clone())
    }

    /// Returns the new room's id.
    pub async fn create_room(&self, title: &str) -> Result<String> {
        let v = self.call(reqwest::Method::POST, "/rooms", Some(json!({ "title": title }))).await?;
        v["id"].as_str().map(str::to_string).ok_or_else(|| anyhow!("unexpected reply from the server"))
    }

    pub async fn end_room(&self, id: &str) -> Result<()> {
        self.call(reqwest::Method::POST, &format!("/rooms/{id}/end"), None).await.map(|_| ())
    }

    /// A pass to stream into a room (publish only).
    pub async fn app_token(&self, id: &str) -> Result<Joined> {
        let v = self.call(reqwest::Method::POST, &format!("/rooms/{id}/app-token"), None).await?;
        let t: TokenResponse = serde_json::from_value(v).context("unexpected reply from the server")?;
        Ok(Joined { livekit_url: link::livekit_url_for(&self.server, &t.url)?, token: t.token, title: t.title })
    }

    pub async fn signout(&self) -> Result<()> {
        self.call(reqwest::Method::POST, "/app/signout", None).await.map(|_| ())
    }

    /// Tell the admin's Health panel something went wrong. Best effort.
    pub async fn report(&self, kind: &str, message: &str, room: Option<&str>) {
        let body = json!({ "kind": kind, "message": message, "room": room, "version": env!("CARGO_PKG_VERSION") });
        let _ = self.call(reqwest::Method::POST, "/health/report", Some(body)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_launch_links() {
        let l = parse_launch("streamroom://open?server=https%3A%2F%2Fstream.eventworks.click&room=AbC-12&pair=x.y").unwrap();
        assert_eq!(
            l,
            Launch::Open { server: "https://stream.eventworks.click".into(), room: Some("AbC-12".into()), pair: Some("x.y".into()) }
        );
        let l = parse_launch("streamroom://open/?server=http://localhost:3000/").unwrap();
        assert_eq!(l, Launch::Open { server: "http://localhost:3000".into(), room: None, pair: None });
        // Room ids are plain: anything else is dropped rather than put into a URL path.
        let l = parse_launch("streamroom://open?server=https://a.b&room=../admin").unwrap();
        assert_eq!(l, Launch::Open { server: "https://a.b".into(), room: None, pair: None });
        assert!(parse_launch("streamroom://open?server=javascript:alert(1)").is_err());
        // Old links still work.
        assert!(matches!(parse_launch("https://stream.eventworks.click/live/AbC#k=t").unwrap(), Launch::Legacy(_)));
        assert!(matches!(
            parse_launch("streamroom://open?link=http%3A%2F%2Flocalhost%3A3000%2Flive%2FR%23k%3Dk").unwrap(),
            Launch::Legacy(_)
        ));
    }
}
