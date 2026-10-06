#!/usr/bin/env bash
# Prints a fresh id_token for Dex's demo operator, for skilj-tui's
# `--token-command` (README.md, "Operator consoles"). skilj-tui runs it
# at startup and again whenever the token expires, so a session isn't
# capped at one token's lifetime.
#
# Uses Dex's password grant (dex/config.yaml, `oauth2.passwordConnector`)
# on the frontend's client id, so the token's audience is the one the
# server already checks. Defaults match dex/config.yaml; override with
# DEX_ISSUER, OPERATOR_USERNAME, OPERATOR_PASSWORD.
set -euo pipefail

issuer="${DEX_ISSUER:-http://127.0.0.1:5556/dex}"
response="$(curl -fsS -X POST "$issuer/token" \
    -d grant_type=password \
    -d client_id=skilj-helpdesk-frontend \
    -d scope=openid \
    --data-urlencode "username=${OPERATOR_USERNAME:-operator@acme.example}" \
    --data-urlencode "password=${OPERATOR_PASSWORD:-operator-demo-pw}")"
# No jq dependency: the id_token is a JWT, so it has no quotes in it.
token="$(printf '%s' "$response" | sed -n 's/.*"id_token":"\([^"]*\)".*/\1/p')"
if [ -z "$token" ]; then
    echo "operator-token.sh: Dex returned no id_token: $response" >&2
    exit 1
fi
printf '%s\n' "$token"
