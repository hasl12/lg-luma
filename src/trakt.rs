//! Optional Trakt "continue watching" widget. Device-code login, token storage and every
//! HTTP request stay on one worker thread; the SDL thread only exchanges messages.
use crate::{
    i18n::Language,
    model::{HomeWidget, Preferences, WidgetPosition},
    state::Page,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// Build-time credentials. Without a client ID, Luma does not offer the widget. Newer Trakt
/// apps are public PKCE clients without a secret; older apps may still send theirs.
const CLIENT_ID: Option<&str> = option_env!("LUMA_TRAKT_CLIENT_ID");
const CLIENT_SECRET: Option<&str> = option_env!("LUMA_TRAKT_CLIENT_SECRET");
const API: &str = "https://api.trakt.tv";
const DEFAULT_VERIFICATION_URL: &str = "https://auth.trakt.tv/activate";
/// Items open in Nuvio Legacy; its webOS build does not route the URL yet.
pub const NUVIO_APP: &str = "space.nuvio.native.legacy";
pub const MAX_ITEMS: usize = 3;
const LIMIT: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH: Duration = Duration::from_secs(600);
const RETRY: Duration = Duration::from_secs(120);
/// Renew tokens a day before Trakt's expiry rather than failing a Home refresh.
const RENEW_BEFORE: i64 = 24 * 3600;

pub fn configured() -> bool {
    CLIENT_ID.is_some_and(|id| !id.trim().is_empty())
}

fn client_secret() -> Option<&'static str> {
    CLIENT_SECRET
        .map(str::trim)
        .filter(|secret| !secret.is_empty())
}

/// A request body with the app's client secret only when this build has one.
fn with_secret(mut body: Value, secret: Option<&str>) -> Value {
    if let Some(secret) = secret {
        body["client_secret"] = json!(secret);
    }
    body
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub position: WidgetPosition,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            position: HomeWidget::Trakt.default_position(),
        }
    }
}
pub fn deserialize_settings<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Settings, D::Error> {
    Ok(serde_json::from_value(Value::deserialize(deserializer)?).unwrap_or_default())
}

pub fn token_path(preferences: &Path) -> PathBuf {
    preferences.with_file_name("trakt.json")
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Movie { year: Option<u16> },
    Episode { season: u16, number: u16 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub title: String,
    pub kind: Kind,
    /// Percent watched, 0–100.
    pub progress: f32,
    /// An IMDb ID (tt…) or, without one, a TMDB number.
    pub id: String,
}

impl Item {
    pub fn detail(&self, language: Language) -> String {
        match self.kind {
            Kind::Episode { season, number } => format!("S{season:02}E{number:02}"),
            Kind::Movie { year: Some(year) } => format!("{} · {year}", language.t("traktMovie")),
            Kind::Movie { year: None } => language.t("traktMovie").into(),
        }
    }

    /// The Nuvio Legacy URL that resumes this item; specials open the show page.
    pub fn deeplink(&self) -> String {
        match self.kind {
            Kind::Movie { .. } => format!("nuvio-legacy://play/movie/{}", self.id),
            Kind::Episode { season: 0, .. } => format!("nuvio-legacy://meta/series/{}", self.id),
            Kind::Episode { season, number } => {
                format!("nuvio-legacy://play/series/{}/{season}/{number}", self.id)
            }
        }
    }

    pub fn launch_params(&self) -> Value {
        json!({ "url": self.deeplink() })
    }
}

fn media_id(ids: &Value) -> Option<String> {
    if let Some(imdb) = ids["imdb"].as_str()
        && imdb.len() <= 12
        && let Some(digits) = imdb.strip_prefix("tt")
        && !digits.is_empty()
        && digits.bytes().all(|c| c.is_ascii_digit())
    {
        return Some(imdb.into());
    }
    ids["tmdb"]
        .as_u64()
        .filter(|id| (1..=u64::from(u32::MAX)).contains(id))
        .map(|id| id.to_string())
}

fn clean_title(value: &Value) -> Option<String> {
    let title: String = value
        .as_str()?
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    let title = title.trim();
    (!title.is_empty()).then(|| title.into())
}

/// Most recently paused first, at most [`MAX_ITEMS`], ignoring entries Nuvio cannot open.
pub fn parse_playback(value: &Value) -> Option<Vec<Item>> {
    let mut entries: Vec<(&str, Item)> = value
        .as_array()?
        .iter()
        .filter_map(|entry| {
            let progress = entry["progress"].as_f64()?;
            if !progress.is_finite() {
                return None;
            }
            let (title, kind, id) = match entry["type"].as_str()? {
                "movie" => {
                    let movie = &entry["movie"];
                    (
                        clean_title(&movie["title"])?,
                        Kind::Movie {
                            year: movie["year"]
                                .as_u64()
                                .and_then(|year| u16::try_from(year).ok()),
                        },
                        media_id(&movie["ids"])?,
                    )
                }
                "episode" => {
                    let episode = &entry["episode"];
                    let show = &entry["show"];
                    (
                        clean_title(&show["title"])?,
                        Kind::Episode {
                            season: u16::try_from(episode["season"].as_u64()?).ok()?,
                            number: u16::try_from(episode["number"].as_u64()?).ok()?,
                        },
                        media_id(&show["ids"])?,
                    )
                }
                _ => return None,
            };
            Some((
                entry["paused_at"].as_str().unwrap_or(""),
                Item {
                    title,
                    kind,
                    progress: progress.clamp(0., 100.) as f32,
                    id,
                },
            ))
        })
        .collect();
    // ISO 8601 UTC timestamps in one format sort chronologically as text.
    entries.sort_by(|a, b| b.0.cmp(a.0));
    Some(
        entries
            .into_iter()
            .map(|(_, item)| item)
            .take(MAX_ITEMS)
            .collect(),
    )
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
struct Token {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
}

fn parse_token(value: &Value, now: i64) -> Option<Token> {
    let token = |key: &str| {
        value[key]
            .as_str()
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= 512
                    && token.bytes().all(|c| c.is_ascii_graphic())
            })
            .map(str::to_owned)
    };
    let created = value["created_at"].as_i64().unwrap_or(now);
    let lifetime = value["expires_in"]
        .as_i64()
        .unwrap_or(0)
        .clamp(0, 365 * 86400);
    Some(Token {
        access_token: token("access_token")?,
        refresh_token: token("refresh_token")?,
        expires_at: created.saturating_add(lifetime),
    })
}

fn load_token(path: &Path) -> Option<Token> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(16 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_token(path: &Path, token: &Token) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(token).map_err(|e| e.to_string())?;
    crate::storage::write_atomic(path, &bytes)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Code {
    pub user_code: String,
    pub url: String,
    pub qr_size: i32,
    pub qr: Vec<bool>,
}

fn parse_code(value: &Value) -> Option<(String, Code, Duration, Duration)> {
    let device_code = value["device_code"]
        .as_str()
        .filter(|code| {
            !code.is_empty() && code.len() <= 256 && code.bytes().all(|c| c.is_ascii_alphanumeric())
        })?
        .to_owned();
    let user_code = value["user_code"]
        .as_str()
        .filter(|code| {
            !code.is_empty() && code.len() <= 16 && code.bytes().all(|c| c.is_ascii_alphanumeric())
        })?
        .to_owned();
    // Only Trakt's own HTTPS pages may be shown as a QR code.
    let url = value["verification_url"]
        .as_str()
        .filter(|url| {
            url.len() <= 64
                && [
                    "https://trakt.tv/",
                    "https://www.trakt.tv/",
                    "https://auth.trakt.tv/",
                ]
                .iter()
                .any(|origin| url.starts_with(origin))
                && url.bytes().all(|c| c.is_ascii_graphic())
        })
        .unwrap_or(DEFAULT_VERIFICATION_URL)
        .to_owned();
    let qr = qrcodegen::QrCode::encode_text(&url, qrcodegen::QrCodeEcc::High).ok()?;
    let size = qr.size();
    let expires = Duration::from_secs(value["expires_in"].as_u64().unwrap_or(600).clamp(60, 1800));
    let interval = Duration::from_secs(value["interval"].as_u64().unwrap_or(5).clamp(1, 60));
    Some((
        device_code,
        Code {
            user_code,
            url,
            qr_size: size,
            qr: (0..size)
                .flat_map(|y| (0..size).map(move |x| (x, y)))
                .map(|(x, y)| qr.get_module(x, y))
                .collect(),
        },
        expires,
        interval,
    ))
}

#[derive(Debug, PartialEq)]
pub enum Command {
    Login,
    CancelLogin,
    Logout,
    Playback(u64),
}

#[derive(Debug)]
enum Reply {
    Account(bool),
    Code(Code),
    LoginFailed(&'static str),
    LoggedIn,
    Playback {
        generation: u64,
        result: Result<Vec<Item>, ()>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Login {
    Idle,
    Requesting,
    Code(Code),
    Failed(&'static str),
}

pub struct Service {
    sender: Option<mpsc::SyncSender<Command>>,
    receiver: mpsc::Receiver<Reply>,
}

impl Service {
    pub fn new(path: PathBuf, enabled: bool) -> Result<Self, String> {
        let (sender, jobs) = mpsc::sync_channel::<Command>(8);
        let (results, receiver) = mpsc::sync_channel(8);
        let (Some(client_id), true) = (CLIENT_ID.filter(|_| configured()), enabled) else {
            return Ok(Self {
                sender: None,
                receiver,
            });
        };
        thread::Builder::new()
            .name("trakt".into())
            .spawn(move || {
                Worker {
                    agent: ureq::Agent::config_builder()
                        .timeout_global(Some(TIMEOUT))
                        .max_redirects(0)
                        .proxy(None)
                        .http_status_as_error(false)
                        .build()
                        .new_agent(),
                    client_id: client_id.trim(),
                    secret: client_secret(),
                    path,
                    results,
                }
                .run(jobs)
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            sender: Some(sender),
            receiver,
        })
    }
}

struct Device {
    code: String,
    interval: Duration,
    next: Instant,
    deadline: Instant,
}

struct Worker {
    agent: ureq::Agent,
    client_id: &'static str,
    secret: Option<&'static str>,
    path: PathBuf,
    results: mpsc::SyncSender<Reply>,
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

impl Worker {
    fn run(self, jobs: mpsc::Receiver<Command>) {
        let mut token = load_token(&self.path);
        if self.results.send(Reply::Account(token.is_some())).is_err() {
            return;
        }
        let mut device: Option<Device> = None;
        loop {
            let command = match &device {
                Some(device) => {
                    match jobs.recv_timeout(device.next.saturating_duration_since(Instant::now())) {
                        Ok(command) => Some(command),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                None => match jobs.recv() {
                    Ok(command) => Some(command),
                    Err(_) => return,
                },
            };
            let reply = match command {
                Some(Command::Login) => match self.request_code() {
                    Some((code, shown, expires, interval)) => {
                        let now = Instant::now();
                        device = Some(Device {
                            code,
                            interval,
                            next: now + interval,
                            deadline: now + expires,
                        });
                        Some(Reply::Code(shown))
                    }
                    None => {
                        device = None;
                        Some(Reply::LoginFailed("traktLoginError"))
                    }
                },
                Some(Command::CancelLogin) => {
                    device = None;
                    None
                }
                Some(Command::Logout) => {
                    device = None;
                    if let Some(old) = token.take() {
                        self.revoke(&old);
                    }
                    let _ = std::fs::remove_file(&self.path);
                    Some(Reply::Account(false))
                }
                Some(Command::Playback(generation)) => {
                    let result = self.playback(&mut token);
                    if token.is_none() {
                        let _ = self.results.send(Reply::Account(false));
                    }
                    Some(Reply::Playback { generation, result })
                }
                None => {
                    let Some(pending) = device.as_mut() else {
                        continue;
                    };
                    let outcome = if Instant::now() >= pending.deadline {
                        Some(Err("traktCodeExpired"))
                    } else {
                        match self.poll(&pending.code) {
                            Poll::Pending => None,
                            Poll::SlowDown => {
                                pending.interval += Duration::from_secs(5);
                                None
                            }
                            Poll::Token(value) => Some(
                                save_token(&self.path, &value)
                                    .map(|()| token = Some(value))
                                    .map_err(|error| {
                                        eprintln!("Luma Trakt token: {error}");
                                        "traktLoginError"
                                    }),
                            ),
                            Poll::Failed(key) => Some(Err(key)),
                        }
                    };
                    pending.next = Instant::now() + pending.interval;
                    // A finished poll ends the login, successfully or not.
                    outcome.map(|outcome| {
                        device = None;
                        match outcome {
                            Ok(()) => Reply::LoggedIn,
                            Err(key) => Reply::LoginFailed(key),
                        }
                    })
                }
            };
            if let Some(reply) = reply
                && self.results.send(reply).is_err()
            {
                return;
            }
        }
    }

    /// A POST when `body` is given, otherwise a GET; `None` for transport failures.
    fn request(
        &self,
        path: &str,
        body: Option<Value>,
        bearer: Option<&str>,
    ) -> Option<(u16, Value)> {
        let url = format!("{API}{path}");
        let authorization = bearer.map(|token| format!("Bearer {token}"));
        let mut response = match body {
            Some(body) => {
                let mut request = self
                    .agent
                    .post(&url)
                    .header("User-Agent", "LumaNative")
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json")
                    .header("trakt-api-version", "2")
                    .header("trakt-api-key", self.client_id);
                if let Some(authorization) = &authorization {
                    request = request.header("Authorization", authorization);
                }
                request.send(body.to_string())
            }
            None => {
                let mut request = self
                    .agent
                    .get(&url)
                    .header("User-Agent", "LumaNative")
                    .header("Accept", "application/json")
                    .header("trakt-api-version", "2")
                    .header("trakt-api-key", self.client_id);
                if let Some(authorization) = &authorization {
                    request = request.header("Authorization", authorization);
                }
                request.call()
            }
        }
        .ok()?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > LIMIT {
            return None;
        }
        Some((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }

    fn request_code(&self) -> Option<(String, Code, Duration, Duration)> {
        match self.request(
            "/oauth/device/code",
            Some(json!({ "client_id": self.client_id })),
            None,
        )? {
            (200, value) => parse_code(&value),
            _ => None,
        }
    }

    fn poll(&self, code: &str) -> Poll {
        let Some((status, value)) = self.request(
            "/oauth/device/token",
            Some(with_secret(
                json!({ "code": code, "client_id": self.client_id }),
                self.secret,
            )),
            None,
        ) else {
            return Poll::Pending;
        };
        match status {
            200 => {
                parse_token(&value, unix_now()).map_or(Poll::Failed("traktLoginError"), Poll::Token)
            }
            400 => Poll::Pending,
            429 => Poll::SlowDown,
            410 | 404 => Poll::Failed("traktCodeExpired"),
            418 => Poll::Failed("traktLoginDenied"),
            _ => Poll::Failed("traktLoginError"),
        }
    }

    /// `Err(true)` when Trakt rejects the refresh token, `Err(false)` when it is unreachable.
    fn renew(&self, token: &Token) -> Result<Token, bool> {
        match self
            .request(
                "/oauth/token",
                Some(with_secret(
                    json!({
                        "refresh_token": token.refresh_token,
                        "client_id": self.client_id,
                        "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
                        "grant_type": "refresh_token",
                    }),
                    self.secret,
                )),
                None,
            )
            .ok_or(false)?
        {
            (200, value) => {
                let renewed = parse_token(&value, unix_now()).ok_or(false)?;
                if let Err(error) = save_token(&self.path, &renewed) {
                    eprintln!("Luma Trakt token: {error}");
                }
                Ok(renewed)
            }
            (400 | 401, _) => Err(true),
            _ => Err(false),
        }
    }

    fn revoke(&self, token: &Token) {
        let _ = self.request(
            "/oauth/revoke",
            Some(with_secret(
                json!({ "token": token.access_token, "client_id": self.client_id }),
                self.secret,
            )),
            None,
        );
    }

    /// Clears `token` only when Trakt rejects both the access and the refresh token.
    fn playback(&self, token: &mut Option<Token>) -> Result<Vec<Item>, ()> {
        let current = token.clone().ok_or(())?;
        let current = if current.expires_at - unix_now() < RENEW_BEFORE {
            match self.renew(&current) {
                Ok(renewed) => {
                    *token = Some(renewed.clone());
                    renewed
                }
                Err(_) => current,
            }
        } else {
            current
        };
        let fetch = |access: &str| self.request("/sync/playback", None, Some(access));
        let (status, value) = match fetch(&current.access_token).ok_or(())? {
            (401, _) => match self.renew(&current) {
                Ok(renewed) => {
                    *token = Some(renewed.clone());
                    fetch(&renewed.access_token).ok_or(())?
                }
                // Only a rejected refresh token signs out; a network failure retries later.
                Err(true) => {
                    *token = None;
                    let _ = std::fs::remove_file(&self.path);
                    return Err(());
                }
                Err(false) => return Err(()),
            },
            reply => reply,
        };
        if status != 200 {
            return Err(());
        }
        parse_playback(&value).ok_or(())
    }
}

enum Poll {
    Pending,
    SlowDown,
    Token(Token),
    Failed(&'static str),
}

/// SDL-thread view of the Trakt account and the widget's items.
pub struct Trakt {
    pub available: bool,
    /// Unknown until the worker has read the saved token.
    pub connected: Option<bool>,
    pub login: Login,
    pub items: Vec<Item>,
    pub loaded: bool,
    pub failed: bool,
    commands: Vec<Command>,
    generation: u64,
    pending: bool,
    next: Option<Instant>,
}

impl Default for Trakt {
    fn default() -> Self {
        Self {
            available: configured(),
            connected: None,
            login: Login::Idle,
            items: vec![],
            loaded: false,
            failed: false,
            commands: vec![],
            generation: 0,
            pending: false,
            next: None,
        }
    }
}

impl Trakt {
    pub fn send(&mut self, command: Command) {
        match command {
            Command::Login => self.login = Login::Requesting,
            Command::CancelLogin => self.login = Login::Idle,
            Command::Logout => self.disconnect(),
            Command::Playback(_) => {}
        }
        self.commands.push(command);
    }

    fn disconnect(&mut self) {
        self.connected = Some(false);
        self.items.clear();
        self.loaded = false;
        self.failed = false;
        self.pending = false;
        self.next = None;
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn tick(
        &mut self,
        service: &Service,
        preferences: &Preferences,
        page: Page,
        visible: bool,
        now: Instant,
    ) -> (bool, Vec<&'static str>) {
        let mut notices = vec![];
        let mut changed = false;
        if let Some(sender) = &service.sender {
            for command in self.commands.drain(..) {
                if sender.try_send(command).is_err() {
                    self.login = match self.login {
                        Login::Requesting => Login::Failed("traktLoginError"),
                        ref login => login.clone(),
                    };
                }
            }
        } else {
            self.commands.clear();
            if self.login == Login::Requesting {
                self.login = Login::Failed("traktLoginError");
            }
        }
        while let Ok(reply) = service.receiver.try_recv() {
            changed = true;
            match reply {
                Reply::Account(connected) => {
                    if !connected {
                        self.disconnect();
                    }
                    self.connected = Some(connected);
                }
                Reply::Code(code) => {
                    if self.login == Login::Requesting {
                        self.login = Login::Code(code);
                    }
                }
                Reply::LoginFailed(key) => {
                    if self.login != Login::Idle {
                        self.login = Login::Failed(key);
                    }
                }
                Reply::LoggedIn => {
                    self.login = Login::Idle;
                    self.connected = Some(true);
                    self.next = None;
                    notices.push("traktConnected");
                }
                Reply::Playback { generation, result } => {
                    if generation != self.generation {
                        continue;
                    }
                    self.pending = false;
                    match result {
                        Ok(items) => {
                            self.items = items;
                            self.loaded = true;
                            self.failed = false;
                            self.next = Some(now + REFRESH);
                        }
                        Err(()) => {
                            self.failed = true;
                            self.next = Some(now + RETRY);
                        }
                    }
                }
            }
        }
        if self.available
            && self.connected == Some(true)
            && visible
            && page == Page::Home
            && preferences.has_widget(HomeWidget::Trakt)
            && !self.pending
            && self.next.is_none_or(|next| now >= next)
            && let Some(sender) = &service.sender
            && sender.try_send(Command::Playback(self.generation)).is_ok()
        {
            self.pending = true;
        }
        (changed, notices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (Service, mpsc::Receiver<Command>, mpsc::SyncSender<Reply>) {
        let (sender, jobs) = mpsc::sync_channel(8);
        let (results, receiver) = mpsc::sync_channel(8);
        (
            Service {
                sender: Some(sender),
                receiver,
            },
            jobs,
            results,
        )
    }

    fn movie(title: &str, imdb: Option<&str>, tmdb: Option<u64>, paused: &str) -> Value {
        json!({
            "progress": 42.5,
            "paused_at": paused,
            "type": "movie",
            "movie": {"title": title, "year": 2010, "ids": {"imdb": imdb, "tmdb": tmdb}},
        })
    }

    #[test]
    fn playback_keeps_three_newest_openable_items() {
        let value = json!([
            movie("Old", Some("tt0000001"), None, "2024-01-01T00:00:00.000Z"),
            {
                "progress": 80.,
                "paused_at": "2024-03-01T00:00:00.000Z",
                "type": "episode",
                "episode": {"season": 1, "number": 5, "title": "Pilot"},
                "show": {"title": "Show", "ids": {"imdb": null, "tmdb": 1399}},
            },
            movie("No ids", None, None, "2024-05-01T00:00:00.000Z"),
            movie("Bad imdb", Some("nm123"), None, "2024-05-02T00:00:00.000Z"),
            movie("Newest", Some("tt1375666"), Some(27205), "2024-04-01T00:00:00.000Z"),
            movie("Middle", None, Some(603), "2024-02-01T00:00:00.000Z"),
        ]);
        let items = parse_playback(&value).unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            ["Newest", "Show", "Middle"]
        );
        assert_eq!(items[0].id, "tt1375666");
        assert_eq!(items[2].id, "603");
        assert_eq!(
            items[1].kind,
            Kind::Episode {
                season: 1,
                number: 5
            }
        );
        assert!(parse_playback(&json!({"error": true})).is_none());
    }

    #[test]
    fn deeplinks_and_details_follow_nuvio_routes() {
        let mut item = Item {
            title: "Show".into(),
            kind: Kind::Episode {
                season: 2,
                number: 7,
            },
            progress: 10.,
            id: "tt0944947".into(),
        };
        assert_eq!(item.deeplink(), "nuvio-legacy://play/series/tt0944947/2/7");
        assert_eq!(item.detail(Language::En), "S02E07");
        assert_eq!(
            item.launch_params(),
            json!({"url": "nuvio-legacy://play/series/tt0944947/2/7"})
        );
        item.kind = Kind::Episode {
            season: 0,
            number: 1,
        };
        assert_eq!(item.deeplink(), "nuvio-legacy://meta/series/tt0944947");
        item.kind = Kind::Movie { year: Some(1999) };
        item.id = "603".into();
        assert_eq!(item.deeplink(), "nuvio-legacy://play/movie/603");
        assert_eq!(
            item.detail(Language::En),
            format!("{} · 1999", Language::En.t("traktMovie"))
        );
    }

    #[test]
    fn titles_progress_and_codes_are_bounded() {
        let mut value = movie("A\u{7}B", Some("tt1"), None, "");
        value["progress"] = json!(250.);
        let items = parse_playback(&json!([value])).unwrap();
        assert_eq!(items[0].title, "AB");
        assert_eq!(items[0].progress, 100.);
        let code = json!({
            "device_code": "abc123",
            "user_code": "5055CC52",
            "verification_url": "https://evil.example/activate",
            "expires_in": 600,
            "interval": 5,
        });
        let (device, shown, _, interval) = parse_code(&code).unwrap();
        assert_eq!(device, "abc123");
        assert_eq!(shown.url, DEFAULT_VERIFICATION_URL);
        let mut auth = code.clone();
        auth["verification_url"] = json!("https://auth.trakt.tv/activate");
        assert_eq!(
            parse_code(&auth).unwrap().1.url,
            "https://auth.trakt.tv/activate"
        );
        let mut lookalike = code.clone();
        lookalike["verification_url"] = json!("https://auth.trakt.tv.evil.example/activate");
        assert_eq!(
            parse_code(&lookalike).unwrap().1.url,
            DEFAULT_VERIFICATION_URL
        );
        assert_eq!(interval, Duration::from_secs(5));
        assert_eq!(shown.qr.len(), (shown.qr_size * shown.qr_size) as usize);
        let mut bad = code.clone();
        bad["user_code"] = json!("<script>");
        assert!(parse_code(&bad).is_none());
    }

    #[test]
    fn public_clients_send_no_client_secret() {
        let body = json!({"code": "c", "client_id": "id"});
        assert_eq!(with_secret(body.clone(), None), body);
        assert_eq!(
            with_secret(body, Some("s")),
            json!({"code": "c", "client_id": "id", "client_secret": "s"})
        );
    }

    #[test]
    fn tokens_round_trip_privately() {
        let directory = std::env::temp_dir().join(format!("luma-trakt-{}", std::process::id()));
        let path = token_path(&directory.join("preferences.json"));
        let token = parse_token(
            &json!({"access_token": "a", "refresh_token": "r", "created_at": 100, "expires_in": 50}),
            0,
        )
        .unwrap();
        assert_eq!(token.expires_at, 150);
        save_token(&path, &token).unwrap();
        assert_eq!(load_token(&path), Some(token));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(parse_token(&json!({"access_token": "a b", "refresh_token": "r"}), 0).is_none());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn playback_is_requested_only_for_a_visible_connected_widget_and_stale_replies_are_ignored() {
        let (service, jobs, results) = service();
        let mut preferences = Preferences::default();
        let mut trakt = Trakt {
            available: true,
            ..Trakt::default()
        };
        let now = Instant::now();
        trakt.tick(&service, &preferences, Page::Home, true, now);
        assert!(
            jobs.try_recv().is_err(),
            "No request before the token is known"
        );
        results.send(Reply::Account(true)).unwrap();
        trakt.tick(&service, &preferences, Page::Home, true, now);
        assert!(jobs.try_recv().is_err(), "No request without the widget");
        preferences.trakt.enabled = true;
        trakt.tick(&service, &preferences, Page::Settings, true, now);
        trakt.tick(&service, &preferences, Page::Home, false, now);
        assert!(jobs.try_recv().is_err());
        trakt.tick(&service, &preferences, Page::Home, true, now);
        assert_eq!(jobs.try_recv().unwrap(), Command::Playback(0));
        trakt.tick(&service, &preferences, Page::Home, true, now);
        assert!(jobs.try_recv().is_err(), "One request at a time");
        let item = Item {
            title: "Movie".into(),
            kind: Kind::Movie { year: None },
            progress: 50.,
            id: "tt1".into(),
        };
        results
            .send(Reply::Playback {
                generation: 0,
                result: Ok(vec![item.clone()]),
            })
            .unwrap();
        trakt.tick(&service, &preferences, Page::Home, true, now);
        assert_eq!(trakt.items, std::slice::from_ref(&item));
        assert!(jobs.try_recv().is_err(), "Refreshes are throttled");
        trakt.tick(&service, &preferences, Page::Home, true, now + REFRESH);
        assert_eq!(jobs.try_recv().unwrap(), Command::Playback(0));
        trakt.send(Command::Logout);
        results
            .send(Reply::Playback {
                generation: 0,
                result: Ok(vec![item]),
            })
            .unwrap();
        trakt.tick(&service, &preferences, Page::Home, true, now + REFRESH);
        assert_eq!(jobs.try_recv().unwrap(), Command::Logout);
        assert!(
            trakt.items.is_empty(),
            "A reply for the old account is dropped"
        );
        assert_eq!(trakt.connected, Some(false));
        assert!(jobs.try_recv().is_err());
    }

    #[test]
    fn login_shows_the_code_and_ignores_late_codes_after_cancel() {
        let (service, jobs, results) = service();
        let preferences = Preferences::default();
        let mut trakt = Trakt {
            available: true,
            ..Trakt::default()
        };
        let now = Instant::now();
        trakt.send(Command::Login);
        assert_eq!(trakt.login, Login::Requesting);
        trakt.tick(&service, &preferences, Page::Settings, true, now);
        assert_eq!(jobs.try_recv().unwrap(), Command::Login);
        let (_, code, _, _) =
            parse_code(&json!({"device_code": "d", "user_code": "ABCD1234"})).unwrap();
        results.send(Reply::Code(code.clone())).unwrap();
        trakt.tick(&service, &preferences, Page::Settings, true, now);
        assert_eq!(trakt.login, Login::Code(code.clone()));
        trakt.send(Command::CancelLogin);
        results.send(Reply::Code(code)).unwrap();
        trakt.tick(&service, &preferences, Page::Settings, true, now);
        assert_eq!(trakt.login, Login::Idle);
        results.send(Reply::LoggedIn).unwrap();
        assert_eq!(
            trakt.tick(&service, &preferences, Page::Settings, true, now),
            (true, vec!["traktConnected"])
        );
        assert_eq!(trakt.connected, Some(true));
    }
}
