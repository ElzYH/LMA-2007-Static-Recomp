# Contributing

Issues and pull requests are welcome. Read [LEGAL.md](LEGAL.md) first: nothing derived from the
game (code, assets, recompiled output, save states, data dumps) may be posted here.

- Build and test: `cd lma-rs && cargo test --workspace` (the game-blocks crate builds as empty
  placeholders without your own recompiled parts).
- Before committing: `bash .github/scripts/no-game-content.sh` must print "no game content tracked".
- Keep changes small and focused, with a short description of what changed and how it was checked.
