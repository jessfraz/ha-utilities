#!/bin/sh
set -eu

if [ "$(id -u)" -eq 0 ]; then
    chown utilities:utilities /data
    if [ -f /data/options.json ]; then
        chown utilities:utilities /data/options.json
    fi
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
