//! The coffee button: ready presses, the threshold, live breaks and history.

use std::{collections::HashSet, time::Duration};

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AppState, Change, auth,
    auth::AuthUser,
    error::AppResult,
    error::AppError,
    groups::{Group, require_member},
    push::Notification,
    util::now,
};

#[derive(sqlx::FromRow, Serialize)]
pub struct ReadyEntry {
    user_id: i64,
    initials: String,
    pressed_at: i64,
    expires_at: i64,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct Participant {
    user_id: i64,
    initials: String,
    pressed_at: i64,
    late_join: bool,
}

#[derive(sqlx::FromRow)]
struct LiveEventRow {
    id: i64,
    started_at: i64,
    expires_at: i64,
    started_by: Option<String>,
    triggered_by: Option<String>,
}

#[derive(Serialize)]
pub struct LiveEvent {
    id: i64,
    started_at: i64,
    expires_at: i64,
    started_by: Option<String>,
    triggered_by: Option<String>,
    participants: Vec<Participant>,
    i_participate: bool,
}

#[derive(Serialize)]
pub struct GroupState {
    group_id: i64,
    name: String,
    is_owner: bool,
    min_ready: i64,
    press_ttl_minutes: i64,
    event_ttl_minutes: i64,
    ready: Vec<ReadyEntry>,
    me_ready: bool,
    live_event: Option<LiveEvent>,
    server_time: i64,
}

async fn live_event_id<'e, E: sqlx::SqliteExecutor<'e>>(db: E, group_id: i64, t: i64) -> AppResult<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM events WHERE group_id = ? AND status = 'live' AND expires_at > ?")
            .bind(group_id)
            .bind(t)
            .fetch_optional(db)
            .await?,
    )
}

async fn load_state(state: &AppState, group: Group, user_id: i64) -> AppResult<GroupState> {
    let t = now();
    let ready: Vec<ReadyEntry> = sqlx::query_as(
        "SELECT p.user_id, u.initials, p.pressed_at, p.expires_at
         FROM presses p JOIN users u ON u.id = p.user_id
         WHERE p.group_id = ? AND p.expires_at > ? ORDER BY p.pressed_at",
    )
    .bind(group.id)
    .bind(t)
    .fetch_all(&state.db)
    .await?;

    let row: Option<LiveEventRow> = sqlx::query_as(
        "SELECT e.id, e.started_at, e.expires_at, sb.initials AS started_by, tb.initials AS triggered_by
         FROM events e
         LEFT JOIN users sb ON sb.id = e.started_by
         LEFT JOIN users tb ON tb.id = e.triggered_by
         WHERE e.group_id = ? AND e.status = 'live' AND e.expires_at > ?",
    )
    .bind(group.id)
    .bind(t)
    .fetch_optional(&state.db)
    .await?;

    let live_event = match row {
        None => None,
        Some(ev) => {
            let participants: Vec<Participant> = sqlx::query_as(
                "SELECT ep.user_id, u.initials, ep.pressed_at, ep.late_join
                 FROM event_participants ep JOIN users u ON u.id = ep.user_id
                 WHERE ep.event_id = ? ORDER BY ep.pressed_at",
            )
            .bind(ev.id)
            .fetch_all(&state.db)
            .await?;
            let i_participate = participants.iter().any(|p| p.user_id == user_id);
            Some(LiveEvent {
                id: ev.id,
                started_at: ev.started_at,
                expires_at: ev.expires_at,
                started_by: ev.started_by,
                triggered_by: ev.triggered_by,
                participants,
                i_participate,
            })
        }
    };

    Ok(GroupState {
        group_id: group.id,
        name: group.name,
        is_owner: group.owner_id == user_id,
        min_ready: group.min_ready,
        press_ttl_minutes: group.press_ttl_minutes,
        event_ttl_minutes: group.event_ttl_minutes,
        me_ready: ready.iter().any(|r| r.user_id == user_id),
        ready,
        live_event,
        server_time: t,
    })
}

pub async fn get_state(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<GroupState>> {
    let group = require_member(&state, id, user.id).await?;
    Ok(Json(load_state(&state, group, user.id).await?))
}

// ---------------------------------------------------------------------------
// Threshold → event
// ---------------------------------------------------------------------------

struct EventStarted {
    participants: Vec<i64>,
}

/// Starts a coffee break if enough people are ready and none is live.
/// Caller must hold `state.coffee_lock`.
async fn check_threshold_locked(
    state: &AppState,
    group_id: i64,
    triggered_by: Option<i64>,
) -> AppResult<Option<EventStarted>> {
    let t = now();
    let mut tx = state.db.begin().await?;
    if live_event_id(&mut *tx, group_id, t).await?.is_some() {
        return Ok(None);
    }
    let (min_ready, event_ttl): (i64, i64) =
        sqlx::query_as("SELECT min_ready, event_ttl_minutes FROM groups WHERE id = ?")
            .bind(group_id)
            .fetch_one(&mut *tx)
            .await?;
    let presses: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT user_id, pressed_at FROM presses WHERE group_id = ? AND expires_at > ? ORDER BY pressed_at, rowid",
    )
    .bind(group_id)
    .bind(t)
    .fetch_all(&mut *tx)
    .await?;
    if (presses.len() as i64) < min_ready || presses.is_empty() {
        return Ok(None);
    }

    // Any leftover live-but-expired event for this group is dropped (never history).
    sqlx::query("DELETE FROM events WHERE group_id = ? AND status = 'live'")
        .bind(group_id)
        .execute(&mut *tx)
        .await?;

    let event_id = sqlx::query(
        "INSERT INTO events (group_id, started_by, triggered_by, started_at, expires_at, min_ready)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(group_id)
    .bind(presses[0].0)
    .bind(triggered_by)
    .bind(t)
    .bind(t + event_ttl * 60)
    .bind(min_ready)
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();

    for (uid, pressed_at) in &presses {
        sqlx::query("INSERT INTO event_participants (event_id, user_id, pressed_at) VALUES (?, ?, ?)")
            .bind(event_id)
            .bind(uid)
            .bind(pressed_at)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM presses WHERE group_id = ?").bind(group_id).execute(&mut *tx).await?;
    tx.commit().await?;

    Ok(Some(EventStarted { participants: presses.into_iter().map(|(u, _)| u).collect() }))
}

/// Public entry point (e.g. after settings changes); takes the lock itself.
pub async fn check_threshold(state: &AppState, group_id: i64, triggered_by: Option<i64>) -> AppResult<()> {
    let started = {
        let _guard = state.coffee_lock.lock().await;
        check_threshold_locked(state, group_id, triggered_by).await?
    };
    if let Some(ev) = started {
        notify_event_started(state, group_id, &ev).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct Prefs {
    user_id: i64,
    notify_press: bool,
    notify_event_any: bool,
    notify_event_mine: bool,
}

async fn member_prefs(state: &AppState, group_id: i64) -> AppResult<Vec<Prefs>> {
    Ok(sqlx::query_as(
        "SELECT user_id, notify_press, notify_event_any, notify_event_mine FROM memberships WHERE group_id = ?",
    )
    .bind(group_id)
    .fetch_all(&state.db)
    .await?)
}

async fn initials_of(state: &AppState, ids: &[i64]) -> AppResult<Vec<String>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(i) = sqlx::query_scalar::<_, String>("SELECT initials FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.db)
            .await?
        {
            out.push(i);
        }
    }
    Ok(out)
}

/// "A", "A and B", "A, B and C".
fn list_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

async fn notify_event_started(state: &AppState, group_id: i64, ev: &EventStarted) -> AppResult<()> {
    let names = initials_of(state, &ev.participants).await?;
    let who = list_names(&names);
    let are = if names.len() == 1 { "is" } else { "are" };
    let participants: HashSet<i64> = ev.participants.iter().copied().collect();
    let mut out = Vec::new();
    for p in member_prefs(state, group_id).await? {
        let mine = participants.contains(&p.user_id);
        if mine && (p.notify_event_mine || p.notify_event_any) {
            out.push((
                p.user_id,
                Notification::new("☕ Coffee time!".to_string(), format!("{who} {are} ready. Let's go!"), group_id),
            ));
        } else if !mine && p.notify_event_any {
            out.push((
                p.user_id,
                Notification::new(
                    "☕ Coffee break started".to_string(),
                    format!("{who} {are} heading for coffee. Join them!"),
                    group_id,
                ),
            ));
        }
    }
    state.push.send(state.db.clone(), out);
    Ok(())
}

async fn notify_pressed(state: &AppState, group_id: i64, presser: &AuthUser, joined_live: bool) -> AppResult<()> {
    let t = now();
    let body = if joined_live {
        format!("{} joined the coffee break", presser.initials)
    } else {
        let (count, min): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM presses WHERE group_id = ?1 AND expires_at > ?2),
                    (SELECT min_ready FROM groups WHERE id = ?1)",
        )
        .bind(group_id)
        .bind(t)
        .fetch_one(&state.db)
        .await?;
        format!("{} is ready for coffee ({count}/{min})", presser.initials)
    };
    let out = member_prefs(state, group_id)
        .await?
        .into_iter()
        .filter(|p| p.notify_press && p.user_id != presser.id)
        .map(|p| (p.user_id, Notification::new("☕ Ready for coffee".to_string(), body.clone(), group_id)))
        .collect();
    state.push.send(state.db.clone(), out);
    Ok(())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Press the button: indicate ready (or refresh), or join a live break.
pub async fn press(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<GroupState>> {
    let group = require_member(&state, id, user.id).await?;

    enum Outcome {
        NewPress,
        Refreshed,
        JoinedLive,
        AlreadyInLive,
        Started(EventStarted),
    }

    let outcome = {
        let _guard = state.coffee_lock.lock().await;
        let t = now();
        let mut tx = state.db.begin().await?;
        let outcome = if let Some(event_id) = live_event_id(&mut *tx, id, t).await? {
            let res = sqlx::query(
                "INSERT OR IGNORE INTO event_participants (event_id, user_id, pressed_at, late_join) VALUES (?, ?, ?, 1)",
            )
            .bind(event_id)
            .bind(user.id)
            .bind(t)
            .execute(&mut *tx)
            .await?;
            if res.rows_affected() > 0 { Outcome::JoinedLive } else { Outcome::AlreadyInLive }
        } else {
            let existing: Option<i64> = sqlx::query_scalar(
                "SELECT expires_at FROM presses WHERE group_id = ? AND user_id = ? AND expires_at > ?",
            )
            .bind(id)
            .bind(user.id)
            .bind(t)
            .fetch_optional(&mut *tx)
            .await?;
            let expires = t + group.press_ttl_minutes * 60;
            if existing.is_some() {
                // Pressing again extends the validity but keeps the original position.
                sqlx::query("UPDATE presses SET expires_at = ? WHERE group_id = ? AND user_id = ?")
                    .bind(expires)
                    .bind(id)
                    .bind(user.id)
                    .execute(&mut *tx)
                    .await?;
                Outcome::Refreshed
            } else {
                sqlx::query("INSERT OR REPLACE INTO presses (group_id, user_id, pressed_at, expires_at) VALUES (?, ?, ?, ?)")
                    .bind(id)
                    .bind(user.id)
                    .bind(t)
                    .bind(expires)
                    .execute(&mut *tx)
                    .await?;
                Outcome::NewPress
            }
        };
        tx.commit().await?;

        match outcome {
            Outcome::NewPress | Outcome::Refreshed => {
                match check_threshold_locked(&state, id, Some(user.id)).await? {
                    Some(ev) => Outcome::Started(ev),
                    None => outcome,
                }
            }
            other => other,
        }
    };

    match &outcome {
        Outcome::Started(ev) => notify_event_started(&state, id, ev).await?,
        Outcome::NewPress => notify_pressed(&state, id, &user, false).await?,
        Outcome::JoinedLive => notify_pressed(&state, id, &user, true).await?,
        Outcome::Refreshed | Outcome::AlreadyInLive => {}
    }
    state.notify(Change::Group(id));
    Ok(Json(load_state(&state, group, user.id).await?))
}

/// Retract a ready indication, or leave a live break.
pub async fn retract(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<GroupState>> {
    let group = require_member(&state, id, user.id).await?;
    {
        let _guard = state.coffee_lock.lock().await;
        let t = now();
        let mut tx = state.db.begin().await?;
        let res = sqlx::query("DELETE FROM presses WHERE group_id = ? AND user_id = ?")
            .bind(id)
            .bind(user.id)
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() == 0
            && let Some(event_id) = live_event_id(&mut *tx, id, t).await?
        {
            sqlx::query("DELETE FROM event_participants WHERE event_id = ? AND user_id = ?")
                .bind(event_id)
                .bind(user.id)
                .execute(&mut *tx)
                .await?;
            // A break nobody is part of anymore is cancelled.
            sqlx::query(
                "DELETE FROM events WHERE id = ? AND NOT EXISTS (SELECT 1 FROM event_participants WHERE event_id = ?)",
            )
            .bind(event_id)
            .bind(event_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    state.notify(Change::Group(id));
    Ok(Json(load_state(&state, group, user.id).await?))
}

/// A participant marks the live break as done, which saves it to history.
pub async fn complete_event(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<GroupState>> {
    let group = require_member(&state, id, user.id).await?;
    {
        let _guard = state.coffee_lock.lock().await;
        let t = now();
        let event_id = live_event_id(&state.db, id, t)
            .await?
            .ok_or_else(|| AppError::not_found("There is no live coffee break"))?;
        let res = sqlx::query(
            "UPDATE events SET status = 'completed', completed_at = ?, completed_by = ?
             WHERE id = ? AND EXISTS (SELECT 1 FROM event_participants WHERE event_id = ? AND user_id = ?)",
        )
        .bind(t)
        .bind(user.id)
        .bind(event_id)
        .bind(event_id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
        if res.rows_affected() == 0 {
            return Err(AppError::forbidden("Only participants can complete the coffee break"));
        }
    }
    state.notify(Change::Group(id));
    Ok(Json(load_state(&state, group, user.id).await?))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    limit: Option<i64>,
    /// Return events that started before this timestamp (pagination).
    before: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct HistoryRow {
    id: i64,
    started_at: i64,
    completed_at: Option<i64>,
    min_ready: i64,
    started_by: Option<String>,
    triggered_by: Option<String>,
    completed_by: Option<String>,
    participants: String,
}

#[derive(Serialize)]
pub struct HistoryEntry {
    id: i64,
    started_at: i64,
    completed_at: Option<i64>,
    min_ready: i64,
    started_by: Option<String>,
    triggered_by: Option<String>,
    completed_by: Option<String>,
    participants: Value,
}

pub async fn history(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<HistoryQuery>,
) -> AppResult<Json<Vec<HistoryEntry>>> {
    require_member(&state, id, user.id).await?;
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    let before = q.before.unwrap_or(i64::MAX);
    let rows: Vec<HistoryRow> = sqlx::query_as(
        "SELECT e.id, e.started_at, e.completed_at, e.min_ready,
                sb.initials AS started_by, tb.initials AS triggered_by, cb.initials AS completed_by,
                (SELECT json_group_array(json_object('initials', x.initials, 'pressed_at', x.pressed_at,
                                                     'late_join', json(CASE WHEN x.late_join THEN 'true' ELSE 'false' END)))
                 FROM (SELECT u.initials, ep.pressed_at, ep.late_join
                       FROM event_participants ep JOIN users u ON u.id = ep.user_id
                       WHERE ep.event_id = e.id ORDER BY ep.pressed_at) x) AS participants
         FROM events e
         LEFT JOIN users sb ON sb.id = e.started_by
         LEFT JOIN users tb ON tb.id = e.triggered_by
         LEFT JOIN users cb ON cb.id = e.completed_by
         WHERE e.group_id = ? AND e.status = 'completed' AND e.started_at < ?
         ORDER BY e.started_at DESC LIMIT ?",
    )
    .bind(id)
    .bind(before)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| HistoryEntry {
                id: r.id,
                started_at: r.started_at,
                completed_at: r.completed_at,
                min_ready: r.min_ready,
                started_by: r.started_by,
                triggered_by: r.triggered_by,
                completed_by: r.completed_by,
                participants: serde_json::from_str(&r.participants).unwrap_or(Value::Array(vec![])),
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// Background expiry
// ---------------------------------------------------------------------------

/// Periodically expires presses and drops live breaks nobody completed, and
/// tells connected clients about it.
pub async fn sweeper(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut n: u64 = 0;
    loop {
        tick.tick().await;
        n += 1;
        if let Err(e) = sweep(&state).await {
            tracing::warn!("sweep failed: {e:?}");
        }
        if n.is_multiple_of(720)
            && let Err(e) = auth::purge_expired_sessions(&state).await
        {
            tracing::warn!("session purge failed: {e:?}");
        }
    }
}

async fn sweep(state: &AppState) -> AppResult<()> {
    let _guard = state.coffee_lock.lock().await;
    let t = now();
    let mut groups: Vec<i64> =
        sqlx::query_scalar("SELECT DISTINCT group_id FROM presses WHERE expires_at <= ?")
            .bind(t)
            .fetch_all(&state.db)
            .await?;
    groups.extend(
        sqlx::query_scalar::<_, i64>("SELECT DISTINCT group_id FROM events WHERE status = 'live' AND expires_at <= ?")
            .bind(t)
            .fetch_all(&state.db)
            .await?,
    );
    if groups.is_empty() {
        return Ok(());
    }
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM presses WHERE expires_at <= ?").bind(t).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM events WHERE status = 'live' AND expires_at <= ?").bind(t).execute(&mut *tx).await?;
    tx.commit().await?;
    groups.sort_unstable();
    groups.dedup();
    for g in groups {
        state.notify(Change::Group(g));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::list_names;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn formats_name_lists() {
        assert_eq!(list_names(&names(&[])), "");
        assert_eq!(list_names(&names(&["JOEN"])), "JOEN");
        assert_eq!(list_names(&names(&["ABC", "DEF"])), "ABC and DEF");
        assert_eq!(list_names(&names(&["ABC", "DEF", "GHI"])), "ABC, DEF and GHI");
    }
}
