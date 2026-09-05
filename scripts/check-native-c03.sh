#!/usr/bin/env bash
# Prove the native pw_filter sink with two simultaneous playback clients.
#
# The disposable graph intentionally has no session manager. The two playback
# clients are therefore native pw_filter source nodes, and every connection is
# made below with pw-link. The null sink monitor is disabled, so this check
# proves downstream delivery through the explicit graph and realtime counters.
set -Eeuo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repo_root=$(cd -- "$script_dir/.." && pwd -P)
harness=$script_dir/with-test-pipewire.sh
target_root=${CARGO_TARGET_DIR:-$repo_root/target}
proof_bin=$target_root/debug/examples/native_pw_proof

for program in cargo jq pw-dump pw-link rg timeout; do
    if ! command -v "$program" >/dev/null 2>&1; then
        printf 'required program not found: %s\n' "$program" >&2
        exit 127
    fi
done

cargo build --locked --offline --example native_pw_proof --features native-audio
if [[ ! -x $proof_bin ]]; then
    printf 'native proof helper was not built: %s\n' "$proof_bin" >&2
    exit 1
fi

if [[ ${PEQ_TEST_C03:-} != 1 ]]; then
    exec "$harness" env PEQ_TEST_C03=1 "$script_dir/check-native-c03.sh"
fi

filter_pid=''
source_a_pid=''
source_b_pid=''
filter_log=$(mktemp "${TMPDIR:-/tmp}/peq-c03-filter.XXXXXX")
source_a_log=$(mktemp "${TMPDIR:-/tmp}/peq-c03-source-a.XXXXXX")
source_b_log=$(mktemp "${TMPDIR:-/tmp}/peq-c03-source-b.XXXXXX")

cleanup_clients() {
    local status=$?
    trap - EXIT HUP INT TERM
    for pid in "$source_a_pid" "$source_b_pid" "$filter_pid"; do
        if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -f -- "$filter_log" "$source_a_log" "$source_b_log"
    exit "$status"
}
trap cleanup_clients EXIT HUP INT TERM

timeout 8s "$proof_bin" filter 6000 >"$filter_log" 2>&1 &
filter_pid=$!

wait_for_ports() {
    local deadline=$((SECONDS + 5))
    while :; do
        if pw-dump -N -R | jq -e '
            ([.[] | select(.type == "PipeWire:Interface:Node")
                | .info.props["node.name"]]
                | contains(["peq", "c03-source-a", "c03-source-b"]))
            and ([.[] | select(.type == "PipeWire:Interface:Port")
                | .info.props["port.name"]]
                | contains(["input_FL", "input_FR", "output_FL", "output_FR"]))
        ' >/dev/null 2>&1; then
            return 0
        fi
        if ((SECONDS >= deadline)); then
            printf 'timed out waiting for native C03 ports; graph:\n' >&2
            pw-dump -N -R >&2 || true
            return 124
        fi
        sleep 0.05
    done
}

"$proof_bin" source c03-source-a 440 880 4500 >"$source_a_log" 2>&1 &
source_a_pid=$!
"$proof_bin" source c03-source-b 660 1320 4500 >"$source_b_log" 2>&1 &
source_b_pid=$!
wait_for_ports

# The six links are the complete C03 route: two independent sources into the
# stereo filter and its two outputs into the isolated null sink.
pw-link c03-source-a:output_FL peq:input_FL
pw-link c03-source-a:output_FR peq:input_FR
pw-link c03-source-b:output_FL peq:input_FL
pw-link c03-source-b:output_FR peq:input_FR
pw-link peq:output_FL peq-a05-null-sink:playback_FL
pw-link peq:output_FR peq-a05-null-sink:playback_FR

links=$(pw-link -l)
for expected in \
    'c03-source-a:output_FL' 'c03-source-a:output_FR' \
    'c03-source-b:output_FL' 'c03-source-b:output_FR' \
    'peq:output_FL' 'peq:output_FR'; do
    if ! rg -Fq "$expected" <<<"$links"; then
        printf 'missing explicit C03 link endpoint: %s\n%s\n' "$expected" "$links" >&2
        exit 1
    fi
done

wait "$source_a_pid"
source_a_pid=''
wait "$source_b_pid"
source_b_pid=''
wait "$filter_pid"
filter_pid=''

source_a_result=$(<"$source_a_log")
source_b_result=$(<"$source_b_log")
filter_result=$(<"$filter_log")
source_a_calls=$(sed -n 's/.*process_calls=\([0-9][0-9]*\).*/\1/p' <<<"$source_a_result")
source_b_calls=$(sed -n 's/.*process_calls=\([0-9][0-9]*\).*/\1/p' <<<"$source_b_result")
filter_calls=$(sed -n 's/.*process_calls=\([0-9][0-9]*\).*/\1/p' <<<"$filter_result")
if [[ -z $source_a_calls || -z $source_b_calls || -z $filter_calls ]] ||
   ((source_a_calls == 0 || source_b_calls == 0 || filter_calls == 0)); then
    printf 'native C03 process proof failed:\n%s\n%s\n%s\n' \
        "$source_a_result" "$source_b_result" "$filter_result" >&2
    exit 1
fi

printf 'native C03 verified: two simultaneous sources, explicit stereo links, source_process_calls=%s/%s filter_process_calls=%s\n' \
    "$source_a_calls" "$source_b_calls" "$filter_calls"
