#!/usr/bin/env bash
set -euo pipefail
umask 077

image=${1:?usage: container-smoke.sh IMAGE [EXPECTED_ARCH]}
actual_arch=$(docker image inspect "$image" --format '{{.Architecture}}')
expected_arch=${2:-$actual_arch}
if [[ $actual_arch != "$expected_arch" ]]; then
    echo "Expected $expected_arch image, got $actual_arch" >&2
    exit 1
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/ha-utilities-smoke.XXXXXX")
container="ha-utilities-smoke-${RANDOM}-${RANDOM}"
volume="$container-data"
restore_volume="$container-restored"
outside_volume="$container-outside"
boundary_container="$container-boundary"
root_link_container="$container-root-link"
# Deliberately fictional and confined to disposable local containers.
token='fictional-container-smoke-token-not-a-provider-credential'

cleanup() {
    status=$?
    if [[ $status -ne 0 ]] && docker inspect "$container" >/dev/null 2>&1; then
        docker logs "$container" >&2 || true
    fi
    for resource in "$container" "$boundary_container" "$root_link_container"; do
        docker rm --force "$resource" >/dev/null 2>&1 || true
    done
    for resource in "$volume" "$restore_volume" "$outside_volume"; do
        docker volume rm "$resource" >/dev/null 2>&1 || true
    done
    rm -rf "$scratch"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Seed through the real Store producer and close SQLite before copying it.
cargo run --locked --example seed -- "$scratch/utilities.sqlite3" config/example.toml
cp config/example.toml "$scratch/config.toml"
printf '%s\n' "$token" >"$scratch/api-token"
printf '%s\n' '{"config_file":"/data/config.toml"}' >"$scratch/options.json"
printf 'fictional-dormant-provider-credential\n' >"$scratch/provider-secret"
chmod 600 "$scratch/utilities.sqlite3" "$scratch/config.toml" \
    "$scratch/api-token" "$scratch/options.json" "$scratch/provider-secret"

docker volume create "$volume" >/dev/null
docker create --name "$container" \
    --cap-drop FOWNER \
    --publish 127.0.0.1::8080 \
    --mount "type=volume,source=$volume,target=/data" \
    "$image" >/dev/null
for file in utilities.sqlite3 config.toml api-token options.json provider-secret; do
    docker cp "$scratch/$file" "$container:/data/$file"
done
# Bootstrap with application ownership; the restore below is deliberately root-owned.
docker run --rm --mount "type=volume,source=$volume,target=/data" \
    --entrypoint chown "$image" -R 10001:10001 /data
base_url=''

wait_for_api() {
    for _ in {1..60}; do
        if curl --fail --silent --max-time 1 \
            --header "Authorization: Bearer $token" \
            "$base_url/v1/system" --output "$scratch/system.json"; then
            return
        fi
        if [[ $(docker inspect "$container" --format '{{.State.Running}}') != true ]]; then
            break
        fi
        sleep 0.25
    done
    echo 'Container API did not become ready' >&2
    return 1
}

start_container() {
    docker start "$container" >/dev/null
    local mapping
    for _ in {1..60}; do
        if [[ $(docker inspect "$container" --format '{{.State.Running}}') != true ]]; then
            echo 'Container exited before its API became ready' >&2
            return 1
        fi
        mapping=$(docker port "$container" 8080/tcp)
        if [[ -n "$mapping" ]]; then
            base_url="http://$mapping"
            wait_for_api
            return
        fi
        sleep 0.25
    done
    echo 'Container API port was not published' >&2
    return 1
}

request() {
    local path=$1 actual
    actual=$(curl --silent --show-error --max-time 5 \
        --output "$scratch/response.json" --write-out '%{http_code}' \
        --header "Authorization: Bearer $token" "$base_url$path")
    if [[ $actual != 200 ]]; then
        echo "$path: expected HTTP 200, got $actual" >&2
        return 1
    fi
}

check_private_files() {
    local permissions
    permissions=$(docker exec "$container" stat -c '%a:%u:%g' \
        /data/api-token /data/config.toml /data/provider-secret /data/options.json)
    if [[ $permissions != $'600:10001:10001\n600:10001:10001\n600:10001:10001\n600:10001:10001' ]]; then
        echo 'Private runtime files lack owner-only modes or application ownership' >&2
        return 1
    fi
    for file in api-token config.toml provider-secret options.json; do
        docker cp "$container:/data/$file" "$scratch/restarted-$file"
        if ! cmp -s "$scratch/$file" "$scratch/restarted-$file"; then
            echo 'Private runtime file bytes changed during startup or restore' >&2
            return 1
        fi
    done
}

check_state() {
    local uid
    uid=$(docker exec "$container" cat /proc/1/status | awk '$1 == "Uid:" { print $2 }')
    if [[ $uid != 10001 ]]; then
        echo "Expected server uid10001, got $uid" >&2
        return 1
    fi
    jq --exit-status '.name == "Utilities" and .read_only_providers == true' \
        "$scratch/system.json" >/dev/null
    request /v1/summary
    jq --exit-status '.accounts == 2 and .bills == 70 and .collections == 5 and .usage_intervals == 72' \
        "$scratch/response.json" >/dev/null
    request '/v1/bills/demo-wm:bill:demo-000'
    jq --exit-status '.id == "demo-wm:bill:demo-000" and .account_id == "demo-wm" and
        .total.minor_units == 8742 and .total.currency == "USD" and .source_status == null' \
        "$scratch/response.json" >/dev/null
    request '/v1/usage?service=demo-electric&limit=1'
    jq --exit-status '.items | length == 1 and .[0].account_id == "demo-energy" and
        .[0].service_id == "demo-electric" and .[0].quantity == "350" and .[0].unit == "Wh"' \
        "$scratch/response.json" >/dev/null
    check_private_files
}

stop_container() {
    local exit_code
    docker stop --time 10 "$container" >/dev/null
    exit_code=$(docker inspect "$container" --format '{{.State.ExitCode}}')
    if [[ $exit_code != 0 ]]; then
        echo "Server did not stop cleanly: exit $exit_code" >&2
        return 1
    fi
}

start_container
check_state
stop_container
start_container
check_state
stop_container
echo 'Container startup/restart passed; restoring root-owned private data.'

# Native Supervisor restore can turn app-owned 0600 files into root-owned 0644
# files. Reproduce that observed state across the full private data tree.
docker volume create "$outside_volume" >/dev/null
docker run --rm --mount "type=volume,source=$volume,target=/data" \
    --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c '
        umask 077
        printf "outside-private-data\n" > /outside/sentinel
        mkdir /outside/private /data/owned-nested
        chmod 640 /outside/sentinel
        chmod 750 /outside /outside/private
        printf "preserved-private-state\n" > /data/owned-nested/marker
        chown -R 10001:10001 /data/owned-nested
        ln -s /outside /data/external-directory
        ln -s /outside/sentinel /data/external-file
    '
mkdir "$scratch/app-data"
docker cp "$container:/data/." "$scratch/app-data"
docker rm "$container" >/dev/null
docker volume create "$restore_volume" >/dev/null
docker create --name "$container" \
    --cap-drop FOWNER \
    --publish 127.0.0.1::8080 \
    --mount "type=volume,source=$restore_volume,target=/data" \
    --mount "type=volume,source=$outside_volume,target=/outside" \
    "$image" >/dev/null
docker cp "$scratch/app-data/." "$container:/data"
docker run --rm --mount "type=volume,source=$restore_volume,target=/data" \
    --entrypoint sh "$image" -c '
        find -P /data -xdev -type f -exec chmod 644 -- {} +
        find -P /data -xdev -type d -exec chmod 755 -- {} +
        test "$(stat -c "%a:%u:%g" /data/api-token)" = 644:0:0
        test "$(stat -c "%a:%u:%g" /data/config.toml)" = 644:0:0
        test "$(stat -c "%a:%u:%g" /data/provider-secret)" = 644:0:0
        test "$(stat -c "%a:%u:%g" /data/owned-nested/marker)" = 644:0:0
        test -L /data/external-directory
        test -L /data/external-file
    '
start_container
check_state
ownership=$(docker exec "$container" stat -c '%a:%u:%g' \
    /data/utilities.sqlite3 /data/owned-nested/marker /data /data/owned-nested)
if [[ $ownership != $'600:10001:10001\n600:10001:10001\n700:10001:10001\n700:10001:10001' ]]; then
    echo 'Restored data was not normalized to private modes and application ownership' >&2
    exit 1
fi
outside=$(docker exec "$container" stat -c '%a:%u:%g' /outside/sentinel /outside/private)
if [[ $outside != $'640:0:0\n750:0:0' ]]; then
    echo 'Startup followed a private-data symlink outside the owned volume' >&2
    exit 1
fi
docker exec "$container" sh -c 'test "$(cat /outside/sentinel)" = outside-private-data'
stop_container
start_container
check_state
stop_container

expect_startup_refusal() {
    local name=$1 reason=$2 running=true logs
    docker start "$name" >/dev/null
    for _ in {1..40}; do
        running=$(docker inspect "$name" --format '{{.State.Running}}')
        if [[ $running == false ]]; then break; fi
        sleep 0.25
    done
    if [[ $running != false || $(docker inspect "$name" --format '{{.State.ExitCode}}') == 0 ]]; then
        echo 'App accepted a private-data ownership boundary violation' >&2
        return 1
    fi
    logs=$(docker logs "$name" 2>&1)
    if [[ $logs != *"$reason"* ]]; then
        echo 'App exited without rejecting the expected private-data boundary' >&2
        return 1
    fi
}

# A nested volume must be rejected before repair touches either mount.
docker run --rm --mount "type=volume,source=$restore_volume,target=/data" \
    --entrypoint sh "$image" -c '
        chown 0:0 /data/owned-nested/marker
        chmod 644 /data/owned-nested/marker
        chmod 755 /data/owned-nested
    '
docker create --name "$boundary_container" \
    --cap-drop FOWNER \
    --mount "type=volume,source=$restore_volume,target=/data" \
    --mount "type=volume,source=$outside_volume,target=/data/external-mount" \
    "$image" >/dev/null
expect_startup_refusal "$boundary_container" 'contains nested mounts'
docker run --rm --mount "type=volume,source=$restore_volume,target=/data" \
    --entrypoint sh "$image" -c '
        test "$(stat -c "%a:%u:%g" /data/owned-nested/marker)" = 644:0:0
        test "$(stat -c "%a" /data/owned-nested)" = 755
    '
docker create --name "$root_link_container" \
    --cap-drop FOWNER \
    --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c \
    'rmdir /data; ln -s /outside /data; exec /usr/local/bin/docker-entrypoint.sh' >/dev/null
expect_startup_refusal "$root_link_container" 'Private data must be a real directory'
docker run --rm --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c '
        test "$(stat -c "%a:%u:%g" /outside/sentinel)" = 640:0:0
        test "$(stat -c "%a:%u:%g" /outside/private)" = 750:0:0
        test "$(stat -c "%a:%u:%g" /outside)" = 750:0:0
    '

# Outside aliases can share an inode without crossing a path or mount.
docker rm "$boundary_container" "$root_link_container" >/dev/null
docker run --rm --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint ln "$image" /outside/sentinel /outside/private/outside-alias
docker create --name "$boundary_container" \
    --cap-drop FOWNER \
    --mount "type=volume,source=$outside_volume,target=/data,volume-subpath=private" \
    "$image" >/dev/null
expect_startup_refusal "$boundary_container" 'outside hard links'
docker run --rm --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c '
        test "$(stat -c "%a:%u:%g" /outside/sentinel)" = 640:0:0
        test "$(stat -c "%a:%u:%g" /outside/private)" = 750:0:0
        test "$(cat /outside/sentinel)" = outside-private-data
        rm /outside/private/outside-alias
        mkfifo /outside/private/unsupported
    '
docker rm "$boundary_container" >/dev/null
docker create --name "$boundary_container" \
    --cap-drop FOWNER \
    --mount "type=volume,source=$outside_volume,target=/data,volume-subpath=private" \
    "$image" >/dev/null
expect_startup_refusal "$boundary_container" 'unsupported entries'
docker run --rm --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c '
        test "$(stat -c "%a:%u:%g" /outside/private)" = 750:0:0
        test "$(stat -c "%u:%g" /outside/private/unsupported)" = 0:0
        rm /outside/private/unsupported
        printf "internal-private-state\n" > /outside/private/internal-one
        chmod 644 /outside/private/internal-one
        ln /outside/private/internal-one /outside/private/internal-two
    '
# A crash during atomic publication can leave both legitimate private aliases.
docker run --rm --name "$root_link_container" \
    --cap-drop FOWNER \
    --mount "type=volume,source=$outside_volume,target=/data,volume-subpath=private" \
    "$image" --help >/dev/null
docker run --rm --mount "type=volume,source=$outside_volume,target=/outside" \
    --entrypoint sh "$image" -c '
        test "$(stat -c "%a:%u:%g:%h" /outside/private/internal-one)" = 600:10001:10001:2
        test "$(stat -c "%a:%u:%g:%h" /outside/private/internal-two)" = 600:10001:10001:2
        test "$(cat /outside/private/internal-one)" = internal-private-state
        test "$(stat -c "%a:%u:%g" /outside/sentinel)" = 640:0:0
    '

echo "Container smoke passed ($expected_arch): no-FOWNER restore/restart, UID 10001, root-owned 0644 restore, files 0600/directories 0700, unchanged credentials/data, symlink/mount/outside-hardlink refusal, internal hardlinks, and durable bills/collections/usage."
