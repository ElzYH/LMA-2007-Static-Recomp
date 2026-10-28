#!/usr/bin/env python3
"""Build LMA Manager 2007 for this computer from your own disc, in one command.

    python3 tools/build_from_disc.py <your LMA Manager 2007 .iso, or its extracted folder>

What it does, all on your machine (nothing is downloaded but Rust crates):
  1. extracts the disc image (tools/xiso.py) into the game folder (default: ~/LMA2007/game),
  2. recompiles the game's code from its default.xex into lma-rs/crates/game-blocks/parts/,
  3. builds the app (lma-app) and the command-line tool (lma-cli) in release mode,
  4. points the app at the game folder (its config.toml), unless a config already exists.

You need Rust (https://rustup.rs) and Python 3. The first build takes a while (the recompiled game
is large: about 10-30 minutes on a recent machine). The recompiled code and the extracted files
come from your copy of the game: don't share them.

Options: --game-dir <dir> (where the extracted game goes), --jobs N (parallel compile jobs).
"""
import argparse, os, platform, shutil, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RS = os.path.join(ROOT, "lma-rs")
EXE = ".exe" if os.name == "nt" else ""


def run(cmd, cwd=RS, env=None):
    print("+", " ".join(cmd), flush=True)
    r = subprocess.run(cmd, cwd=cwd, env=env)
    if r.returncode != 0:
        sys.exit(f"failed: {' '.join(cmd)}")


def config_path():
    home = os.path.expanduser("~")
    if sys.platform == "darwin":
        return os.path.join(home, "Library", "Application Support", "lma-rs", "config.toml")
    if os.name == "nt":
        return os.path.join(os.environ.get("APPDATA", home), "lma-rs", "config.toml")
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.join(home, ".config")
    return os.path.join(base, "lma-rs", "config.toml")


def find_xex(d):
    for name in os.listdir(d):
        if name.lower() == "default.xex":
            return os.path.join(d, name)
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("source", help="the game's .iso, or a folder with its extracted files (default.xex at the top)")
    ap.add_argument("--game-dir", default=os.path.join(os.path.expanduser("~"), "LMA2007", "game"))
    ap.add_argument("--jobs", type=int, default=None)
    a = ap.parse_args()

    if not shutil.which("cargo"):
        sys.exit("Rust isn't installed: get it from https://rustup.rs, open a new terminal, and run this again.")
    src = os.path.abspath(os.path.expanduser(a.source))
    if os.path.isdir(src):
        game = src
    elif os.path.isfile(src):
        game = os.path.abspath(os.path.expanduser(a.game_dir))
        if os.path.isdir(game) and find_xex(game):
            print(f"using the files already extracted in {game}")
        else:
            os.makedirs(game, exist_ok=True)
            run([sys.executable, os.path.join(ROOT, "tools", "xiso.py"), src, "extract", game], cwd=ROOT)
    else:
        sys.exit(f"{src} doesn't exist")
    xex = find_xex(game)
    if not xex:
        sys.exit(f"no default.xex in {game}: is this an LMA Manager 2007 (Xbox 360) disc?")

    env = dict(os.environ)
    if a.jobs:
        env["CARGO_BUILD_JOBS"] = str(a.jobs)
    run(["cargo", "build", "--release", "-p", "recomp"], env=env)
    run([os.path.join(RS, "target", "release", "lma-recomp" + EXE), xex, "all", "crates/game-blocks", "999999", "32"], env=env)
    # the recompiler rewrites the two committed placeholders: keep them out of `git status`, so the
    # game's code can't be committed by accident (parts/ itself is git-ignored)
    if os.path.isdir(os.path.join(ROOT, ".git")):
        subprocess.run(["git", "update-index", "--skip-worktree", "crates/game-blocks/Cargo.toml", "crates/game-blocks/src/lib.rs"], cwd=RS)
    run(["cargo", "build", "--release", "-p", "lma-cli", "-p", "lma-app"], env=env)

    cfg = config_path()
    if not os.path.exists(cfg):
        os.makedirs(os.path.dirname(cfg), exist_ok=True)
        with open(cfg, "w", encoding="utf-8") as f:
            f.write("game_dir = " + '"' + game.replace("\\", "\\\\") + '"\n')
        print(f"wrote {cfg} (game_dir = {game})")
    else:
        print(f"kept your {cfg}: make sure its game_dir is {game}")
    app = os.path.join(RS, "target", "release", "lma-app" + EXE)
    print(f"\nDone ({platform.system()}). Start the game with:\n  {app}")


if __name__ == "__main__":
    main()
