# LMA Manager 2007 — Static Recompilation

A native PC and Mac port of **LMA Manager 2007** (Xbox 360), built by *static recompilation*: the
game's PowerPC code is translated ahead of time into Rust, and the Xbox around it (kernel, threads,
GPU, audio) is replaced with native code. No emulator runs at play time.

The goal is the whole game as readable, moddable native code, playable at 60 fps on Windows,
macOS (Apple Silicon and Intel) and Linux, from your own copy of the game.

## Status

- The whole game recompiles (about 23,000 functions, 99 % native) and plays: career mode, matches
  in 3D, saves and loads, sound.
- Rendering runs on the GPU through wgpu (Metal, Vulkan, DirectX 12).
- Modding: the game's database (players, clubs, squads, names, badges) and 50+ of its rules (wages,
  transfers, prize money, player development, substitutions) can be changed by name. See
  [MODS.md](MODS.md).
- Releases are source only: one command builds the game from your own disc (below). A download
  that recompiles from your disc at first run is planned.

See [lma-rs/PORT_PLAN.md](lma-rs/PORT_PLAN.md) for the architecture and
[docs/NATIVE_GAME.md](docs/NATIVE_GAME.md) for the plan towards fully readable code.

## Building

You need your own legally obtained LMA Manager 2007 (Xbox 360) disc image, [Rust](https://rustup.rs)
and Python 3. Then, from this folder:

```
python3 tools/build_from_disc.py <your LMA Manager 2007 .iso>
```

It extracts the disc, recompiles the game's code from its `default.xex`, builds the app and points
it at the game files, all on your machine. The first build takes about 10-30 minutes. Then start
`lma-rs/target/release/lma-app` (`lma-app.exe` on Windows). An already extracted copy works too:
pass its folder instead of the `.iso`.

By hand, the same steps are:

```
python3 tools/xiso.py <your.iso> extract <dir>
cd lma-rs
cargo build --release -p recomp
target/release/lma-recomp <dir>/default.xex all crates/game-blocks 999999 32
cargo build --release -p lma-cli -p lma-app
```

The app finds the game files through `game_dir` in its `config.toml` (`~/Library/Application
Support/lma-rs/` on macOS, `%APPDATA%\lma-rs\` on Windows, `~/.config/lma-rs/` elsewhere).

The recompiled code (`crates/game-blocks/parts/`) and the extracted files are generated from your
copy of the game: never share or commit them.

## Layout

| crate | job |
|---|---|
| `xex` | XEX2 loader: decryption, decompression, sections, imports |
| `ppc` | Xenon (PowerPC) decoder |
| `recomp` | PowerPC → Rust translator |
| `guest` | CPU state, guest memory, the helpers recompiled code calls |
| `runtime` | kernel and XAM in native code, threads, GPU, audio, mods |
| `lma-data` | the game's database format (players, clubs, squads, names) |
| `lma-import` | builds season mods from external squad data |
| `lma-cli` | headless runner and tools |
| `lma-app` | the game window, launcher and settings (Bevy) |

## Legal

See [LEGAL.md](LEGAL.md) for the full notice. In short:


This project contains no game code or assets. You need your own legally obtained copy of
LMA Manager 2007 (Xbox 360); the game's code is recompiled locally from your own files.
Not affiliated with or endorsed by Codemasters. LMA Manager is a trademark of its respective owner.

## How it's built

Developed with a team of AI coding agents (Claude Code) across Mac and Windows machines,
coordinated through GitHub issues and pull requests. The maintainer sets the architecture,
priorities and milestones, and reviews and merges every change.

## License

The code in this repository is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option. The licence covers this project's own code only:
LMA Manager 2007 itself, its code and its assets remain the property of their owners (see
[LEGAL.md](LEGAL.md)).
