//! Accounts and sessions.
//!
//! Password handling:
//! - Passwords are hashed with Argon2id (OWASP-recommended parameters: 19 MiB,
//!   2 iterations, 1 lane) with a per-password random salt, stored as a PHC string.
//! - No password rules (length/content) are enforced, by design.
//! - Hashing/verification runs on the blocking thread pool.
//! - Login with unknown initials still performs a verification against a dummy
//!   hash so response timing doesn't reveal which accounts exist.
//! - Repeated failed logins for an account are throttled with exponential backoff.
//! - Changing the password revokes all other sessions.
//!
//! Sessions: a random 256-bit token is stored in an HttpOnly, SameSite=Lax cookie;
//! the database only stores its SHA-256 hash, so a leaked database can't be used
//! to hijack sessions.

use std::{collections::HashMap, sync::OnceLock};

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use axum::{
    Json,
    extract::{FromRequestParts, State},
    http::request::Parts,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    AppState,
    error::{AppError, AppResult},
    util::{now, random_token},
};

pub const SESSION_COOKIE: &str = "kaffe_session";
const SESSION_DAYS: i64 = 60;

fn argon2() -> Argon2<'static> {
    // m = 19 MiB, t = 2, p = 1 (OWASP Password Storage Cheat Sheet).
    let params = Params::new(19 * 1024, 2, 1, None).expect("valid argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

async fn hash_password(password: String) -> AppResult<String> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        argon2()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| anyhow::anyhow!("hash failed: {e}"))
    })
    .await?
    .map_err(AppError::Internal)
}

async fn verify_password(password: String, hash: String) -> AppResult<bool> {
    Ok(tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash)
            .map(|parsed| argon2().verify_password(password.as_bytes(), &parsed).is_ok())
            .unwrap_or(false)
    })
    .await?)
}

fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        let salt = SaltString::generate(&mut OsRng);
        argon2()
            .hash_password(random_token().as_bytes(), &salt)
            .expect("dummy hash")
            .to_string()
    })
}

fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Validates and normalises initials: 2–6 letters, stored upper-case.
pub fn normalize_initials(raw: &str) -> AppResult<String> {
    let s: String = raw.trim().to_uppercase();
    let n = s.chars().count();
    if !(2..=6).contains(&n) || !s.chars().all(char::is_alphabetic) {
        return Err(AppError::bad("Initials must be 2–6 letters"));
    }
    Ok(s)
}

// ---------------------------------------------------------------------------
// Login throttling
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct LoginLimiter {
    inner: std::sync::Mutex<HashMap<String, (u32, i64)>>, // initials -> (failures, locked_until)
}

const FREE_ATTEMPTS: u32 = 5;

impl LoginLimiter {
    /// Seconds remaining before another attempt is allowed, if locked.
    fn locked_for(&self, key: &str) -> Option<i64> {
        let map = self.inner.lock().unwrap();
        map.get(key).and_then(|&(_, until)| {
            let left = until - now();
            (left > 0).then_some(left)
        })
    }

    fn fail(&self, key: &str) {
        let mut map = self.inner.lock().unwrap();
        if map.len() > 10_000 {
            let t = now();
            map.retain(|_, (_, until)| *until > t);
        }
        let entry = map.entry(key.to_string()).or_insert((0, 0));
        entry.0 += 1;
        if entry.0 >= FREE_ATTEMPTS {
            let exp = (entry.0 - FREE_ATTEMPTS).min(5);
            entry.1 = now() + 30 * (1 << exp); // 30s, 60s, ... up to 16 min
        }
    }

    fn success(&self, key: &str) {
        self.inner.lock().unwrap().remove(key);
    }
}

// ---------------------------------------------------------------------------
// Session extractor
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AuthUser {
    pub id: i64,
    pub initials: String,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar.get(SESSION_COOKIE).ok_or(AppError::Unauthorized)?.value();
        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT u.id, u.initials FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ? AND s.expires_at > ?",
        )
        .bind(token_hash(token))
        .bind(now())
        .fetch_optional(&state.db)
        .await?;
        let (id, initials) = row.ok_or(AppError::Unauthorized)?;
        Ok(AuthUser { id, initials })
    }
}

async fn create_session(state: &AppState, jar: CookieJar, user_id: i64) -> AppResult<CookieJar> {
    let token = random_token();
    let t = now();
    sqlx::query("INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)")
        .bind(token_hash(&token))
        .bind(user_id)
        .bind(t)
        .bind(t + SESSION_DAYS * 86_400)
        .execute(&state.db)
        .await?;
    let cookie = Cookie::build((SESSION_COOKIE, token))
        .http_only(true)
        .secure(state.config.secure_cookies)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::days(SESSION_DAYS))
        .build();
    Ok(jar.add(cookie))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct Credentials {
    initials: String,
    password: String,
}

#[derive(Serialize)]
pub struct MeResponse {
    id: i64,
    initials: String,
    active_group_id: Option<i64>,
}

pub async fn signup(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(body): Json<Credentials>,
) -> AppResult<(CookieJar, Json<MeResponse>)> {
    let initials = normalize_initials(&body.initials)?;

    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE initials = ?")
        .bind(&initials)
        .fetch_optional(&state.db)
        .await?;
    if exists.is_some() {
        return Err(AppError::conflict("Those initials are taken. Try adding a letter."));
    }

    let hash = hash_password(body.password).await?;
    let res = sqlx::query("INSERT INTO users (initials, password_hash, created_at) VALUES (?, ?, ?)")
        .bind(&initials)
        .bind(hash)
        .bind(now())
        .execute(&state.db)
        .await;
    let id = match res {
        Ok(r) => r.last_insert_rowid(),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(AppError::conflict("Those initials are taken. Try adding a letter."));
        }
        Err(e) => return Err(e.into()),
    };
    let jar = create_session(&state, jar, id).await?;
    Ok((jar, Json(MeResponse { id, initials, active_group_id: None })))
}

pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(body): Json<Credentials>,
) -> AppResult<(CookieJar, Json<MeResponse>)> {
    let invalid = || AppError::bad("Wrong initials or password");
    let initials = body.initials.trim().to_uppercase();
    if initials.chars().count() > 32 {
        return Err(invalid());
    }
    if let Some(secs) = state.login_limiter.locked_for(&initials) {
        return Err(AppError::TooManyRequests(format!(
            "Too many failed attempts. Try again in {secs} seconds."
        )));
    }

    let row: Option<(i64, String, String, Option<i64>)> =
        sqlx::query_as("SELECT id, initials, password_hash, active_group_id FROM users WHERE initials = ?")
            .bind(&initials)
            .fetch_optional(&state.db)
            .await?;

    let (stored_hash, user) = match row {
        Some((id, ini, hash, active)) => (hash, Some((id, ini, active))),
        None => (dummy_hash().to_string(), None),
    };
    let ok = verify_password(body.password, stored_hash).await?;

    match (ok, user) {
        (true, Some((id, initials, active_group_id))) => {
            state.login_limiter.success(&initials);
            let jar = create_session(&state, jar, id).await?;
            Ok((jar, Json(MeResponse { id, initials, active_group_id })))
        }
        _ => {
            state.login_limiter.fail(&initials);
            Err(invalid())
        }
    }
}

pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> AppResult<(CookieJar, Json<Value>)> {
    if let Some(c) = jar.get(SESSION_COOKIE) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash(c.value()))
            .execute(&state.db)
            .await?;
    }
    let jar = jar.remove(Cookie::build(SESSION_COOKIE).path("/"));
    Ok((jar, Json(json!({ "ok": true }))))
}

#[derive(Deserialize)]
pub struct ChangePassword {
    current_password: String,
    new_password: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    user: AuthUser,
    jar: CookieJar,
    Json(body): Json<ChangePassword>,
) -> AppResult<(CookieJar, Json<Value>)> {
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    if !verify_password(body.current_password, hash).await? {
        return Err(AppError::bad("Current password is wrong"));
    }
    let new_hash = hash_password(body.new_password).await?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(new_hash)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    // Revoke every session (including this one) and issue a fresh one.
    sqlx::query("DELETE FROM sessions WHERE user_id = ?").bind(user.id).execute(&state.db).await?;
    let jar = create_session(&state, jar, user.id).await?;
    Ok((jar, Json(json!({ "ok": true }))))
}

pub async fn me(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<MeResponse>> {
    let active_group_id: Option<i64> = sqlx::query_scalar("SELECT active_group_id FROM users WHERE id = ?")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(MeResponse { id: user.id, initials: user.initials, active_group_id }))
}

pub async fn purge_expired_sessions(state: &AppState) -> AppResult<()> {
    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?").bind(now()).execute(&state.db).await?;
    Ok(())
}
