# peq

A parametric EQ for the terminal, backed by PipeWire's `libpipewire-module-filter-chain`.
CLI-first (swap headphone EQ curves in one command), with a TUI for editing presets.

## Important: read this before `peq init`

The original design assumed you could push new EQ values into a running filter-chain
without reloading it. **That doesn't work on this PipeWire build** (verified exhaustively -
see `NOTES.md`). So `peq <name>` regenerates the filter-chain config and runs
`systemctl --user restart pipewire pipewire.socket wireplumber` - **this briefly restarts
the whole PipeWire session, not just peq's own sink.** In practice it's fast (~200-300ms)
and most apps reconnect on their own, but there's an audible blip system-wide on every
preset switch. This is documented, not silently swallowed.

## Install

```
cargo build --release
install -Dm755 target/release/peq ~/.local/bin/peq
```

Requires `clang` at build time (`pipewire`/`libspa` bindings use `bindgen`):
```
sudo pacman -S clang
```

## Usage

```
peq init                 # generate ~/.config/pipewire/pipewire.conf.d/99-peq.conf
systemctl --user restart pipewire pipewire.socket wireplumber

peq import HD6XX.txt     # AutoEQ ParametricEQ.txt -> ~/.config/peq/presets/HD6XX.toml
peq hd6                  # fuzzy-matches "HD6XX", applies it (restarts PipeWire)
peq                      # print the active preset + its curve
peq ls                   # list presets
peq off / peq on         # bypass / restore, without forgetting which preset is active
peq next                 # cycle presets (bind to a key)
peq show hd6              # preview a curve without applying it
peq edit hd6               # open the TUI on that preset
peq status --json         # waybar-friendly: {"text":"HD6XX","tooltip":"preamp -6.0 dB","class":"active"}
peq completions fish > ~/.config/fish/completions/peq.fish
```

### Make peq's sink the default

Apps already playing don't move to a new sink by themselves. Set peq's sink as the
system default once:

```
wpctl set-default $(wpctl status | grep -m1 ' peq ' | grep -oE '[0-9]+')
```

New streams will use it; anything already playing needs `wpctl set-default` again, or a
manual move via `pw-metadata` / `qpwgraph` / your session manager's routing UI.

### waybar

```jsonc
"custom/peq": {
    "exec": "peq status --json",
    "return-type": "json",
    "interval": 5,
    "on-click": "peq next"
}
```

### Hyprland bind

```
bind = $mainMod, E, exec, peq next
```

## TUI

`peq edit <name>`: arrow keys (or hjkl) navigate rows/fields, `+`/`-` fine-adjust, `[`/`]`
coarse-adjust, `s` saves, `a` pushes the current edit to the real sink (debounced - see
NOTES.md, this also restarts PipeWire), `?` toggles help, `q` quits (asks again if unsaved).
The response curve preview is instant and local; it does not touch PipeWire.

## Preset format

`~/.config/peq/presets/<name>.toml`:

```toml
name = "HD6XX"
preamp_db = -6.0
match = ["Sennheiser HD 6XX*"]   # reserved for a future `peq watch`, validated on load

[[band]]
type = "lowshelf"   # peaking | lowshelf | highshelf
freq = 105.0
gain = 4.0
q = 0.70
```

20 fixed filter-chain slots back every preset (1 lowshelf + 18 peaking + 1 highshelf), so
every preset fits the same graph shape - see `NOTES.md` for why.

## Development

```
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

PipeWire-touching integration behavior is exercised through `peq`'s own commands against a
live session (see NOTES.md for how Fase 0 was verified) rather than gated `cargo test`
integration tests, since there's no way to spin up a disposable PipeWire session cheaply in
CI. The unit tests (`dsp`, `render`, `chain`, `preset`) are pure and run everywhere.
