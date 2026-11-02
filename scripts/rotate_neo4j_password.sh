#!/usr/bin/env bash
# Move the live Neo4j off its old password to NEO4J_PASSWORD from .env
# (OUTSIDE_REVIEW_PLAN Phase 0). NEO4J_AUTH in docker-compose.yml applies only
# when the data volume is first created, so an existing database changes it
# here. Run it BEFORE deploying a compose that reads NEO4J_PASSWORD, then
# deploy straight away: until then the running brain's new connections fail.
# Safe to run twice. Never prints a password; passwords travel by environment
# and stdin, not argv.
#
#   scripts/rotate_neo4j_password.sh [old-password]   (default: password)
set -euo pipefail
cd "$(dirname "$0")/.."

export OLD_PW="${1:-password}"
NEW_PW=$(grep -E '^NEO4J_PASSWORD=' .env | tail -1 | cut -d= -f2- | sed -E "s/^(['\"])(.*)\1$/\2/")
export NEW_PW
[ -n "$NEW_PW" ] || { echo "NEO4J_PASSWORD is not set in .env" >&2; exit 1; }
[ "$NEW_PW" != "password" ] || { echo "NEO4J_PASSWORD in .env is still 'password'; choose a new one" >&2; exit 1; }
[ "${#NEW_PW}" -ge 8 ] || { echo "Neo4j needs a password of at least 8 characters" >&2; exit 1; }
case "$NEW_PW$OLD_PW" in *"'"* | *'\'*) echo "a password may not contain ' or \\" >&2; exit 1 ;; esac

# cypher-shell as neo4j with the password in the named variable.
shell() {
    NEO4J_PASSWORD="${!1}" docker compose exec -T -e NEO4J_PASSWORD -e NEO4J_USERNAME=neo4j \
        neo4j cypher-shell -d system --format plain
}

if echo 'SHOW CURRENT USER;' | shell NEW_PW >/dev/null 2>&1; then
    echo "Neo4j already uses NEO4J_PASSWORD from .env; nothing to change."
    exit 0
fi
if ! echo 'SHOW CURRENT USER;' | shell OLD_PW >/dev/null 2>&1; then
    echo "Neither the old password nor NEO4J_PASSWORD from .env logs in; pass the current one as the first argument." >&2
    exit 1
fi
printf "ALTER CURRENT USER SET PASSWORD FROM '%s' TO '%s';\n" "$OLD_PW" "$NEW_PW" | shell OLD_PW >/dev/null
echo 'SHOW CURRENT USER;' | shell NEW_PW >/dev/null
echo "Neo4j now uses NEO4J_PASSWORD from .env. Deploy now: docker compose up -d --build"
