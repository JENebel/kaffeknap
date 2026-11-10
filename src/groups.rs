//! Groups, membership, invites and join requests.

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    AppState, Change,
    auth::AuthUser,
    coffee,
    error::{AppError, AppResult},
    util::{invite_code, now},
};

#[derive(sqlx::FromRow, Serialize, Clone, Debug)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub owner_id: i64,
    pub invite_code: String,
    pub min_ready: i64,
    pub press_ttl_minutes: i64,
    pub event_ttl_minutes: i64,
    pub is_open: bool,
    pub created_at: i64,
}

const GROUP_COLS: &str =
    "g.id, g.name, g.owner_id, g.invite_code, g.min_ready, g.press_ttl_minutes, g.event_ttl_minutes, g.is_open, g.created_at";

/// Loads a group the user is a member of, or fails with 404/403.
pub async fn require_member(state: &AppState, group_id: i64, user_id: i64) -> AppResult<Group> {
    let group: Option<Group> = sqlx::query_as(&format!(
        "SELECT {GROUP_COLS} FROM groups g JOIN memberships m ON m.group_id = g.id
         WHERE g.id = ? AND m.user_id = ?"
    ))
    .bind(group_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    group.ok_or_else(|| AppError::not_found("Group not found or you're not a member"))
}

pub async fn require_owner(state: &AppState, group_id: i64, user_id: i64) -> AppResult<Group> {
    let group = require_member(state, group_id, user_id).await?;
    if group.owner_id != user_id {
        return Err(AppError::forbidden("Only the group owner can do that"));
    }
    Ok(group)
}

fn validate_name(raw: &str) -> AppResult<String> {
    let name = raw.trim().to_string();
    let n = name.chars().count();
    if n == 0 || n > 60 {
        return Err(AppError::bad("Group name must be 1–60 characters"));
    }
    Ok(name)
}

async fn add_member(state: &AppState, group_id: i64, user_id: i64) -> AppResult<()> {
    sqlx::query("INSERT OR IGNORE INTO memberships (group_id, user_id, joined_at) VALUES (?, ?, ?)")
        .bind(group_id)
        .bind(user_id)
        .bind(now())
        .execute(&state.db)
        .await?;
    sqlx::query("DELETE FROM join_requests WHERE group_id = ? AND user_id = ?")
        .bind(group_id)
        .bind(user_id)
        .execute(&state.db)
        .await?;
    // Make it the active group if the user doesn't have one yet.
    sqlx::query("UPDATE users SET active_group_id = ? WHERE id = ? AND active_group_id IS NULL")
        .bind(group_id)
        .bind(user_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Listing / CRUD
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow, Serialize)]
pub struct GroupSummary {
    id: i64,
    name: String,
    is_owner: bool,
    min_ready: i64,
    ready_count: i64,
    member_count: i64,
    live_event: bool,
    pending_requests: i64,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct PendingRequest {
    group_id: i64,
    name: String,
    invite_code: String,
    created_at: i64,
}

pub async fn list_groups(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<Value>> {
    let t = now();
    let groups: Vec<GroupSummary> = sqlx::query_as(
        "SELECT g.id, g.name, g.owner_id = ?1 AS is_owner, g.min_ready,
            (SELECT COUNT(*) FROM presses p WHERE p.group_id = g.id AND p.expires_at > ?2) AS ready_count,
            (SELECT COUNT(*) FROM memberships mm WHERE mm.group_id = g.id) AS member_count,
            EXISTS(SELECT 1 FROM events e WHERE e.group_id = g.id AND e.status = 'live' AND e.expires_at > ?2) AS live_event,
            CASE WHEN g.owner_id = ?1 THEN (SELECT COUNT(*) FROM join_requests r WHERE r.group_id = g.id) ELSE 0 END
                AS pending_requests
         FROM groups g JOIN memberships m ON m.group_id = g.id
         WHERE m.user_id = ?1
         ORDER BY g.name COLLATE NOCASE",
    )
    .bind(user.id)
    .bind(t)
    .fetch_all(&state.db)
    .await?;

    let pending: Vec<PendingRequest> = sqlx::query_as(
        "SELECT g.id AS group_id, g.name, g.invite_code, r.created_at
         FROM join_requests r JOIN groups g ON g.id = r.group_id WHERE r.user_id = ?",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(json!({ "groups": groups, "pending": pending })))
}

#[derive(Deserialize)]
pub struct CreateGroup {
    name: String,
}

pub async fn create_group(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<CreateGroup>,
) -> AppResult<Json<Value>> {
    let name = validate_name(&body.name)?;
    let id = sqlx::query("INSERT INTO groups (name, owner_id, invite_code, created_at) VALUES (?, ?, ?, ?)")
        .bind(&name)
        .bind(user.id)
        .bind(invite_code())
        .bind(now())
        .execute(&state.db)
        .await?
        .last_insert_rowid();
    add_member(&state, id, user.id).await?;
    sqlx::query("UPDATE users SET active_group_id = ? WHERE id = ?")
        .bind(id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    state.notify(Change::User(user.id));
    Ok(Json(json!({ "id": id })))
}

#[derive(Serialize)]
pub struct GroupDetail {
    #[serde(flatten)]
    group: Group,
    is_owner: bool,
}

pub async fn get_group(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<GroupDetail>> {
    let group = require_member(&state, id, user.id).await?;
    let is_owner = group.owner_id == user.id;
    Ok(Json(GroupDetail { group, is_owner }))
}

#[derive(Deserialize)]
pub struct UpdateGroup {
    name: Option<String>,
    min_ready: Option<i64>,
    press_ttl_minutes: Option<i64>,
    event_ttl_minutes: Option<i64>,
    is_open: Option<bool>,
}

pub async fn update_group(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<UpdateGroup>,
) -> AppResult<Json<GroupDetail>> {
    let mut g = require_owner(&state, id, user.id).await?;
    if let Some(name) = body.name {
        g.name = validate_name(&name)?;
    }
    if let Some(v) = body.min_ready {
        if !(1..=100).contains(&v) {
            return Err(AppError::bad("Minimum ready must be between 1 and 100"));
        }
        g.min_ready = v;
    }
    if let Some(v) = body.press_ttl_minutes {
        if !(1..=720).contains(&v) {
            return Err(AppError::bad("Ready duration must be between 1 and 720 minutes"));
        }
        g.press_ttl_minutes = v;
    }
    if let Some(v) = body.event_ttl_minutes {
        if !(1..=720).contains(&v) {
            return Err(AppError::bad("Break duration must be between 1 and 720 minutes"));
        }
        g.event_ttl_minutes = v;
    }
    if let Some(v) = body.is_open {
        g.is_open = v;
    }
    sqlx::query(
        "UPDATE groups SET name = ?, min_ready = ?, press_ttl_minutes = ?, event_ttl_minutes = ?, is_open = ?
         WHERE id = ?",
    )
    .bind(&g.name)
    .bind(g.min_ready)
    .bind(g.press_ttl_minutes)
    .bind(g.event_ttl_minutes)
    .bind(g.is_open)
    .bind(id)
    .execute(&state.db)
    .await?;

    // Lowering the threshold may mean enough people are already ready.
    coffee::check_threshold(&state, id, None).await?;
    state.notify(Change::Group(id));
    Ok(Json(GroupDetail { group: g, is_owner: true }))
}

async fn member_ids(state: &AppState, group_id: i64) -> AppResult<Vec<i64>> {
    Ok(sqlx::query_scalar("SELECT user_id FROM memberships WHERE group_id = ?")
        .bind(group_id)
        .fetch_all(&state.db)
        .await?)
}

pub async fn delete_group(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    require_owner(&state, id, user.id).await?;
    let members = member_ids(&state, id).await?;
    sqlx::query("DELETE FROM groups WHERE id = ?").bind(id).execute(&state.db).await?;
    for uid in members {
        state.notify(Change::User(uid));
    }
    Ok(Json(json!({ "ok": true })))
}

pub async fn regenerate_invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    require_owner(&state, id, user.id).await?;
    let code = invite_code();
    sqlx::query("UPDATE groups SET invite_code = ? WHERE id = ?")
        .bind(&code)
        .bind(id)
        .execute(&state.db)
        .await?;
    state.notify(Change::Group(id));
    Ok(Json(json!({ "invite_code": code })))
}

// ---------------------------------------------------------------------------
// Members & requests
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow, Serialize)]
pub struct Member {
    user_id: i64,
    initials: String,
    joined_at: i64,
    is_owner: bool,
}

pub async fn list_members(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Vec<Member>>> {
    require_member(&state, id, user.id).await?;
    let members: Vec<Member> = sqlx::query_as(
        "SELECT u.id AS user_id, u.initials, m.joined_at, g.owner_id = u.id AS is_owner
         FROM memberships m JOIN users u ON u.id = m.user_id JOIN groups g ON g.id = m.group_id
         WHERE m.group_id = ? ORDER BY u.initials",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(members))
}

/// Owner removes a member, or a member leaves the group themselves.
pub async fn remove_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, uid)): Path<(i64, i64)>,
) -> AppResult<Json<Value>> {
    let group = require_member(&state, id, user.id).await?;
    if uid == group.owner_id {
        return Err(AppError::bad("The owner can't leave the group. Delete it instead."));
    }
    if uid != user.id && group.owner_id != user.id {
        return Err(AppError::forbidden("Only the group owner can remove members"));
    }
    {
        let _guard = state.coffee_lock.lock().await;
        let mut tx = state.db.begin().await?;
        sqlx::query("DELETE FROM memberships WHERE group_id = ? AND user_id = ?")
            .bind(id)
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM presses WHERE group_id = ? AND user_id = ?")
            .bind(id)
            .bind(uid)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE users SET active_group_id = NULL WHERE id = ? AND active_group_id = ?")
            .bind(uid)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    state.notify(Change::Group(id));
    state.notify(Change::User(uid));
    Ok(Json(json!({ "ok": true })))
}

#[derive(sqlx::FromRow, Serialize)]
pub struct JoinRequest {
    user_id: i64,
    initials: String,
    created_at: i64,
}

pub async fn list_requests(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<Vec<JoinRequest>>> {
    require_owner(&state, id, user.id).await?;
    let reqs: Vec<JoinRequest> = sqlx::query_as(
        "SELECT u.id AS user_id, u.initials, r.created_at
         FROM join_requests r JOIN users u ON u.id = r.user_id
         WHERE r.group_id = ? ORDER BY r.created_at",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(reqs))
}

async fn take_request(state: &AppState, group_id: i64, user_id: i64) -> AppResult<()> {
    let res = sqlx::query("DELETE FROM join_requests WHERE group_id = ? AND user_id = ?")
        .bind(group_id)
        .bind(user_id)
        .execute(&state.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::not_found("No such join request"));
    }
    Ok(())
}

pub async fn approve_request(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, uid)): Path<(i64, i64)>,
) -> AppResult<Json<Value>> {
    require_owner(&state, id, user.id).await?;
    take_request(&state, id, uid).await?;
    add_member(&state, id, uid).await?;
    state.notify(Change::Group(id));
    state.notify(Change::User(uid));
    Ok(Json(json!({ "ok": true })))
}

pub async fn reject_request(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, uid)): Path<(i64, i64)>,
) -> AppResult<Json<Value>> {
    require_owner(&state, id, user.id).await?;
    take_request(&state, id, uid).await?;
    state.notify(Change::Group(id));
    state.notify(Change::User(uid));
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Invites
// ---------------------------------------------------------------------------

async fn group_by_code(state: &AppState, code: &str) -> AppResult<Group> {
    let g: Option<Group> = sqlx::query_as(&format!("SELECT {GROUP_COLS} FROM groups g WHERE g.invite_code = ?"))
        .bind(code.trim().to_lowercase())
        .fetch_optional(&state.db)
        .await?;
    g.ok_or_else(|| AppError::not_found("This invite link is invalid or has been replaced"))
}

async fn membership_status(state: &AppState, group_id: i64, user_id: i64) -> AppResult<&'static str> {
    let (member, pending): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM memberships WHERE group_id = ?1 AND user_id = ?2),
                EXISTS(SELECT 1 FROM join_requests WHERE group_id = ?1 AND user_id = ?2)",
    )
    .bind(group_id)
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    Ok(if member {
        "member"
    } else if pending {
        "pending"
    } else {
        "none"
    })
}

pub async fn invite_info(
    State(state): State<AppState>,
    user: AuthUser,
    Path(code): Path<String>,
) -> AppResult<Json<Value>> {
    let g = group_by_code(&state, &code).await?;
    let status = membership_status(&state, g.id, user.id).await?;
    let member_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memberships WHERE group_id = ?")
        .bind(g.id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(json!({
        "group_id": g.id,
        "name": g.name,
        "is_open": g.is_open,
        "member_count": member_count,
        "status": status,
    })))
}

pub async fn join_by_invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path(code): Path<String>,
) -> AppResult<Json<Value>> {
    let g = group_by_code(&state, &code).await?;
    let status = match membership_status(&state, g.id, user.id).await? {
        "member" => "member",
        _ if g.is_open => {
            add_member(&state, g.id, user.id).await?;
            sqlx::query("UPDATE users SET active_group_id = ? WHERE id = ?")
                .bind(g.id)
                .bind(user.id)
                .execute(&state.db)
                .await?;
            "member"
        }
        _ => {
            sqlx::query("INSERT OR IGNORE INTO join_requests (group_id, user_id, created_at) VALUES (?, ?, ?)")
                .bind(g.id)
                .bind(user.id)
                .bind(now())
                .execute(&state.db)
                .await?;
            "pending"
        }
    };
    state.notify(Change::Group(g.id));
    state.notify(Change::User(user.id));
    Ok(Json(json!({ "group_id": g.id, "status": status })))
}

pub async fn cancel_request(
    State(state): State<AppState>,
    user: AuthUser,
    Path(code): Path<String>,
) -> AppResult<Json<Value>> {
    let g = group_by_code(&state, &code).await?;
    sqlx::query("DELETE FROM join_requests WHERE group_id = ? AND user_id = ?")
        .bind(g.id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    state.notify(Change::Group(g.id));
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Active group & notification preferences
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SetActive {
    group_id: Option<i64>,
}

pub async fn set_active_group(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<SetActive>,
) -> AppResult<Json<Value>> {
    if let Some(gid) = body.group_id {
        require_member(&state, gid, user.id).await?;
    }
    sqlx::query("UPDATE users SET active_group_id = ? WHERE id = ?")
        .bind(body.group_id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    state.notify(Change::User(user.id));
    Ok(Json(json!({ "active_group_id": body.group_id })))
}

#[derive(sqlx::FromRow, Serialize, Deserialize)]
pub struct NotificationPrefs {
    notify_press: bool,
    notify_event_any: bool,
    notify_event_mine: bool,
}

pub async fn get_notifications(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> AppResult<Json<NotificationPrefs>> {
    let prefs: Option<NotificationPrefs> = sqlx::query_as(
        "SELECT notify_press, notify_event_any, notify_event_mine FROM memberships
         WHERE group_id = ? AND user_id = ?",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&state.db)
    .await?;
    prefs.map(Json).ok_or_else(|| AppError::not_found("Group not found or you're not a member"))
}

pub async fn put_notifications(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<NotificationPrefs>,
) -> AppResult<Json<NotificationPrefs>> {
    let res = sqlx::query(
        "UPDATE memberships SET notify_press = ?, notify_event_any = ?, notify_event_mine = ?
         WHERE group_id = ? AND user_id = ?",
    )
    .bind(body.notify_press)
    .bind(body.notify_event_any)
    .bind(body.notify_event_mine)
    .bind(id)
    .bind(user.id)
    .execute(&state.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::not_found("Group not found or you're not a member"));
    }
    Ok(Json(body))
}
