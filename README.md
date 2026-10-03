# peq

A terminal picker and editor for [AutoEQ](https://github.com/jaakkopasanen/AutoEq)
curves, played through [EasyEffects](https://github.com/wwmm/easyeffects).

Browse a folder of `ParametricEQ.txt` files as a tree, see the frequency response as a
braille plot, and tweak bands while listening. peq does no audio processing itself:
EasyEffects and its LSP equalizer handle that. peq just writes an EasyEffects output
preset named `peq` and asks the running EasyEffects to load it.

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
peq [DIR]
```

`DIR` defaults to `~/.config/peq/profiles`. Pointing it at your AutoEQ folder with a
symlink works well.

> Loading the `peq` preset replaces EasyEffects' whole output chain with that single
> equalizer.

## Keys

| Where | Key | Action |
|---|---|---|
| anywhere | `b` | toggle EasyEffects global bypass |
| tree | `j` / `k` | move |
| tree | `l` / `Enter` | expand folder or open curve |
| tree | `h` | collapse |
| tree | `Tab` | go to the EQ screen |
| tree | `q` | quit |
| EQ | `j` / `k` | select band |
| EQ | `h` / `l` | select field |
| EQ | `+` / `-` | fine adjust |
| EQ | `[` / `]` | coarse adjust |
| EQ | `t` | change band type |
| EQ | `space` | enable / disable band |
| EQ | `n` / `x` | add / delete band |
| EQ | `s` / `u` | save / discard |
| EQ | `Esc` / `Tab` | back to the tree |
| EQ | `?` | help |

Leaving a curve with unsaved edits asks whether to save (`s`) or discard (`d`).

## Development

```sh
cargo test --all-targets
cargo test -- --ignored   # loads a curve into the running EasyEffects (changes live audio)
cargo clippy --all-targets -- -D warnings
```

## License

[MIT](LICENSE)
