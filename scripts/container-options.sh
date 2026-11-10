#!/usr/bin/env bash
set -euo pipefail
umask 077

image=${1:?usage: container-options.sh IMAGE}
scratch=$(mktemp -d "${TMPDIR:-/tmp}/ha-utilities-options.XXXXXX")
container="ha-utilities-options-${RANDOM}-${RANDOM}"
volume="$container-data"
cleanup() {
    status=$?
    docker rm --force "$container" >/dev/null 2>&1 || true
    docker volume rm "$volume" >/dev/null 2>&1 || true
    rm -rf "$scratch"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# This is the single file Supervisor writes through its normal options API.
# No configuration-file or credential-file copy is needed by the app.
python3 - "$scratch" <<'PY'
import json, pathlib, sys, tomllib
directory = pathlib.Path(sys.argv[1])
example = tomllib.loads(pathlib.Path('config/example.toml').read_text())
settings = {
    'deployment': example['deployment'], 'accounts': example['accounts'],
    'wm_token': 'fictional-options-wm-token',
    'wm_client_id': 'fictional-options-wm-client',
    'bayou_api_key': 'fictional-options-bayou-key',
}
(directory / 'options.json').write_text(json.dumps(settings))
for filename, field in [('wm-token', 'wm_token'), ('wm-client-id', 'wm_client_id'), ('bayou-api-key', 'bayou_api_key')]:
    (directory / filename).write_text(settings[field])
settings['accounts'][0]['deployment'] = 'different-deployment'
(directory / 'invalid.json').write_text(json.dumps(settings))
settings['accounts'][0]['deployment'] = example['deployment']
settings['accounts'][1]['provider_account'] = 'DIFFERENT-FICTIONAL-ACCOUNT'
(directory / 'changed-binding.json').write_text(json.dumps(settings))
PY
chmod 600 "$scratch"/*
docker volume create "$volume" >/dev/null
docker create --name "$container" --network none \
    --mount "type=volume,source=$volume,target=/data" "$image" >/dev/null
docker cp "$scratch/options.json" "$container:/data/options.json"

wait_for_api() {
    for _ in {1..80}; do
        if docker exec --user utilities "$container" ha-utilities \
            --token-file /data/api-token system --json >"$scratch/system.json" 2>/dev/null; then return; fi
        if [[ $(docker inspect "$container" --format '{{.State.Running}}') != true ]]; then break; fi
        sleep 0.25
    done
    echo 'Supervisor options startup did not become ready' >&2
    return 1
}
stop_cleanly() {
    docker stop --time 10 "$container" >/dev/null
    if [[ $(docker inspect "$container" --format '{{.State.ExitCode}}') != 0 ]]; then
        echo 'Options-configured server did not stop cleanly' >&2
        return 1
    fi
}
check_state() {
    local uid modes
    uid=$(docker exec "$container" awk '$1 == "Uid:" {print $2}' /proc/1/status)
    [[ $uid == 10001 ]]
    docker exec "$container" test ! -e /data/config.toml
    docker exec --user utilities "$container" ha-utilities --token-file /data/api-token providers list --json >"$scratch/providers.json"
    jq --exit-status '.items | length == 2 and all(.configured == true)' "$scratch/providers.json" >/dev/null
    docker exec --user utilities "$container" ha-utilities --token-file /data/api-token accounts list --json >"$scratch/accounts.json"
    jq --exit-status '.items | map(.id) | sort == ["demo-energy","demo-wm"]' "$scratch/accounts.json" >/dev/null
    modes=$(docker exec "$container" stat -c '%a:%u:%g' \
        /data/credentials /data/credentials/wm-token /data/credentials/wm-client-id /data/credentials/bayou-api-key /data/options.json)
    [[ $modes == $'700:10001:10001\n600:10001:10001\n600:10001:10001\n600:10001:10001\n600:10001:10001' ]]
    for file in wm-token wm-client-id bayou-api-key; do
        docker cp "$container:/data/credentials/$file" "$scratch/actual-$file"
        cmp -s "$scratch/$file" "$scratch/actual-$file"
    done
    docker logs "$container" >"$scratch/service.log" 2>&1
    python3 - "$scratch" <<'PY'
import pathlib, sys
directory = pathlib.Path(sys.argv[1])
public = ''.join((directory / name).read_text() for name in ['accounts.json', 'providers.json', 'system.json', 'service.log'])
for filename in ['wm-token', 'wm-client-id', 'bayou-api-key']:
    assert (directory / filename).read_text() not in public, 'Credential appeared in public output'
PY
}
expect_rejection() {
    local file=$1 expected=$2 running=true
    docker cp "$scratch/$file" "$container:/data/options.json"
    docker start "$container" >/dev/null
    for _ in {1..80}; do
        running=$(docker inspect "$container" --format '{{.State.Running}}')
        if [[ $running == false ]]; then break; fi
        sleep 0.25
    done
    if [[ $running != false || $(docker inspect "$container" --format '{{.State.ExitCode}}') == 0 ]]; then
        echo 'Invalid Supervisor bindings started the app' >&2
        return 1
    fi
    docker logs "$container" >"$scratch/service.log" 2>&1
    python3 - "$scratch/service.log" "$expected" <<'PY_CHECK'
import pathlib, sys
assert sys.argv[2] in pathlib.Path(sys.argv[1]).read_text(), 'Unexpected startup rejection'
PY_CHECK
}

docker start "$container" >/dev/null
wait_for_api
check_state
docker cp "$container:/data/api-token" "$scratch/original-api-token"
stop_cleanly
docker start "$container" >/dev/null
wait_for_api
check_state
docker cp "$container:/data/api-token" "$scratch/restarted-api-token"
cmp -s "$scratch/original-api-token" "$scratch/restarted-api-token"
stop_cleanly

expect_rejection invalid.json 'account deployment does not match'
expect_rejection changed-binding.json 'account binding changed'
docker cp "$scratch/options.json" "$container:/data/options.json"
docker start "$container" >/dev/null
wait_for_api
check_state
stop_cleanly
echo 'Container options passed: Supervisor options only, explicit WM/Bayou bindings, private credentials, UID10001, no provider network, restart continuity, and rejected binding changes.'
