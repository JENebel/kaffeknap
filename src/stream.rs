//! Server-sent events: tells a logged-in client when something it can see changed.
//! The payload is only a hint ("group 3 changed"); the client re-fetches state
//! through the normal authorised endpoints.

use std::{convert::Infallible, time::Duration};

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures::Stream;
use tokio::sync::broadcast::error::RecvError;

use crate::{AppState, Change, auth::AuthUser};

pub async fn stream(
    State(state): State<AppState>,
    user: AuthUser,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.changes.subscribe();
    let db = state.db.clone();
    let uid = user.id;
    let shutdown = state.shutdown.clone();

    let s = async_stream_lite(move |tx| async move {
        if tx.send(Event::default().event("hello").data("{}")).await.is_err() {
            return;
        }
        loop {
            let msg = tokio::select! {
                msg = rx.recv() => msg,
                _ = shutdown.cancelled() => return, // server stopping: end the stream
            };
            let ev = match msg {
                Ok(Change::User(id)) if id == uid => Event::default().event("user").data("{}"),
                Ok(Change::User(_)) => continue,
                Ok(Change::Group(gid)) => {
                    let member: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM memberships WHERE group_id = ? AND user_id = ?)",
                    )
                    .bind(gid)
                    .bind(uid)
                    .fetch_one(&db)
                    .await
                    .unwrap_or(false);
                    if !member {
                        continue;
                    }
                    Event::default().event("group").data(format!("{{\"group_id\":{gid}}}"))
                }
                // Missed messages: ask the client to refresh everything.
                Err(RecvError::Lagged(_)) => Event::default().event("resync").data("{}"),
                Err(RecvError::Closed) => return,
            };
            if tx.send(ev).await.is_err() {
                return; // client went away
            }
        }
    });

    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}

/// Minimal generator: runs `f` with a channel sender and yields what it sends.
fn async_stream_lite<F, Fut>(f: F) -> impl Stream<Item = Result<Event, Infallible>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<Event>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(f(tx));
    futures::StreamExt::map(tokio_stream::wrappers::ReceiverStream::new(rx), Ok)
}
