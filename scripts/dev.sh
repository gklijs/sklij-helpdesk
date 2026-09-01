#!/usr/bin/env bash
# Brings up the whole backend in one command: a throwaway local Postgres
# (unless DATABASE_URL is already set - your own, real database is left
# alone), Dex (if a built binary is found - see README.md's "Build Dex
# once" step for why this doesn't build it for you), then the server in
# the foreground. Ctrl+C tears everything this script started back down.
#
# This is exactly README.md's own "Running it" steps 1-2, in one
# command - not a replacement for understanding them, a shortcut once
# you do. Doesn't start `frontend/` (trunk serve) - that's a separate
# terminal, `cd frontend && trunk serve`, same as the README says.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PIDS=()
PG_DATA_DIR=""

cleanup() {
    echo
    echo "dev.sh: stopping..."
    # Every PID here, including the server's own (see the bottom of this
    # file), not just Dex - found the hard way: a plain foreground
    # `cargo run --bin server` as this script's last line left the
    # server (and everything above it) running after `kill -TERM` on
    # this script's own PID, because bash doesn't act on a trap until a
    # foreground external command returns, and `cargo run` here never
    # does on its own. Backgrounding it and `wait`-ing (below) is what
    # makes this trap actually interruptible, and this loop is what
    # actually stops the process `wait` was blocked on.
    for pid in "${PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    if [ -n "$PG_DATA_DIR" ] && command -v pg_ctl >/dev/null 2>&1; then
        pg_ctl -D "$PG_DATA_DIR" stop -m fast >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM

# --- Postgres: use DATABASE_URL if you've already set one, otherwise
#     start a throwaway instance for this run only ---
if [ -z "${DATABASE_URL:-}" ]; then
    if ! command -v pg_ctl >/dev/null 2>&1 || ! command -v initdb >/dev/null 2>&1; then
        echo "dev.sh: no DATABASE_URL set, and no local pg_ctl/initdb on PATH." >&2
        echo "        Either 'export DATABASE_URL=postgres://...' yourself, or install Postgres." >&2
        exit 1
    fi
    PG_DATA_DIR="$(mktemp -d)/pgdata"
    PG_PORT=55499
    echo "dev.sh: starting a throwaway local Postgres ($PG_DATA_DIR, port $PG_PORT)..."
    initdb -D "$PG_DATA_DIR" -U postgres --auth=trust -E UTF8 >/dev/null
    pg_ctl -D "$PG_DATA_DIR" -o "-p $PG_PORT -k /tmp" -l "$PG_DATA_DIR/log.txt" start >/dev/null
    createdb -h /tmp -p "$PG_PORT" -U postgres skilj_helpdesk_dev
    export DATABASE_URL="postgres:///skilj_helpdesk_dev?host=/tmp&port=${PG_PORT}&user=postgres"
fi

# --- Dex: only if a built binary is actually found - never built here,
#     see README.md's own "Build Dex once" step ---
DEX_BIN="${DEX_BIN:-}"
if [ -z "$DEX_BIN" ]; then
    if command -v dex >/dev/null 2>&1; then
        DEX_BIN="$(command -v dex)"
    elif [ -x "$ROOT/dex/dex" ]; then
        DEX_BIN="$ROOT/dex/dex"
    fi
fi
if [ -n "$DEX_BIN" ]; then
    echo "dev.sh: starting Dex ($DEX_BIN)..."
    "$DEX_BIN" serve dex/config.yaml &
    PIDS+=("$!")
    export OIDC_ISSUER_URL="http://127.0.0.1:5556/dex"
    sleep 1
else
    echo "dev.sh: no Dex binary found (checked PATH and ./dex/dex) - GraphQL/frontend login won't work."
    echo "        See README.md's 'Build Dex once' step. Continuing without it."
fi

echo "dev.sh: starting the server..."
echo
# Backgrounded, then `wait`ed on - not run directly in the foreground.
# `wait` (unlike a synchronous foreground command) returns as soon as a
# trapped signal arrives, which is what makes Ctrl+C/`kill -TERM` on
# this script actually interrupt promptly instead of the trap only
# firing once `cargo run` exits on its own. Its own PID goes in `PIDS`
# so `cleanup` above actually stops it too.
cargo run --bin server &
PIDS+=("$!")
wait "$!"
