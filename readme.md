# Kaffeknap ☕

A big red button for deciding when to go for a coffee break. Press it when you're ready. Once enough people in your group are ready, everyone who pressed gets a notification and the break starts.

- **Backend:** Rust (axum) + SQLite (sqlx, migrations run on startup)
- **Frontend:** plain HTML/CSS/JS in `static/`, no build step. It's installable as a PWA.
- **Live updates:** Server-Sent Events (`/api/stream`)
- **Background notifications:** Web Push (VAPID) via a service worker

## Running

```sh
cargo run --release
# → http://127.0.0.1:8080
```

| Env var                | Default                    | Purpose                                              |
|------------------------|----------------------------|------------------------------------------------------|
| `KAFFE_BIND`           | `127.0.0.1:8080`           | Listen address                                       |
| `KAFFE_DB`             | `kaffeknap.db`             | SQLite file (created if missing)                     |
| `KAFFE_STATIC`         | `static`                   | Frontend directory                                   |
| `KAFFE_SECURE_COOKIES` | `false`                    | Set `true` when served over HTTPS                    |
| `KAFFE_VAPID_SUBJECT`  | `mailto:admin@example.com` | Contact for push services. Set it to a real address. |
| `RUST_LOG`             | `info`                     | Log filter                                           |

**Push notifications require HTTPS** (`localhost` is exempt). In production, put the app behind a TLS reverse proxy (Caddy, nginx and so on) and set `KAFFE_SECURE_COOKIES=true`. The proxy must not buffer `/api/stream`. On iPhone/iPad, push only works after "Add to Home Screen".

The VAPID key pair is generated on first start and stored in the `app_settings` table. If you delete it, every device has to re-enable notifications.

## Deploying (Debian/Ubuntu, e.g. a Proxmox container)

The files are in `deploy/`. Database migrations are compiled into the binary.

```sh
apt update && apt install -y git curl build-essential pkg-config libssl-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && . ~/.cargo/env
git clone <your-repo-url> /root/kaffeknap
/root/kaffeknap/deploy/update.sh   # builds, installs to /opt/kaffeknap and starts the systemd service
```

Edit `/etc/systemd/system/kaffeknap.service` (at least `KAFFE_VAPID_SUBJECT`), then run `systemctl daemon-reload && systemctl restart kaffeknap`. To update later, run `deploy/update.sh` again. Data is stored in `/var/lib/kaffeknap/`.

For HTTPS, which push notifications need: `apt install caddy`, copy `deploy/Caddyfile` to `/etc/caddy/Caddyfile` with your domain, and run `systemctl reload caddy`.

## How it works

- **Press:** you're marked ready for `press_ttl_minutes` (default 30). Pressing again extends that time. "Not ready anymore" retracts it.
- **Threshold:** when `min_ready` (default 3) valid presses exist, a **live break** starts. The pressers become participants and the presses are cleared.
- **Live break:** pressing during a break joins it (marked as a late join). Any participant can confirm it with "We had coffee", which saves it to history. "I'm out" leaves the break, and a break with nobody left is cancelled.
- **Expiry:** if nobody confirms within `event_ttl_minutes` (default 30), the break is deleted and **never saved to history**. A background sweeper runs every 5 seconds.
- Lowering `min_ready` can start a break straight away when enough people are already ready.

### Groups

- Sign up with your initials (2–6 letters) and a password. You can be in any number of groups, and you pick the active one from the header.
- The owner can configure the name, `min_ready`, both durations, and whether the group is **open** (anyone with the link joins) or uses **approval** (people with the link send a join request).
- Invite links look like `/join/<code>`. The owner can create a new code, and the old link then stops working.
- The owner can remove members or delete the group. Other members can leave.

### Notifications (per user, per group)

- A break I was ready for starts (on by default)
- Any break starts
- Someone presses the button

These go out as Web Push to every device where you enabled notifications. If the app is open and focused, no system notification is shown. The browser tab title shows the active group's status, e.g. `2/3 ready ✓ · Kontoret` or `☕ Coffee time! · Kontoret`, and the favicon shows the count.

## Security notes

- Passwords are hashed with **Argon2id** (19 MiB, t=2, p=1, the OWASP parameters) using a random salt, on a blocking thread. There are no password rules: any password, including an empty one, is accepted.
- A login with unknown initials still runs a hash check, so response timing doesn't reveal which accounts exist. After 5 failed logins on an account, it's throttled with exponential backoff (30 s up to 16 min).
- The session token is 256 random bits in an `HttpOnly`, `SameSite=Lax` cookie (`Secure` is configurable). Only its SHA-256 is stored, and sessions last 60 days. Changing your password revokes all sessions.
- CSRF: every non-GET API call must send `X-Requested-With: kaffeknap`. Cross-site pages can't send that header without a CORS preflight, which the server never grants.
- A strict CSP is set (no inline script or style), plus `X-Frame-Options: DENY`, `nosniff` and `Referrer-Policy`.

## Data for stats

All timestamps are unix seconds.

- `events`: completed breaks only (`status = 'completed'`). Columns: `started_by` (first to press), `triggered_by` (the press that reached the threshold, `NULL` if a settings change started it), `started_at`, `completed_at`, `completed_by`, and `min_ready` at the time.
- `event_participants`: `pressed_at` for each participant, and `late_join` if they joined after the break started.

History is available at `GET /api/groups/{id}/history?limit=&before=`.

## API overview

```
POST /api/auth/signup|login|logout|password    GET /api/me    PUT /api/me/active-group
GET|POST /api/groups                           GET|PATCH|DELETE /api/groups/{id}
POST /api/groups/{id}/invite-code              GET /api/groups/{id}/members
DELETE /api/groups/{id}/members/{uid}          GET /api/groups/{id}/requests
POST /api/groups/{id}/requests/{uid}/approve|reject
GET|PUT /api/groups/{id}/notifications
GET /api/invite/{code}   POST /api/invite/{code}/join   DELETE /api/invite/{code}/request
GET /api/groups/{id}/state   POST|DELETE /api/groups/{id}/ready
POST /api/groups/{id}/event/complete   GET /api/groups/{id}/history
GET /api/push/key   POST /api/push/subscribe|unsubscribe|test   GET /api/stream (SSE)
```
