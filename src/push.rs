//! Web Push (VAPID) so notifications arrive even when the tab is in the background
//! or closed. The VAPID key pair is generated on first start and kept in the DB.

use axum::{Json, extract::State};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use web_push::{
    ContentEncoding, HyperWebPushClient, PartialVapidSignatureBuilder, SubscriptionInfo, Urgency,
    VapidSignatureBuilder, WebPushClient, WebPushError, WebPushMessageBuilder,
};

use crate::{
    AppState,
    auth::AuthUser,
    error::{AppError, AppResult},
    util::now,
};

pub struct Push {
    vapid: PartialVapidSignatureBuilder,
    public_key: String,
    subject: String,
    client: HyperWebPushClient,
}

#[derive(Serialize, Clone, Debug)]
pub struct Notification {
    title: String,
    body: String,
    /// Same tag replaces the previous notification for the group.
    tag: String,
    url: String,
    group_id: i64,
}

impl Notification {
    pub fn new(title: String, body: String, group_id: i64) -> Self {
        Self { title, body, tag: format!("kaffe-{group_id}"), url: "/".into(), group_id }
    }
}

impl Push {
    pub async fn init(db: &SqlitePool, subject: String) -> anyhow::Result<Self> {
        let existing: Option<String> =
            sqlx::query_scalar("SELECT value FROM app_settings WHERE key = 'vapid_private_key'")
                .fetch_optional(db)
                .await?;
        let private = match existing {
            Some(k) => k,
            None => {
                let secret = p256::SecretKey::random(&mut rand::rngs::OsRng);
                let encoded = URL_SAFE_NO_PAD.encode(secret.to_bytes());
                sqlx::query("INSERT INTO app_settings (key, value) VALUES ('vapid_private_key', ?)")
                    .bind(&encoded)
                    .execute(db)
                    .await?;
                tracing::info!("generated new VAPID key pair");
                encoded
            }
        };
        let vapid = VapidSignatureBuilder::from_base64_no_sub(&private)
            .map_err(|e| anyhow::anyhow!("invalid VAPID key: {e}"))?;
        let public_key = URL_SAFE_NO_PAD.encode(vapid.get_public_key());
        Ok(Self { vapid, public_key, subject, client: HyperWebPushClient::new() })
    }

    /// Sends notifications in the background. Dead subscriptions are removed.
    pub fn send(self: &std::sync::Arc<Self>, db: SqlitePool, targets: Vec<(i64, Notification)>) {
        if targets.is_empty() {
            return;
        }
        let push = self.clone();
        tokio::spawn(async move {
            for (user_id, note) in targets {
                if let Err(e) = push.send_to_user(&db, user_id, &note).await {
                    tracing::warn!("push to user {user_id} failed: {e:?}");
                }
            }
        });
    }

    async fn send_to_user(&self, db: &SqlitePool, user_id: i64, note: &Notification) -> anyhow::Result<()> {
        let subs: Vec<(i64, String, String, String)> =
            sqlx::query_as("SELECT id, endpoint, p256dh, auth FROM push_subscriptions WHERE user_id = ?")
                .bind(user_id)
                .fetch_all(db)
                .await?;
        let payload = serde_json::to_vec(note)?;
        for (id, endpoint, p256dh, auth) in subs {
            let info = SubscriptionInfo::new(endpoint, p256dh, auth);
            match self.send_one(&info, &payload).await {
                Ok(()) => {}
                Err(WebPushError::EndpointNotValid(_) | WebPushError::EndpointNotFound(_)) => {
                    sqlx::query("DELETE FROM push_subscriptions WHERE id = ?").bind(id).execute(db).await?;
                }
                Err(e) => tracing::warn!("push send failed: {e:?}"),
            }
        }
        Ok(())
    }

    async fn send_one(&self, info: &SubscriptionInfo, payload: &[u8]) -> Result<(), WebPushError> {
        let mut sig = self.vapid.clone().add_sub_info(info);
        sig.add_claim("sub", self.subject.as_str());
        let mut builder = WebPushMessageBuilder::new(info);
        builder.set_payload(ContentEncoding::Aes128Gcm, payload);
        builder.set_vapid_signature(sig.build()?);
        builder.set_ttl(15 * 60);
        builder.set_urgency(Urgency::High);
        self.client.send(builder.build()?).await
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn public_key(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "public_key": state.push.public_key }))
}

#[derive(Deserialize)]
pub struct Keys {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
pub struct Subscription {
    endpoint: String,
    keys: Keys,
}

pub async fn subscribe(
    State(state): State<AppState>,
    user: AuthUser,
    Json(sub): Json<Subscription>,
) -> AppResult<Json<Value>> {
    if !sub.endpoint.starts_with("https://") || sub.endpoint.len() > 2048 {
        return Err(AppError::bad("Invalid push endpoint"));
    }
    if sub.keys.p256dh.len() > 256 || sub.keys.auth.len() > 256 {
        return Err(AppError::bad("Invalid push keys"));
    }
    sqlx::query(
        "INSERT INTO push_subscriptions (user_id, endpoint, p256dh, auth, created_at) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(endpoint) DO UPDATE SET user_id = excluded.user_id, p256dh = excluded.p256dh, auth = excluded.auth",
    )
    .bind(user.id)
    .bind(&sub.endpoint)
    .bind(&sub.keys.p256dh)
    .bind(&sub.keys.auth)
    .bind(now())
    .execute(&state.db)
    .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct Unsubscribe {
    endpoint: String,
}

pub async fn unsubscribe(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<Unsubscribe>,
) -> AppResult<Json<Value>> {
    sqlx::query("DELETE FROM push_subscriptions WHERE endpoint = ? AND user_id = ?")
        .bind(&body.endpoint)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

/// Sends a test notification to the current user's devices.
pub async fn test(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM push_subscriptions WHERE user_id = ?")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    if count == 0 {
        return Err(AppError::bad("No devices are subscribed to notifications"));
    }
    let note = Notification {
        title: "☕ Kaffeknap".into(),
        body: "Notifications work!".into(),
        tag: "kaffe-test".into(),
        url: "/".into(),
        group_id: 0,
    };
    state.push.send(state.db.clone(), vec![(user.id, note)]);
    Ok(Json(json!({ "devices": count })))
}
