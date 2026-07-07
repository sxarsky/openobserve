#!/usr/bin/env bash
# Print the auth token for the SUT. OpenObserve uses HTTP Basic auth on /api
# endpoints, so the "token" is base64("<email>:<password>"); workspace.yml sets
# authType: basic / authScheme: Basic so the executor sends
#   Authorization: Basic <token>
# Default root credentials come from the workflow env (mirrors upstream):
#   ZO_ROOT_USER_EMAIL    = root@example.com
#   ZO_ROOT_USER_PASSWORD = Complexpass#123
set -euo pipefail
USER="${ZO_ROOT_USER_EMAIL:-root@example.com}"
PASS="${ZO_ROOT_USER_PASSWORD:-Complexpass#123}"
printf '%s:%s' "$USER" "$PASS" | base64 | tr -d '\n'
