#!/bin/sh
# Explicitly consented VM-wide changes. Never lower a shared limit.
set -eu
[ "$(id -u)" -eq 0 ] || { echo 'Raising SonarQube Linux VM limits requires root.' >&2; exit 1; }
map=$(cat /proc/sys/vm/max_map_count)
files=$(cat /proc/sys/fs/file-max)
case "$map:$files" in *[!0-9:]*) echo 'Cannot read numeric Linux VM limits.' >&2; exit 1;; esac
[ "$map" -ge 524288 ] || sysctl -w vm.max_map_count=524288
[ "$files" -ge 131072 ] || sysctl -w fs.file-max=131072
[ "$(cat /proc/sys/vm/max_map_count)" -ge 524288 ]
[ "$(cat /proc/sys/fs/file-max)" -ge 131072 ]
printf 'SonarQube VM limits verified (existing higher values retained).\n'
