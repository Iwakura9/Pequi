# peq

Terminal picker/editor for EasyEffects EQ curves. Browse a folder of AutoEQ
`ParametricEQ.txt` files as a tree, see the response as a braille curve, and tweak the
bands. EasyEffects (with its LSP equalizer) does all the audio. peq writes an
EasyEffects output preset named `peq` and runs `easyeffects -l peq`.

Edits are temporary: they are previewed live through EasyEffects, but the `.txt` file only
changes when you save with `s`. If you discard (`u`, or `d` when leaving), peq reloads the saved curve.

```
cargo build --release
peq [DIR]        # default DIR: ~/.config/peq/profiles (symlink it to your AutoEQ folder)
```

Loading the `peq` preset replaces EasyEffects' entire output chain with that single equalizer.

## Keys

`b` toggles the EasyEffects global bypass on both screens (shown as `BYPASS` in the status bar).

Tree: `j/k` move, `l`/`Enter` expand or open, `h` collapse, `Tab` go to the EQ screen, `q` quit.

EQ: `j/k` select a row, `h/l` select a field, `+/-` fine adjust, `[`/`]` coarse adjust, `t` change type,
`space` turn the band on or off, `n` add a band, `x` delete it, `s` save, `u` discard, `Esc`/`Tab` back to the tree, `?` help.

The old standalone audio engine (daemon, IPC, native PipeWire filter) is archived and not
compiled in `.old/`.
