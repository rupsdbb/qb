//! Thin qBittorrent WebUI API client.
//!
//! Keeps one cookie-backed session, re-logs in on 403, and maintains an
//! incremental `sync/maindata` mirror so each poll only transfers deltas.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use reqwest::{Client, RequestBuilder, Response, StatusCode, multipart};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Http(reqwest::Error),
    Login(String),
    Api(StatusCode, String),
    Rejected(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Http(e) if e.is_timeout() => write!(f, "qBittorrent timed out"),
            Error::Http(e) if e.is_connect() => write!(f, "qBittorrent unreachable"),
            Error::Http(e) => write!(f, "qBittorrent request failed: {e}"),
            Error::Login(m) => write!(f, "qBittorrent login failed: {m}"),
            Error::Api(s, m) if m.is_empty() => write!(f, "qBittorrent returned {s}"),
            Error::Api(s, m) => write!(f, "qBittorrent returned {s}: {m}"),
            Error::Rejected(m) => write!(f, "{m}"),
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e)
    }
}

/// Mirror of qBittorrent's sync state, advanced by `rid`.
#[derive(Default)]
struct SyncState {
    rid: i64,
    torrents: Map<String, Value>,
    categories: Map<String, Value>,
    server: Map<String, Value>,
}

pub struct Qbit {
    http: Client,
    base: String,
    user: String,
    pass: String,
    sync: Mutex<SyncState>,
    /// Set once a 404 shows the server predates the v5 stop/start endpoints.
    legacy_pause: AtomicBool,
    /// One login at a time, and remembers a rejected one: qBittorrent bans
    /// the IP after 5 bad logins, which 2-second polling would hit in 10 s.
    login_state: Mutex<Option<(Instant, String)>>,
}

const LOGIN_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Fields forwarded to the browser; everything else (magnet_uri, tracker
/// lists, ...) stays server side to keep the 2-second poll small.
const TORRENT_FIELDS: &[&str] = &[
    "name",
    "state",
    "progress",
    "dlspeed",
    "upspeed",
    "eta",
    "size",
    "completed",
    "amount_left",
    "num_seeds",
    "num_leechs",
    "num_complete",
    "num_incomplete",
    "ratio",
    "uploaded",
    "added_on",
    "completion_on",
    "category",
    "save_path",
    "force_start",
    "seq_dl",
];

const SERVER_FIELDS: &[&str] = &[
    "dl_info_speed",
    "up_info_speed",
    "free_space_on_disk",
    "connection_status",
    "use_alt_speed_limits",
    "dht_nodes",
];

impl Qbit {
    pub fn new(base: String, user: String, pass: String) -> Self {
        let http = Client::builder()
            .cookie_store(true)
            // a hung host must not stall every poll indefinitely
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(4))
            .build()
            .expect("reqwest client");

        Qbit {
            http,
            base: base.trim_end_matches('/').to_owned(),
            user,
            pass,
            sync: Mutex::default(),
            legacy_pause: AtomicBool::new(false),
            login_state: Mutex::new(None),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v2/{}", self.base, path)
    }

    async fn login(&self) -> Result<()> {
        let mut failed = self.login_state.lock().await;
        if let Some((at, msg)) = failed.as_ref() {
            if at.elapsed() < LOGIN_RETRY_AFTER {
                return Err(Error::Login(msg.clone()));
            }
        }

        let result = self.try_login().await;
        // only a definite rejection backs off; a network blip retries next poll
        *failed = match &result {
            Err(Error::Login(msg)) => Some((Instant::now(), msg.clone())),
            _ => None,
        };
        result
    }

    async fn try_login(&self) -> Result<()> {
        let r = self
            .http
            .post(self.url("auth/login"))
            .form(&[("username", &self.user), ("password", &self.pass)])
            .send()
            .await?;

        let status = r.status();
        let body = r.text().await.unwrap_or_default();

        if status == StatusCode::FORBIDDEN {
            return Err(Error::Login(
                "IP banned after too many failed attempts".into(),
            ));
        }
        if !status.is_success() || body.trim() != "Ok." {
            return Err(Error::Login(if body.trim() == "Fails." {
                "wrong username or password".into()
            } else {
                format!("{status} {}", body.trim())
            }));
        }
        Ok(())
    }

    /// Sends a request, logging in once and retrying if the session expired.
    /// `build` is called again for the retry because multipart bodies are
    /// single-use.
    async fn send(&self, build: impl Fn(&Client) -> RequestBuilder) -> Result<Response> {
        let mut r = build(&self.http).send().await?;

        // 403 on every version; some reverse proxies turn it into 401
        if matches!(r.status(), StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) {
            self.login().await?;
            r = build(&self.http).send().await?;
        }

        let status = r.status();
        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            return Err(Error::Api(status, body.trim().chars().take(200).collect()));
        }
        Ok(r)
    }

    async fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<Response> {
        let url = self.url(path);
        self.send(|c| c.post(&url).form(form)).await
    }

    /// Advances the sync mirror and returns a browser-sized snapshot.
    pub async fn snapshot(&self) -> Result<Value> {
        let mut st = self.sync.lock().await;

        let url = self.url("sync/maindata");
        let rid = st.rid.to_string();
        let mut data: Value = self
            .send(|c| c.get(&url).query(&[("rid", &rid)]))
            .await?
            .json()
            .await?;

        apply_sync(&mut st, &mut data);

        let torrents: Vec<Value> = st
            .torrents
            .iter()
            .map(|(hash, t)| {
                let mut out = Map::with_capacity(TORRENT_FIELDS.len() + 1);
                out.insert("hash".into(), Value::String(hash.clone()));
                for &k in TORRENT_FIELDS {
                    if let Some(v) = t.get(k) {
                        out.insert(k.into(), v.clone());
                    }
                }
                Value::Object(out)
            })
            .collect();

        let server: Map<String, Value> = SERVER_FIELDS
            .iter()
            .filter_map(|&k| st.server.get(k).map(|v| (k.to_owned(), v.clone())))
            .collect();

        let mut categories: Vec<&String> = st.categories.keys().collect();
        categories.sort_by_key(|c| c.to_lowercase());

        Ok(serde_json::json!({
            "server": server,
            "categories": categories,
            "torrents": torrents,
        }))
    }

    pub async fn magnet(&self, hash: &str) -> Option<String> {
        let st = self.sync.lock().await;
        st.torrents
            .get(hash)?
            .get("magnet_uri")?
            .as_str()
            .map(str::to_owned)
    }

    pub async fn add(&self, add: AddRequest) -> Result<()> {
        let url = self.url("torrents/add");
        let paused = if add.paused { "true" } else { "false" };

        let r = self
            .send(|c| {
                let mut form = multipart::Form::new()
                    // qBittorrent >= 5 reads `stopped`, older versions `paused`
                    .text("paused", paused)
                    .text("stopped", paused);

                if !add.urls.is_empty() {
                    form = form.text("urls", add.urls.join("\n"));
                }
                if !add.category.is_empty() {
                    form = form.text("category", add.category.clone());
                }
                for (name, bytes) in &add.files {
                    let part = multipart::Part::bytes(bytes.clone())
                        .file_name(name.clone())
                        .mime_str("application/x-bittorrent")
                        .expect("static mime");
                    form = form.part("torrents", part);
                }
                c.post(&url).multipart(form)
            })
            .await?;

        // qBittorrent 4.x answers 200 "Fails." when nothing could be added;
        // 5.1+ answers JSON counts instead
        let body = r.text().await.unwrap_or_default();
        let body = body.trim();
        let rejected = body == "Fails."
            || serde_json::from_str::<Value>(body).is_ok_and(|v| {
                let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
                n("failure_count") > 0 && n("success_count") == 0 && n("pending_count") == 0
            });
        if rejected {
            return Err(Error::Rejected(
                "qBittorrent rejected it (invalid or already added)",
            ));
        }
        Ok(())
    }

    pub async fn action(&self, action: Action, hashes: &str) -> Result<()> {
        let h = [("hashes", hashes)];
        match action {
            // qBittorrent 5 renamed pause/resume to stop/start
            Action::Pause => {
                self.with_fallback("torrents/stop", "torrents/pause", &h)
                    .await
            }
            Action::Resume => {
                self.with_fallback("torrents/start", "torrents/resume", &h)
                    .await
            }
            Action::Recheck => self.post_form("torrents/recheck", &h).await.map(drop),
            Action::Reannounce => self.post_form("torrents/reannounce", &h).await.map(drop),
            Action::ForceStart => self
                .post_form(
                    "torrents/setForceStart",
                    &[("hashes", hashes), ("value", "true")],
                )
                .await
                .map(drop),
            Action::Delete | Action::DeleteFiles => {
                let files = if action == Action::DeleteFiles {
                    "true"
                } else {
                    "false"
                };
                self.post_form(
                    "torrents/delete",
                    &[("hashes", hashes), ("deleteFiles", files)],
                )
                .await
                .map(drop)
            }
        }
    }

    pub async fn toggle_alt_speed(&self) -> Result<()> {
        self.post_form("transfer/toggleSpeedLimitsMode", &[])
            .await
            .map(drop)
    }

    async fn with_fallback(&self, path: &str, legacy: &str, form: &[(&str, &str)]) -> Result<()> {
        if self.legacy_pause.load(Ordering::Relaxed) {
            return self.post_form(legacy, form).await.map(drop);
        }
        match self.post_form(path, form).await {
            Err(Error::Api(StatusCode::NOT_FOUND, _)) => {
                self.legacy_pause.store(true, Ordering::Relaxed);
                self.post_form(legacy, form).await.map(drop)
            }
            r => r.map(drop),
        }
    }
}

fn apply_sync(st: &mut SyncState, data: &mut Value) {
    let full = data
        .get("full_update")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if full {
        st.torrents.clear();
        st.categories.clear();
        st.server.clear();
    }

    st.rid = data.get("rid").and_then(Value::as_i64).unwrap_or(0);

    merge_nested(&mut st.torrents, data.get_mut("torrents"));
    merge_nested(&mut st.categories, data.get_mut("categories"));
    remove_keys(&mut st.torrents, data.get("torrents_removed"));
    remove_keys(&mut st.categories, data.get("categories_removed"));

    if let Some(Value::Object(server)) = data.get_mut("server_state") {
        st.server.extend(std::mem::take(server));
    }
}

/// Deltas carry only changed fields, so merge per key rather than replace.
fn merge_nested(into: &mut Map<String, Value>, delta: Option<&mut Value>) {
    let Some(Value::Object(delta)) = delta else {
        return;
    };
    for (key, value) in std::mem::take(delta) {
        match (into.get_mut(&key), value) {
            (Some(Value::Object(existing)), Value::Object(changes)) => existing.extend(changes),
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

fn remove_keys(from: &mut Map<String, Value>, keys: Option<&Value>) {
    for k in keys.and_then(Value::as_array).into_iter().flatten() {
        if let Some(k) = k.as_str() {
            from.remove(k);
        }
    }
}

pub struct AddRequest {
    pub urls: Vec<String>,
    pub files: Vec<(String, Vec<u8>)>,
    pub category: String,
    pub paused: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Pause,
    Resume,
    Recheck,
    Reannounce,
    ForceStart,
    Delete,
    DeleteFiles,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sync_merges_partial_updates_and_removals() {
        let mut st = SyncState::default();

        apply_sync(
            &mut st,
            &mut json!({
                "rid": 1, "full_update": true,
                "torrents": {"aa": {"name": "a", "progress": 0.1}, "bb": {"name": "b"}},
                "categories": {"tv": {"name": "tv"}},
                "server_state": {"dl_info_speed": 5, "free_space_on_disk": 9}
            }),
        );
        apply_sync(
            &mut st,
            &mut json!({
                "rid": 2,
                "torrents": {"aa": {"progress": 0.5}, "cc": {"name": "c"}},
                "torrents_removed": ["bb"],
                "server_state": {"dl_info_speed": 7}
            }),
        );

        assert_eq!(st.rid, 2);
        assert_eq!(st.torrents["aa"], json!({"name": "a", "progress": 0.5}));
        assert!(!st.torrents.contains_key("bb"));
        assert!(st.torrents.contains_key("cc"));
        assert_eq!(st.server["dl_info_speed"], 7);
        assert_eq!(st.server["free_space_on_disk"], 9);
        assert!(st.categories.contains_key("tv"));
    }

    #[test]
    fn full_update_drops_stale_entries() {
        let mut st = SyncState::default();
        apply_sync(
            &mut st,
            &mut json!({"rid": 5, "torrents": {"old": {"name": "x"}}}),
        );
        apply_sync(
            &mut st,
            &mut json!({"rid": 1, "full_update": true, "torrents": {"new": {"name": "y"}}}),
        );
        assert_eq!(st.torrents.keys().collect::<Vec<_>>(), ["new"]);
    }
}
