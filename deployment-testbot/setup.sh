#!/usr/bin/env bash
# HOST-NATIVE SUT bring-up (no Docker/DinD).
# The workflow already compiled the self-contained binary (frontend embedded via
# rust-embed) with the release-ci profile + rust-cache. This script just starts it
# on the runner host at :5080, so the Skyramp executor (host networking) reaches it
# directly at localhost:5080 — the P0-fix hypothesis vs the prior docker-compose setup.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="target/x86_64-unknown-linux-gnu/release-ci/openobserve"
if [[ ! -x "$BIN" ]]; then
  # Fallback: some profiles/targets land elsewhere.
  BIN="$(find target -type f -name openobserve -perm -u+x 2>/dev/null | head -n1 || true)"
fi
[[ -n "${BIN:-}" && -f "$BIN" ]] || { echo "SUT binary not found under target/ — did the build step run?"; exit 1; }
chmod +x "$BIN"

echo "Starting OpenObserve natively: $BIN"
nohup "$BIN" > "${GITHUB_WORKSPACE:-$PWD}/o2.log" 2>&1 &
disown || true
echo "OpenObserve started (pid $!) on http://localhost:5080 — logs: o2.log"
# Readiness is enforced by the workflow's targetReadyCheckCommand (curl /web/login).
