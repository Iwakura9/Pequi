#!/usr/bin/env bash
# Run one command against a disposable, hardware-free PipeWire daemon.
#
# The daemon and the command share a private XDG runtime directory, so the
# remote name can never resolve to a user's normal PipeWire socket.
set -Eeuo pipefail

if (($# == 0)); then
    printf 'usage: %s COMMAND [ARG... ]\n' "${0##*/}" >&2
    exit 64
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
config_file=$script_dir/../tests/pipewire/pipewire.conf
if [[ ! -r $config_file ]]; then
    printf 'pipewire test config not found: %s\n' "$config_file" >&2
    exit 66
fi
for program in pipewire pw-dump timeout; do
    if ! command -v "$program" >/dev/null 2>&1; then
        printf 'required program not found: %s\n' "$program" >&2
        exit 127
    fi
done

sandbox=$(mktemp -d "${TMPDIR:-/tmp}/peq-test-pipewire.XXXXXX")
runtime_dir=$sandbox/runtime
config_home=$sandbox/config
state_home=$sandbox/state
data_home=$sandbox/data
cache_home=$sandbox/cache
mkdir -p "$runtime_dir" "$config_home" "$state_home" "$data_home" "$cache_home"
chmod 700 "$runtime_dir"

# BASHPID is different for nested bash invocations, while $$ identifies this
# harness invocation. Together they make accidental remote reuse impractical.
remote=peq-a05-$BASHPID-$$
socket=$runtime_dir/$remote
pipewire_log=$sandbox/pipewire.log
ready_dump=$sandbox/ready.json

export XDG_RUNTIME_DIR=$runtime_dir
export XDG_CONFIG_HOME=$config_home
export XDG_STATE_HOME=$state_home
export XDG_DATA_HOME=$data_home
export XDG_CACHE_HOME=$cache_home
export PIPEWIRE_RUNTIME_DIR=$runtime_dir
export PIPEWIRE_CORE=$remote
export PIPEWIRE_REMOTE=$remote

# Do not let inherited PipeWire config selection redirect either side of this
# test to a user's configuration. The daemon still receives the explicit file
# below with -c.
unset PIPEWIRE_CONFIG_DIR PIPEWIRE_CONFIG_PREFIX PIPEWIRE_CONFIG_NAME PIPEWIRE_NO_CONFIG

pipewire -c "$config_file" >"$pipewire_log" 2>&1 &
pipewire_pid=$!

cleanup() {
    status=$?
    trap - EXIT HUP INT TERM

    # Only terminate the daemon started above. In particular, never use
    # systemctl, pkill, or a process-group kill: the user's session is out of
    # scope for this harness.
    if [[ -n ${pipewire_pid:-} ]] && kill -0 "$pipewire_pid" 2>/dev/null; then
        kill "$pipewire_pid" 2>/dev/null || true
        stop_deadline=$((SECONDS + 2))
        while kill -0 "$pipewire_pid" 2>/dev/null && ((SECONDS < stop_deadline)); do
            sleep 0.05 || true
        done
        if kill -0 "$pipewire_pid" 2>/dev/null; then
            kill -KILL "$pipewire_pid" 2>/dev/null || true
        fi
        wait "$pipewire_pid" 2>/dev/null || true
    fi
    rm -rf -- "$sandbox"
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

# A socket file alone is not sufficient: wait until a client can complete a
# bounded pw-dump handshake. This also catches a daemon that exited after
# creating the socket or a config that failed to grant local access.
deadline=$((SECONDS + 10))
while :; do
    if [[ -S $socket ]] && timeout 1s pw-dump -r "$remote" -N -R >"$ready_dump" 2>/dev/null; then
        break
    fi
    if ! kill -0 "$pipewire_pid" 2>/dev/null; then
        printf 'test PipeWire exited before readiness; log:\n' >&2
        sed -n '1,160p' "$pipewire_log" >&2 || true
        exit 1
    fi
    if ((SECONDS >= deadline)); then
        printf 'timed out waiting for test PipeWire socket %s; log:\n' "$socket" >&2
        sed -n '1,160p' "$pipewire_log" >&2 || true
        exit 124
    fi
    sleep 0.05
done

set +e
"$@"
command_status=$?
set -e
exit "$command_status"
