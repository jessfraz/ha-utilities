#!/bin/sh
set -eu

# Native restores can copy app-owned files as root with permissive modes.
# Normalize this private volume before dropping privileges, without traversing
# links or touching a nested mount. Regular files are data, never executables.
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
    find -P /data -xdev -type d -exec chmod 0700 -- {} +
    find -P /data -xdev -type f -exec chmod 0600 -- {} +
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
