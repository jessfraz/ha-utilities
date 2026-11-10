#!/usr/bin/env bash
set -euo pipefail

image=${1:?usage: container-smoke.sh IMAGE}
volume=$(docker volume create)
container=''
cleanup() {
    if [[ -n "$container" ]]; then docker rm -f "$container" >/dev/null; fi
    docker volume rm "$volume" >/dev/null
}
trap cleanup EXIT

container=$(docker run -d -v "$volume:/data" "$image")
for _ in $(seq 1 60); do
    if docker exec --user utilities "$container" ha-utilities --token-file /data/api-token system --json >/dev/null 2>&1; then break; fi
    sleep 1
done
docker exec --user utilities "$container" ha-utilities --token-file /data/api-token system --json | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["name"] == "Utilities" and d["read_only_providers"]'
docker exec --user utilities "$container" ha-utilities --token-file /data/api-token health --json | python3 -c 'import json,sys; assert json.load(sys.stdin)["status"] == "not_configured"'
docker exec "$container" sh -c 'test "$(stat -c %a /data/api-token)" = 600; test "$(stat -c %u /data/api-token)" = 10001'
docker stop "$container" >/dev/null
docker start "$container" >/dev/null
for _ in $(seq 1 30); do
    if docker exec --user utilities "$container" ha-utilities --token-file /data/api-token summary --json >/dev/null 2>&1; then break; fi
    sleep 1
done
docker exec --user utilities "$container" ha-utilities --token-file /data/api-token summary --json | python3 -c 'import json,sys; d=json.load(sys.stdin); assert d["accounts"] == 0 and d["stale"]'
