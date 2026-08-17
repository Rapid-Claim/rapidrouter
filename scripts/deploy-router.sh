#!/usr/bin/env bash
#
# Deploy rapid-router to the AGI host (router.rapidclaims.ai).
#
#   bash scripts/deploy-router.sh
#
# Mirrors rapid-mono's deploy-agi-stack.sh pattern: the box builds a
# static musl binary in docker (no toolchain rot on the host), installs
# it to /opt/rapid-router, and restarts the systemd unit. State lives in
# the S3 store (rapidclaims-router-store/prod), so the binary is
# stateless and a bad deploy is rolled back by reinstalling the previous
# binary — nothing on disk to migrate.
#
# One-time setup already on the box:
#   /etc/rapid-router/rapid-router.env   RAPID_STORE_* + RAPID_MASTER_KEY
#   /etc/rapid-router/seed.toml          console admin key (import to reseed)
#   /etc/systemd/system/rapid-router.service
#   nginx vhost router.rapidclaims.ai + certbot certificate
set -euo pipefail

HOST="${DEPLOY_HOST:-34.233.227.213}"
SSH_KEY="${DEPLOY_KEY:-$HOME/.ssh/ashutosh-server.pem}"
SSH=(ssh -i "$SSH_KEY" -o BatchMode=yes "ubuntu@${HOST}")

echo "==> Syncing source"
rsync -az -e "ssh -i $SSH_KEY" \
  --exclude target --exclude node_modules --exclude .git \
  --exclude .rapidrouter --exclude console/dist \
  ./ "ubuntu@${HOST}:rapid-router-src/"

echo "==> Building (console bundle, then static musl binary)"
"${SSH[@]}" bash -s << 'REMOTE'
set -euo pipefail
cd ~/rapid-router-src
docker run --rm -v "$PWD":/src -w /src/console node:22 bash -c "npm ci --no-audit --no-fund && npm run build"
docker run --rm -v "$PWD":/src -w /src rust:1 bash -c "
  apt-get update -qq && apt-get install -y -qq musl-tools >/dev/null &&
  rustup target add x86_64-unknown-linux-musl &&
  cargo build --release --target x86_64-unknown-linux-musl -p router-bin"
REMOTE

echo "==> Installing and restarting"
"${SSH[@]}" bash -s << 'REMOTE'
set -euo pipefail
sudo install -m 755 ~/rapid-router-src/target/x86_64-unknown-linux-musl/release/rapid-router /opt/rapid-router/rapid-router
sudo systemctl restart rapid-router
for i in $(seq 1 30); do
  curl -sf http://127.0.0.1:8091/health >/dev/null && break
  sleep 1
done
curl -sf http://127.0.0.1:8091/health
echo
REMOTE

echo "==> Public check"
curl -sf "https://router.rapidclaims.ai/health"
echo
echo "==> Deployed."
