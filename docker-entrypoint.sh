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
    private_entries=$(find -P /data -xdev -printf '%y %D:%i %n\n')
    if ! printf '%s\n' "$private_entries" | awk '
        NF != 3 || ($1 != "d" && $1 != "f" && $1 != "l") { invalid = 1 }
        $1 == "f" || $1 == "l" { seen[$2]++; links[$2] = $3 }
        END {
            if (invalid) exit 1
            for (inode in links) if (seen[inode] != links[inode]) exit 1
        }
    '; then
        echo 'Private data contains unsupported entries or outside hard links' >&2
        exit 1
    fi
    # Own metadata while repairing it. Root cannot chmod app-owned inodes
    # without CAP_FOWNER, including on an ordinary restart.
    find -P /data -xdev -exec chown --no-dereference 0:0 -- {} +
    find -P /data -xdev -type d -exec chmod 0700 -- {} +
    find -P /data -xdev -type f -exec chmod 0600 -- {} +
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
