#!/usr/bin/env bash
# Pull, build and (re)install Kaffeknap. Run as root from the repo checkout.
set -euo pipefail
cd "$(dirname "$0")/.."

git pull --ff-only
cargo build --release

install -d /opt/kaffeknap
install -m 0755 target/release/kaffeknap /opt/kaffeknap/kaffeknap.new
mv /opt/kaffeknap/kaffeknap.new /opt/kaffeknap/kaffeknap
rm -rf /opt/kaffeknap/static
cp -r static /opt/kaffeknap/static

id kaffeknap >/dev/null 2>&1 || useradd --system --home /var/lib/kaffeknap --shell /usr/sbin/nologin kaffeknap
install -m 0644 deploy/kaffeknap.service /etc/systemd/system/kaffeknap.service.dist
# Keep a locally edited unit file; only install the default the first time.
[ -f /etc/systemd/system/kaffeknap.service ] || cp /etc/systemd/system/kaffeknap.service.dist /etc/systemd/system/kaffeknap.service
systemctl daemon-reload
systemctl enable kaffeknap >/dev/null
systemctl restart kaffeknap
systemctl --no-pager --lines=5 status kaffeknap
