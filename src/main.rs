mod auth;
mod coffee;
mod error;
mod groups;
mod push;
mod stream;
mod util;

use std::{net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use axum::{
    Router,
    extract::Request,
    http::{HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use tokio::sync::{Mutex, broadcast};
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

/// Something changed that connected clients may want to re-fetch.
#[derive(Clone, Copy, Debug)]
pub enum Change {
    /// State of a group changed (presses, events, settings, membership).
    Group(i64),
    /// Something user-specific changed (e.g. a join request was approved).
    User(i64),
}

pub struct Config {
    /// Set the `Secure` flag on cookies. Enable when served over HTTPS.
    pub secure_cookies: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub changes: broadcast::Sender<Change>,
    pub push: Arc<push::Push>,
    pub config: Arc<Config>,
    pub login_limiter: Arc<auth::LoginLimiter>,
    /// Serialises coffee state mutations (press/retract/event transitions) so the
    /// threshold check and event creation are race-free.
    pub coffee_lock: Arc<Mutex<()>>,
    /// Cancelled when the server shuts down, so long-lived streams can end.
    pub shutdown: CancellationToken,
}

impl AppState {
    pub fn notify(&self, change: Change) {
        // No receivers is fine.
        let _ = self.changes.send(change);
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tower_http=info")),
        )
        .init();

    let bind: SocketAddr = env_or("KAFFE_BIND", "127.0.0.1:8080").parse()?;
    let db_path = env_or("KAFFE_DB", "kaffeknap.db");
    let static_dir = PathBuf::from(env_or("KAFFE_STATIC", "static"));
    let secure_cookies = matches!(env_or("KAFFE_SECURE_COOKIES", "false").as_str(), "1" | "true" | "yes");

    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{db_path}"))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let db = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;
    sqlx::migrate!("./migrations").run(&db).await?;

    let push = Arc::new(push::Push::init(&db, VAPID_SUBJECT.to_string()).await?);
    let (changes, _) = broadcast::channel(256);

    let state = AppState {
        db,
        changes,
        push,
        config: Arc::new(Config { secure_cookies }),
        login_limiter: Arc::new(auth::LoginLimiter::default()),
        coffee_lock: Arc::new(Mutex::new(())),
        shutdown: CancellationToken::new(),
    };
    let shutdown = state.shutdown.clone();

    tokio::spawn(coffee::sweeper(state.clone()));

    let api = Router::new()
        // Accounts
        .route("/auth/signup", post(auth::signup))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/password", post(auth::change_password))
        .route("/me", get(auth::me))
        .route("/me/active-group", put(groups::set_active_group))
        // Groups
        .route("/groups", get(groups::list_groups).post(groups::create_group))
        .route("/groups/{id}", get(groups::get_group).patch(groups::update_group).delete(groups::delete_group))
        .route("/groups/{id}/invite-code", post(groups::regenerate_invite))
        .route("/groups/{id}/members", get(groups::list_members))
        .route("/groups/{id}/members/{uid}", delete(groups::remove_member))
        .route("/groups/{id}/requests", get(groups::list_requests))
        .route("/groups/{id}/requests/{uid}/approve", post(groups::approve_request))
        .route("/groups/{id}/requests/{uid}/reject", post(groups::reject_request))
        .route("/groups/{id}/notifications", get(groups::get_notifications).put(groups::put_notifications))
        .route("/invite/{code}", get(groups::invite_info))
        .route("/invite/{code}/join", post(groups::join_by_invite))
        .route("/invite/{code}/request", delete(groups::cancel_request))
        // Coffee
        .route("/groups/{id}/state", get(coffee::get_state))
        .route("/groups/{id}/ready", post(coffee::press).delete(coffee::retract))
        .route("/groups/{id}/event/complete", post(coffee::complete_event))
        .route("/groups/{id}/history", get(coffee::history))
        // Push / live updates
        .route("/push/key", get(push::public_key))
        .route("/push/subscribe", post(push::subscribe))
        .route("/push/unsubscribe", post(push::unsubscribe))
        .route("/push/test", post(push::test))
        .route("/stream", get(stream::stream))
        .fallback(|| async { (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"error": "Not found"}))) })
        .layer(middleware::from_fn(csrf_guard))
        .layer(SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("no-store")));

    let index = static_dir.join("index.html");
    let static_files = ServeDir::new(&static_dir).fallback(ServeFile::new(index));

    let app = Router::new()
        .nest("/api", api)
        .fallback_service(static_files)
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; \
                 connect-src 'self'; manifest-src 'self'; worker-src 'self'; frame-ancestors 'none'; \
                 base-uri 'self'; form-action 'self'",
            ),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("Kaffeknap listening on http://{bind}");
    tokio::spawn(watch_signals(shutdown.clone()));

    let server = axum::serve(listener, app).with_graceful_shutdown({
        let token = shutdown.clone();
        async move { token.cancelled().await }
    });
    tokio::select! {
        res = std::future::IntoFuture::into_future(server) => res?,
        _ = async {
            shutdown.cancelled().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::warn!("connections still open after {SHUTDOWN_GRACE:?}; exiting anyway"),
    }
    tracing::info!("stopped");
    Ok(())
}

/// Contact sent to push services (Google/Mozilla/Apple) with every push.
const VAPID_SUBJECT: &str = "https://kaffe.joachimen.dk";

/// How long to wait for in-flight requests after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// CSRF protection: every state-changing API request must carry a custom header.
/// Browsers won't attach custom headers to cross-site requests without a CORS
/// preflight, which this server never approves. Combined with SameSite=Lax
/// cookies this blocks cross-site request forgery.
async fn csrf_guard(req: Request, next: Next) -> Response {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let has_header = req
        .headers()
        .get(HeaderName::from_static("x-requested-with"))
        .is_some_and(|v| v == "kaffeknap");
    if !safe && !has_header {
        return (StatusCode::FORBIDDEN, axum::Json(serde_json::json!({"error": "Missing CSRF header"})))
            .into_response();
    }
    next.run(req).await
}

/// First Ctrl+C / SIGTERM: graceful shutdown (live streams are closed, in-flight
/// requests get a few seconds). Second signal: exit immediately.
async fn watch_signals(shutdown: CancellationToken) {
    next_signal().await;
    tracing::info!("shutting down (press Ctrl+C again to force)");
    shutdown.cancel();
    next_signal().await;
    tracing::warn!("forced exit");
    std::process::exit(130);
}

async fn next_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
