#!/usr/bin/env bash
# A throwaway PostgreSQL cluster for development and tests.
#
#   scripts/pg.sh start    create (first time) and start the cluster in .pgdata on port 54329
#   scripts/pg.sh stop     stop it
#   scripts/pg.sh url      print the connection url
#   scripts/pg.sh test     start it, run the storage tests and every example on SQLite and
#                          PostgreSQL (same hash chain on both), stop it
#   scripts/pg.sh reset    stop it and delete .pgdata
#
# It never touches a PostgreSQL service installed on the machine: its own data folder,
# its own port, its own socket directory, trust authentication on localhost only.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA="$ROOT/.pgdata"
PORT="${QUADRA_PG_PORT:-54329}"
USER_NAME="$(id -un)"
URL="postgres://$USER_NAME@localhost:$PORT/quadra"

find_bin() {
    for dir in "${PG_BIN:-}" /opt/homebrew/bin /usr/local/bin /usr/lib/postgresql/*/bin; do
        if [ -n "$dir" ] && [ -x "$dir/pg_ctl" ]; then
            echo "$dir"
            return
        fi
    done
    command -v pg_ctl >/dev/null && dirname "$(command -v pg_ctl)" && return
    echo "pg_ctl not found: install PostgreSQL or set PG_BIN" >&2
    exit 1
}
BIN="$(find_bin)"

running() {
    "$BIN/pg_ctl" -D "$DATA" status >/dev/null 2>&1
}

start() {
    if [ ! -f "$DATA/PG_VERSION" ]; then
        "$BIN/initdb" -D "$DATA" -U "$USER_NAME" --auth=trust --encoding=UTF8 --locale=C >/dev/null
        {
            echo "listen_addresses = 'localhost'"
            echo "port = $PORT"
            echo "unix_socket_directories = '$DATA'"
            echo "fsync = off"
            echo "synchronous_commit = off"
            echo "full_page_writes = off"
        } >>"$DATA/postgresql.conf"
    fi
    if ! running; then
        "$BIN/pg_ctl" -D "$DATA" -l "$DATA/server.log" -w start >/dev/null
    fi
    if ! "$BIN/psql" -h localhost -p "$PORT" -U "$USER_NAME" -d postgres -tAc "SELECT 1 FROM pg_database WHERE datname = 'quadra'" | grep -q 1; then
        "$BIN/createdb" -h localhost -p "$PORT" -U "$USER_NAME" quadra
    fi
    echo "$URL"
}

stop() {
    if running; then
        "$BIN/pg_ctl" -D "$DATA" -m fast -w stop >/dev/null
    fi
}

case "${1:-}" in
    start) start ;;
    stop) stop ;;
    url) echo "$URL" ;;
    reset)
        stop
        rm -rf "$DATA"
        ;;
    test)
        start >/dev/null
        trap stop EXIT
        QUADRA="${QUADRA:-$ROOT/target/debug/quadra}"
        status=0
        # same tests through cargo: SQLite and PostgreSQL must write the same hash chain
        (cd "$ROOT" && QUADRA_TEST_PG="$URL" cargo test -q -p quadra-store) || status=1
        for example in "$ROOT"/examples/*/; do
            for db in sqlite "$URL"; do
                echo "── $(basename "$example") on ${db%%:*}"
                "$QUADRA" test "$example" --db "$db" || status=1
            done
        done
        exit $status
        ;;
    *)
        sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'
        exit 2
        ;;
esac
