//! Reminders: decide what is due (pure), send it through FCM HTTP v1, and run the scheduler.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};

use crate::model::{Completion, Item};
use crate::store::{self, now_ms};
use crate::AppState;

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub token: String,
    pub title: String,
    pub body: String,
    pub data: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    /// The device token is dead and should be forgotten.
    Unregistered,
}

#[async_trait]
pub trait PushSender: Send + Sync {
    fn enabled(&self) -> bool;
    async fn send(&self, msg: &Message) -> anyhow::Result<SendOutcome>;
}

/// Used when no service account is configured; reminders are simply not sent.
pub struct NoPush;

#[async_trait]
impl PushSender for NoPush {
    fn enabled(&self) -> bool {
        false
    }
    async fn send(&self, _msg: &Message) -> anyhow::Result<SendOutcome> {
        anyhow::bail!("push is not configured")
    }
}

/// Records messages instead of sending them. Used by tests.
#[derive(Default)]
pub struct MockSender {
    pub sent: Mutex<Vec<Message>>,
    pub dead_tokens: Mutex<HashSet<String>>,
}

#[async_trait]
impl PushSender for MockSender {
    fn enabled(&self) -> bool {
        true
    }
    async fn send(&self, msg: &Message) -> anyhow::Result<SendOutcome> {
        if self.dead_tokens.lock().unwrap().contains(&msg.token) {
            return Ok(SendOutcome::Unregistered);
        }
        self.sent.lock().unwrap().push(msg.clone());
        Ok(SendOutcome::Sent)
    }
}

// ---------- FCM HTTP v1 ----------

#[derive(Deserialize)]
struct ServiceAccount {
    project_id: String,
    client_email: String,
    private_key: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".into()
}

#[derive(Serialize)]
struct JwtClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: i64,
    exp: i64,
}

/// How reminders are delivered.
/// `Notification`: FCM carries a notification block, so Android shows it itself even when the
/// app is not running (no action buttons possible). `Data`: data only; the app builds the
/// notification in a background handler so it can offer a Mark done action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PushMode {
    #[default]
    Notification,
    Data,
}

impl PushMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "notification" => Some(Self::Notification),
            "data" => Some(Self::Data),
            _ => None,
        }
    }
}

pub const CHANNEL_ID: &str = "reminders";

/// The FCM HTTP v1 request body for one message.
pub fn fcm_payload(msg: &Message, mode: PushMode) -> serde_json::Value {
    match mode {
        PushMode::Notification => serde_json::json!({
            "message": {
                "token": msg.token,
                "notification": { "title": msg.title, "body": msg.body },
                "data": msg.data,
                "android": { "priority": "HIGH", "notification": { "channel_id": CHANNEL_ID } }
            }
        }),
        PushMode::Data => {
            // Data values must be strings. The app turns this into a notification itself.
            let mut data = msg.data.clone();
            data.insert("title".into(), msg.title.clone());
            data.insert("body".into(), msg.body.clone());
            data.insert("channel_id".into(), CHANNEL_ID.into());
            serde_json::json!({
                "message": {
                    "token": msg.token,
                    "data": data,
                    "android": { "priority": "HIGH" }
                }
            })
        }
    }
}

pub struct Fcm {
    mode: PushMode,
    client: reqwest::Client,
    project_id: String,
    client_email: String,
    key: EncodingKey,
    token_uri: String,
    cached: tokio::sync::Mutex<Option<(String, Instant)>>,
}

impl Fcm {
    pub fn from_file(path: &Path, mode: PushMode) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let sa: ServiceAccount = serde_json::from_str(&raw).context("parsing service account json")?;
        Ok(Self {
            mode,
            client: reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?,
            key: EncodingKey::from_rsa_pem(sa.private_key.as_bytes()).context("service account private key")?,
            project_id: sa.project_id,
            client_email: sa.client_email,
            token_uri: sa.token_uri,
            cached: tokio::sync::Mutex::new(None),
        })
    }

    async fn access_token(&self) -> anyhow::Result<String> {
        let mut cached = self.cached.lock().await;
        if let Some((tok, exp)) = cached.as_ref() {
            if Instant::now() < *exp {
                return Ok(tok.clone());
            }
        }
        let now = Utc::now().timestamp();
        let claims = JwtClaims {
            iss: &self.client_email,
            scope: "https://www.googleapis.com/auth/firebase.messaging",
            aud: &self.token_uri,
            iat: now,
            exp: now + 3600,
        };
        let jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)?;
        let body = format!("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={jwt}");
        let resp = self
            .client
            .post(&self.token_uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("fcm token exchange failed: {}", resp.status());
        }
        #[derive(Deserialize)]
        struct Tok {
            access_token: String,
            expires_in: u64,
        }
        let t: Tok = resp.json().await?;
        let exp = Instant::now() + Duration::from_secs(t.expires_in.saturating_sub(120));
        *cached = Some((t.access_token.clone(), exp));
        Ok(t.access_token)
    }
}

#[async_trait]
impl PushSender for Fcm {
    fn enabled(&self) -> bool {
        true
    }

    async fn send(&self, msg: &Message) -> anyhow::Result<SendOutcome> {
        let access = self.access_token().await?;
        let payload = fcm_payload(msg, self.mode);
        let url = format!("https://fcm.googleapis.com/v1/projects/{}/messages:send", self.project_id);
        let resp = self.client.post(url).bearer_auth(access).json(&payload).send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(SendOutcome::Sent);
        }
        let text = resp.text().await.unwrap_or_default();
        if text.contains("UNREGISTERED") || text.contains("not a valid FCM registration token") {
            return Ok(SendOutcome::Unregistered);
        }
        anyhow::bail!("fcm send failed: {status}: {text}")
    }
}

// ---------- what is due ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Due {
    pub item_id: String,
    pub name: String,
    pub kind: String,
    pub date: String,
}

pub fn in_quiet_hours(now: NaiveTime, quiet: Option<(NaiveTime, NaiveTime)>) -> bool {
    match quiet {
        None => false,
        Some((s, e)) if s <= e => now >= s && now < e,
        Some((s, e)) => now >= s || now < e, // wraps midnight
    }
}

/// Reminders that should go out now. A reminder is due once its local time has passed,
/// until `grace_mins` later; it is skipped when already done today or already sent.
pub fn due_reminders(
    now: DateTime<Tz>,
    items: &[Item],
    completions: &[Completion],
    sent: &HashSet<(String, String)>,
    grace_mins: i64,
    quiet: Option<(NaiveTime, NaiveTime)>,
) -> Vec<Due> {
    let local = now.naive_local();
    let today = local.date();
    let date = today.format("%Y-%m-%d").to_string();
    if in_quiet_hours(local.time(), quiet) {
        return Vec::new();
    }
    let done: HashSet<&str> = completions
        .iter()
        .filter(|c| c.date == date && c.deleted_at.is_none())
        .map(|c| c.item_id.as_str())
        .collect();

    let mut out = Vec::new();
    for it in items.iter().filter(|i| i.active && i.deleted_at.is_none()) {
        let Some(rt) = it.remind_time.as_deref().and_then(|t| NaiveTime::parse_from_str(t, "%H:%M").ok()) else {
            continue;
        };
        let applies = match it.kind.as_str() {
            "chore" | "habit" => it.weekdays & (1 << today.weekday().num_days_from_monday()) != 0,
            "todo" => it.due_date.as_deref() == Some(date.as_str()),
            _ => false,
        };
        if !applies || done.contains(it.id.as_str()) || sent.contains(&(it.id.clone(), date.clone())) {
            continue;
        }
        let at = today.and_time(rt);
        let late = (local - at).num_minutes();
        if late < 0 || late > grace_mins {
            continue;
        }
        out.push(Due { item_id: it.id.clone(), name: it.name.clone(), kind: it.kind.clone(), date: date.clone() });
    }
    out
}

fn body_for(kind: &str) -> &'static str {
    match kind {
        "chore" => "Chore due now",
        "habit" => "Habit check-in",
        _ => "To-do due today",
    }
}

fn parse_quiet(start: &str, end: &str) -> Option<(NaiveTime, NaiveTime)> {
    let s = NaiveTime::parse_from_str(start, "%H:%M").ok()?;
    let e = NaiveTime::parse_from_str(end, "%H:%M").ok()?;
    Some((s, e))
}

/// One scheduler pass. Returns the number of reminders sent.
pub async fn tick(st: &AppState, now_utc: DateTime<Utc>) -> anyhow::Result<usize> {
    if !st.push.enabled() || store::get_setting(&st.db, "reminders_enabled").await? != "1" {
        return Ok(0);
    }
    let tz = Tz::from_str(&store::get_setting(&st.db, "timezone").await?).unwrap_or(chrono_tz::Australia::Perth);
    let quiet = parse_quiet(
        &store::get_setting(&st.db, "quiet_start").await?,
        &store::get_setting(&st.db, "quiet_end").await?,
    );
    let now = tz.from_utc_datetime(&now_utc.naive_utc());
    let date = now.format("%Y-%m-%d").to_string();

    let items = store::all_items(&st.db).await?;
    let completions: Vec<Completion> = store::all_completions(&st.db)
        .await?
        .into_iter()
        .filter(|c| c.date == date)
        .collect();
    let sent: HashSet<(String, String)> = sqlx::query_as::<_, (String, String)>("SELECT item_id, date FROM reminders_sent WHERE date = ?")
        .bind(&date)
        .fetch_all(&st.db)
        .await?
        .into_iter()
        .collect();

    let due = due_reminders(now, &items, &completions, &sent, st.config.reminder_grace_mins, quiet);
    if due.is_empty() {
        return Ok(0);
    }
    let devices: Vec<(String, String)> =
        sqlx::query_as("SELECT id, fcm_token FROM devices WHERE fcm_token IS NOT NULL AND fcm_token != ''")
            .fetch_all(&st.db)
            .await?;
    if devices.is_empty() {
        return Ok(0); // not marked sent, so it can still go out after a device registers
    }

    let mut count = 0;
    for d in due {
        let mut delivered = false;
        for (device_id, token) in &devices {
            let msg = Message {
                token: token.clone(),
                title: d.name.clone(),
                body: body_for(&d.kind).into(),
                data: HashMap::from([
                    ("item_id".to_string(), d.item_id.clone()),
                    ("date".to_string(), d.date.clone()),
                    ("kind".to_string(), d.kind.clone()),
                ]),
            };
            match st.push.send(&msg).await {
                Ok(SendOutcome::Sent) => delivered = true,
                Ok(SendOutcome::Unregistered) => {
                    tracing::info!(device = %device_id, "fcm token unregistered, clearing");
                    sqlx::query("UPDATE devices SET fcm_token = NULL WHERE id = ?")
                        .bind(device_id)
                        .execute(&st.db)
                        .await?;
                }
                Err(e) => tracing::warn!(error = ?e, item = %d.item_id, "reminder send failed"),
            }
        }
        if delivered {
            sqlx::query("INSERT OR IGNORE INTO reminders_sent (item_id, date, sent_at) VALUES (?, ?, ?)")
                .bind(&d.item_id)
                .bind(&d.date)
                .bind(now_ms())
                .execute(&st.db)
                .await?;
            count += 1;
        }
    }
    Ok(count)
}

pub fn spawn_scheduler(st: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            if let Err(e) = tick(&st, Utc::now()).await {
                tracing::error!(error = ?e, "reminder tick failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg() -> Message {
        Message {
            token: "tok".into(),
            title: "Recycling".into(),
            body: "Chore due now".into(),
            data: HashMap::from([
                ("item_id".to_string(), "i1".to_string()),
                ("date".to_string(), "2026-10-19".to_string()),
                ("kind".to_string(), "chore".to_string()),
            ]),
        }
    }

    #[test]
    fn notification_mode_keeps_the_existing_payload() {
        let v = fcm_payload(&msg(), PushMode::Notification);
        assert_eq!(v["message"]["notification"]["title"], "Recycling");
        assert_eq!(v["message"]["notification"]["body"], "Chore due now");
        assert_eq!(v["message"]["data"]["item_id"], "i1");
        assert_eq!(v["message"]["android"]["priority"], "HIGH");
        assert_eq!(v["message"]["android"]["notification"]["channel_id"], "reminders");
    }

    #[test]
    fn data_mode_has_no_notification_block_and_carries_the_text_as_data() {
        let v = fcm_payload(&msg(), PushMode::Data);
        assert!(v["message"].get("notification").is_none(), "a notification block would make Android show it itself");
        let d = &v["message"]["data"];
        assert_eq!((d["title"].as_str(), d["body"].as_str()), (Some("Recycling"), Some("Chore due now")));
        assert_eq!((d["item_id"].as_str(), d["date"].as_str(), d["kind"].as_str()), (Some("i1"), Some("2026-10-19"), Some("chore")));
        assert_eq!(v["message"]["android"]["priority"], "HIGH", "high priority so it wakes the app");
        assert!(d.as_object().unwrap().values().all(|x| x.is_string()), "FCM data values must be strings");
    }

    #[test]
    fn push_mode_parsing() {
        assert_eq!(PushMode::parse("DATA"), Some(PushMode::Data));
        assert_eq!(PushMode::parse(" notification "), Some(PushMode::Notification));
        assert_eq!(PushMode::parse("both"), None);
        assert_eq!(PushMode::default(), PushMode::Notification);
    }
}
