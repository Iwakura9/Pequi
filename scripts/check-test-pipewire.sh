#!/usr/bin/env bash
# Integration check for the disposable PipeWire session.
set -Eeuo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
harness=$script_dir/with-test-pipewire.sh

for program in jq pw-dump timeout; do
    if ! command -v "$program" >/dev/null 2>&1; then
        printf 'required program not found: %s\n' "$program" >&2
        exit 127
    fi
done

# Re-enter this script as the command hosted by the disposable daemon. This
# keeps all client environment setup in one place and makes direct invocation
# convenient for developers and CI.
if [[ ${PEQ_TEST_PIPEWIRE_CHECK:-} != 1 ]]; then
    exec "$harness" env PEQ_TEST_PIPEWIRE_CHECK=1 "$script_dir/check-test-pipewire.sh"
fi

: "${PIPEWIRE_REMOTE:?with-test-pipewire.sh did not set PIPEWIRE_REMOTE}"
dump=$(timeout 5s pw-dump -r "$PIPEWIRE_REMOTE" -N -R)

jq -e --arg remote "$PIPEWIRE_REMOTE" '
    any(.[]; .type == "PipeWire:Interface:Core" and .info.name == $remote)
    and ([.[] | select(.type == "PipeWire:Interface:Node")]
        | map(.info.props["node.name"])
        | sort == ["peq-a05-dummy-driver", "peq-a05-null-sink"])
    and any(.[];
        .type == "PipeWire:Interface:Node"
        and .info.props["node.name"] == "peq-a05-null-sink"
        and .info.props["media.class"] == "Audio/Sink")
    and ([.[] | select(.type == "PipeWire:Interface:Port")
        | .info.props["port.name"]]
        | sort == ["playback_FL", "playback_FR"])
    and ([.[] | select(.type == "PipeWire:Interface:Port")
        | .info.props["audio.channel"]]
        | sort == ["FL", "FR"])
    and (any(.[]; ((.info.props // {}) | tostring | test("alsa"; "i"))) | not)
' <<<"$dump" >/dev/null

printf 'test PipeWire verified: remote=%s, stereo null sink, no ALSA objects\n' "$PIPEWIRE_REMOTE"
