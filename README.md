# Pequi

A terminal picker and editor for parametric EQ curves, played through
[EasyEffects](https://github.com/wwmm/easyeffects).

Curves are plain text files, one band per line:

```
Preamp: -3.0 dB
Filter 1: ON LSC Fc 60 Hz Gain 2.0 dB Q 0.700
Filter 2: ON PK Fc 200 Hz Gain 1.0 dB Q 1.400
Filter 3: ON HSC Fc 15000 Hz Gain 3.0 dB Q 0.700
```

Browse a folder of them as a tree, see the frequency response as a
braille plot, and tweak bands while listening. Pequi does no audio processing itself:
EasyEffects and its LSP equalizer handle that. Pequi just writes an EasyEffects output
preset named `pequi` and asks the running EasyEffects to load it.

Edits are previewed live, but the `.txt` file only changes when you save. Discard and
you're back to the saved curve.

## Requirements

- Linux with PipeWire
- EasyEffects (with the LSP plugins it uses for its equalizer), running
- A stable Rust toolchain

## Install

```sh
cargo install --path .
```

## Usage

```sh
pequi [DIR]
```

`DIR` defaults to `~/.config/pequi/profiles`. Symlinking it to the folder where you keep
your curves works well.

To try it without your own curves:

```sh
pequi examples/profiles
```

The bundled examples (V-Shape, U-Shape, Bass Boost, Treble Control, Relaxed) are
generic tonal tilts, not tuned to any particular headphone.

> Loading the `pequi` preset replaces EasyEffects' whole output chain with that single
> equalizer.

## Keys

| Where | Key | Action |
|---|---|---|
| anywhere | `b` | bypass: filters off, preamp kept (levels stay comparable) |
| tree | `j` / `k` | move |
| tree | `Enter` | expand folder or open curve (`Flat`, at the top, plays no filters at 0 dB preamp) |
| tree | `Tab` | go to the EQ screen |
| tree | `q` | quit |
| EQ | `+` / `-` | adjust the selected value |
| EQ | `Enter` | type a value, or pick the band type from a list |
| EQ | `space` | enable / disable band |
| EQ | `s` | solo band (not saved) |
| EQ | `n` / `d` | new / delete band |
| EQ | `Ctrl+s` | save |
| EQ | `u` | undo (back to the saved curve) |
| EQ | `Esc` / `Tab` | back to the tree |
| EQ | `?` | help |

Arrow keys move around both screens. Opening another curve or quitting drops unsaved
edits.

## Development

```sh
cargo test --all-targets
cargo test -- --ignored   # loads a curve into the running EasyEffects (changes live audio)
cargo clippy --all-targets -- -D warnings
```

## License

[MIT](LICENSE)
