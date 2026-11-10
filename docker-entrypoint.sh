#!/bin/sh
set -eu

# Native restores retain restrictive modes but copy private data as root.
# Repair only this volume, without following links or crossing another mount,
# before the server and worker drop to their fixed application user.
if [ "$(id -u)" -eq 0 ]; then
    if [ -L /data ] || [ ! -d /data ]; then
        echo 'Private data must be a real directory' >&2
        exit 1
    fi
    if ! awk '$5 ~ /^\/data\// { exit 1 }' /proc/self/mountinfo; then
        echo 'Private data mount boundary is unavailable or contains nested mounts' >&2
        exit 1
    fi
    find -P /data -xdev -exec chown --no-dereference utilities:utilities -- {} +
    exec gosu utilities "$0" "$@"
fi

umask 077
if [ -f /data/options.json ]; then
    set -- --options /data/options.json "$@"
fi
if [ "${UTILITIES_ENABLE_INGRESS:-false}" = true ]; then
    set -- --ingress-bind 0.0.0.0:8099 --ingress-peer 172.30.32.2 "$@"
fi
exec utilities "$@"
