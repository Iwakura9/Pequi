# Arquitetura do `peq`

Este é o desenho técnico vigente para a versão 1.0. O histórico de decisões que
levou até ele está em [`NOTES.md`](../NOTES.md), enquanto [`prompt.md`](../prompt.md)
é o prompt original arquivado. Quando esses arquivos contradizem este documento,
este desenho representa o requisito atual.

## Componentes

```text
                    ┌──────────────┐
                    │ CLI          │
                    └──────┬───────┘
                           │ JSON Lines / Unix socket
                    ┌──────▼───────┐
                    │ daemon peq   │── state, revisions, leases
                    └──────┬───────┘
                           │ EngineClient
                    ┌──────▼───────┐
                    │ audio engine │── pw_filter + Rust DSP
                    └──────┬───────┘
   applications ──> stable peq sink ── explicit link ──> selected output
                    ▲
                    │
                    └──────── TUI / EngineClient
```

The executable remains `peq`, built in Rust with `ratatui`, `crossterm`, `clap`,
`serde`/`toml`, `anyhow` at boundaries and `thiserror` inside libraries. The core
library contains preset documents, validation, DSP, renderer and application state.
Adapters contain the PipeWire bindings, Unix IPC and terminal integration. CLI and
TUI call shared application services.

## Audio graph

The engine creates one duplex `pw_filter` graph with explicit links and a stable
`peq` sink. It supports two application streams at once. Output selection is an
explicit operation; links owned by `peq` are maintained without touching unrelated
links, and cycles are rejected. PipeWire outputs are discovered from events and
identified with stable device information in addition to temporary object IDs.

Sample rate is obtained from the active graph and passed to both coefficient and
response calculations. Coefficients are prepared outside the audio callback. The
callback processes stereo blocks with independent state per channel, using fixed
capacity queues and preallocated filter banks. It performs no allocation, lock,
file access, logging or other I/O.

An update carries a monotonically increasing revision. The engine crossfades the old
and new filter banks over 20 ms; rapid updates coalesce toward the newest revision.
Bypass uses the same transition path. Server loss, reconnection and sample-rate
changes reconstruct the graph from the confirmed snapshot, never from an expired
preview.

The initial integration must run inside a PipeWire session configured with its own
socket and runtime/config directories. It must not use physical devices or alter the
normal user session. This isolated session is the proof for duplex links and two
simultaneous playback clients.

## Presets and state

`PresetDocument` is versioned and has a stable identity, name, metadata, manual
preamp, match patterns and up to 20 bands. Each band has an explicit `enabled` flag
and a supported type: peaking, lowshelf or highshelf. Validation centrally enforces
20 Hz–20 kHz and Nyquist, Q 0.05–50, gain −24–24 dB and preamp −60–12 dB. It rejects
non-finite values, unsafe names and excess bands with diagnostics.

`PresetStore` resolves XDG paths, reads legacy TOML without rewriting it, validates
documents, imports AutoEQ/SquigLink TXT and the known legacy JSON, exports TOML/TXT,
and manages create, duplicate, rename, recoverable delete and favorites. Writes use
temporary files plus atomic rename, backups and concurrent-edit detection. Invalid
presets are reported individually while healthy entries remain available.

`AppliedSnapshot` is the confirmed document, revision and bypass state. It records
what the engine accepted, independently of a saved library document or a temporary
preview. `EngineStatus` reports connection, selected output, actual sample rate,
applied revision, input/output metrics and clipping.

## IPC and preview ownership

CLI and TUI communicate with the daemon through versioned JSON Lines over a Unix
socket accessible only to the user. Requests include IDs, expected revisions and
timeouts. Success is returned only after the engine confirms the requested revision.
The daemon serializes mutations and rejects stale revisions.

Saving changes the library. Applying confirms and persists the snapshot to restore on
future daemon starts. A live listen operation creates an exclusive, expiring preview
lease; A/B switches between confirmed content and that draft. Bypass temporarily
suspends equalization while retaining the confirmed content. If the preview owner
disconnects or its lease expires, the engine restores the confirmed snapshot. A newer
external command revokes older preview ownership, so a stale TUI cannot undo it.

## UI behavior

The TUI is keyboard-complete. Wide terminals show the preset library and editor at
once; 80×24 uses switchable panels; smaller terminals retain the draft and explain
how to resize. The theme uses a graphite background, cyan focus, distinct curve
colors and amber attention. Text and symbols accompany color states, with high
contrast and ASCII fallback.

The editor exposes frequency, gain, Q, type, enabled state and preamp; add, duplicate,
delete, reorder, undo/redo, restore, save, apply, preview, A/B, bypass, import/export,
output selection, device matching, help and first-run recovery. Its graph uses the
same response calculations and effective sample rate as the engine.

## Operational invariants

- The `peq` sink is stable while presets, previews and bypass change.
- The normal path never restarts the global PipeWire session.
- A reported success means the requested engine revision was confirmed.
- Recovery uses `AppliedSnapshot`; abandoned previews are never resurrected.
- Unrelated PipeWire links and outputs are preserved.
- Audio callback code is bounded and free of allocation, locks and I/O.
- Preset writes are recoverable and concurrent edits are surfaced.
- The TUI always retains an unsaved draft across backend errors and terminal cleanup.

## Historical context

The first implementation used PipeWire's filter-chain module with 20 fixed slots and
runtime property writes. The local PipeWire verification recorded in `NOTES.md`
showed that writes were accepted but did not reach the filter graph, so the MVP
switched to regenerating config and restarting the entire session. That fallback is
archived history and explains the abandoned state in `README.md`; the current 1.0
plan adopts an in-process `pw_filter` engine to keep the sink stable and provide
continuous processing.
