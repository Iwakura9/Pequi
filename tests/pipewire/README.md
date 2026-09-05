# Disposable PipeWire session

Run `scripts/check-test-pipewire.sh` from the repository root to validate the
native PipeWire path against a short lived server. It starts the daemon with
`tests/pipewire/pipewire.conf`, which contains only the native protocol,
access, metadata, adapter, and node factory modules. The graph has a dummy
driver and a fixed two channel `peq-a05-null-sink`; it does not load ALSA,
PulseAudio, Bluetooth, JACK, or WirePlumber.

`scripts/with-test-pipewire.sh COMMAND [ARG...]` is the reusable wrapper. Each
invocation gets a fresh `mktemp` tree for `XDG_RUNTIME_DIR` and XDG
config/state/data/cache homes, plus a unique `PIPEWIRE_REMOTE`. Readiness is
bounded by ten seconds and requires a completed `pw-dump` handshake. On exit
the wrapper terminates only the daemon PID it started and removes its
temporary tree. It never calls systemd or changes the ordinary user session.
