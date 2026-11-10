-- All timestamps are unix epoch seconds (UTC).

CREATE TABLE users (
    id              INTEGER PRIMARY KEY,
    initials        TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash   TEXT    NOT NULL,           -- Argon2id PHC string
    active_group_id INTEGER REFERENCES groups(id) ON DELETE SET NULL,
    created_at      INTEGER NOT NULL
);

-- Session tokens are random 256-bit values; only their SHA-256 is stored.
CREATE TABLE sessions (
    token_hash  TEXT    PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
);
CREATE INDEX sessions_user ON sessions(user_id);

CREATE TABLE groups (
    id                  INTEGER PRIMARY KEY,
    name                TEXT    NOT NULL,
    owner_id            INTEGER NOT NULL REFERENCES users(id),
    invite_code         TEXT    NOT NULL UNIQUE,
    min_ready           INTEGER NOT NULL DEFAULT 3,
    press_ttl_minutes   INTEGER NOT NULL DEFAULT 30,
    event_ttl_minutes   INTEGER NOT NULL DEFAULT 30,
    is_open             INTEGER NOT NULL DEFAULT 1,  -- 1: anyone with link joins; 0: owner approves
    created_at          INTEGER NOT NULL
);

CREATE TABLE memberships (
    group_id            INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id             INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    joined_at           INTEGER NOT NULL,
    notify_press        INTEGER NOT NULL DEFAULT 0,  -- someone else presses ready
    notify_event_any    INTEGER NOT NULL DEFAULT 0,  -- any coffee break starts
    notify_event_mine   INTEGER NOT NULL DEFAULT 1,  -- a break I was ready for starts
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX memberships_user ON memberships(user_id);

CREATE TABLE join_requests (
    group_id    INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (group_id, user_id)
);

-- Current "ready" indications. At most one per user per group.
CREATE TABLE presses (
    group_id    INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    pressed_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    PRIMARY KEY (group_id, user_id)
);

-- Coffee breaks. status: 'live' until a participant completes it ('completed').
-- Live events that expire without completion are deleted (never part of history).
CREATE TABLE events (
    id              INTEGER PRIMARY KEY,
    group_id        INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    started_by      INTEGER REFERENCES users(id) ON DELETE SET NULL, -- first person to press ready
    triggered_by    INTEGER REFERENCES users(id) ON DELETE SET NULL, -- press that reached the threshold
    started_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'live' CHECK (status IN ('live', 'completed')),
    completed_at    INTEGER,
    completed_by    INTEGER REFERENCES users(id) ON DELETE SET NULL,
    min_ready       INTEGER NOT NULL  -- threshold in effect when the event started
);
CREATE INDEX events_group_status ON events(group_id, status);

CREATE TABLE event_participants (
    event_id    INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    pressed_at  INTEGER NOT NULL,          -- when they indicated ready
    late_join   INTEGER NOT NULL DEFAULT 0, -- joined after the event had started
    PRIMARY KEY (event_id, user_id)
);

CREATE TABLE push_subscriptions (
    id          INTEGER PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    endpoint    TEXT    NOT NULL UNIQUE,
    p256dh      TEXT    NOT NULL,
    auth        TEXT    NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE TABLE app_settings (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);
