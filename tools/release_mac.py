#!/usr/bin/env python3
"""The Mac release's three zips in one command, as v1.0-rc5 was made by hand (29 Oct):

  python tools/release_mac.py v1.0-rc6 --bin ~/lma/play/builds/<build> --commit <sha> --mods <dir with mods/> \\
      --previous <build it was gated against> --gate "<the gate's result>" [--out <dir>] [--ffmpeg ~/lma/ffmpeg-min]
  python tools/release_mac.py --selftest

1. README-mac.txt from tools/macos/README-mac.txt.in ({version} {commit} {mod} {previous} {gate}); {mod} is
   the mod space menu.txt lists first.
   The mods: only menu.txt and the mod spaces it names go in (the others are listed as left out).
2. lma-<version>-mac-arm64.zip: package_release --ffmpeg-static <ffmpeg> --source <commit> (binaries, glyphs,
   mods, README, FFmpeg's licence and relink recipe, source/ with ffmpeg-min).
3. lma-rs-<version>-mods.zip: the mods folder.
4. lma-<version>-mac-arm64-app.zip: that zip unpacked, tools/macos/bundle.sh on it (the commit's own
   bundle.sh, so the bundle is the release's), zipped with ditto -c -k --keepParent.
5. release_check --zip on all three: no game content, the same mod spaces.
Nothing is published: upload the three with `gh release create` and run `release_check.py <tag> --local URL`.
"""
import argparse, os, re, shutil, subprocess, sys, tempfile, zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
TEMPLATE = os.path.join(HERE, "macos", "README-mac.txt.in")
sys.path.insert(0, HERE)


def first_mod(mods):
    """The first mod space menu.txt lists (its lines are `id|name|...`), else the first folder."""
    root = os.path.join(mods, "mods") if os.path.isdir(os.path.join(mods, "mods")) else mods
    menu = os.path.join(root, "menu.txt")
    if os.path.isfile(menu):
        for line in open(menu, encoding="utf-8"):
            ident = line.split("|")[0].strip()
            if ident and not ident.startswith("#") and os.path.isdir(os.path.join(root, ident)):
                return ident
    dirs = sorted(d for d in os.listdir(root) if os.path.isdir(os.path.join(root, d)))
    return dirs[0] if dirs else "none"


def curated_mods(mods, into):
    """Copy menu.txt and the mod spaces it names into `into`/mods; ([kept], [left out]). A player's
    play folder holds every old mod build (55 spaces, menu.txt.bak-*: 335 MB zipped, mac-perf's #928
    review), so only what the launcher's menu offers ships."""
    root = os.path.join(mods, "mods") if os.path.isdir(os.path.join(mods, "mods")) else mods
    menu = os.path.join(root, "menu.txt")
    if not os.path.isfile(menu):
        sys.exit(f"{menu} is missing: the release ships the mod spaces menu.txt lists")
    named = []
    for line in open(menu, encoding="utf-8"):
        ident = line.split("|")[0].strip()
        if ident and not ident.startswith("#") and os.path.isdir(os.path.join(root, ident)) and ident not in named:
            named.append(ident)
    dst = os.path.join(into, "mods")
    os.makedirs(dst, exist_ok=True)
    shutil.copy2(menu, dst)
    for ident in named:
        shutil.copytree(os.path.join(root, ident), os.path.join(dst, ident))
    left = sorted(n for n in os.listdir(root) if n != "menu.txt" and n not in named)
    return named, left


def readme(version, commit, mod, previous, gate, template=TEMPLATE):
    text = open(template, encoding="utf-8").read()
    left = set(re.findall(r"\{(\w+)\}", text)) - {"version", "commit", "mod", "previous", "gate"}
    if left:
        sys.exit(f"{template} has placeholders this script doesn't fill: {sorted(left)}")
    return text.format(version=version, commit=commit, mod=mod, previous=previous, gate=gate)


def zip_dir(src, out, arc_root="mods"):
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as z:
        for dirpath, _, files in os.walk(src):
            for f in sorted(files):
                p = os.path.join(dirpath, f)
                z.write(p, os.path.join(arc_root, os.path.relpath(p, src)).replace(os.sep, "/"))


def run(cmd, **kw):
    r = subprocess.run(cmd, **kw)
    if r.returncode:
        sys.exit(f"failed ({r.returncode}): {' '.join(map(str, cmd))}")


def release(a):
    import package_release, release_check
    out = os.path.abspath(a.out or f"release-{a.version}")
    os.makedirs(out, exist_ok=True)
    commit = subprocess.run(["git", "rev-parse", "--short=8", a.commit], cwd=REPO, capture_output=True, text=True, check=True).stdout.strip()
    mods_root = os.path.join(a.mods, "mods") if os.path.isdir(os.path.join(a.mods, "mods")) else a.mods
    root = f"lma-{a.version}-mac-arm64"
    folder_zip, mods_zip, app_zip = (os.path.join(out, n) for n in (f"{root}.zip", f"lma-rs-{a.version}-mods.zip", f"{root}-app.zip"))
    with tempfile.TemporaryDirectory() as t:
        kept, left = curated_mods(a.mods, os.path.join(t, "m"))
        print(f"mods: {', '.join(kept) or 'none'} (menu.txt's){'; left out: ' + ', '.join(left[:8]) + (' ...' if len(left) > 8 else '') if left else ''}")
        a.mods, mods_root = os.path.join(t, "m"), os.path.join(t, "m", "mods")
        rd = os.path.join(t, "README-mac.txt")
        open(rd, "w", encoding="utf-8").write(readme(a.version, commit, first_mod(a.mods), a.previous, a.gate, a.template))
        if package_release.main(["--out", folder_zip, "--root", root, "--bin", a.bin, "--mods", a.mods, "--file", rd,
                                 "--ffmpeg-static", a.ffmpeg, "--source", a.commit]):
            return 1
        zip_dir(mods_root, mods_zip)
        unz = os.path.join(t, "unz")
        zipfile.ZipFile(folder_zip).extractall(unz)
        for name in ("lma-app", "lma-cli"):  # zipfile drops the exec bits
            os.chmod(os.path.join(unz, root, name), 0o755)
        bundle = a.bundle or os.path.join(t, "bundle.sh")
        if not a.bundle:  # the release commit's own bundle.sh
            r = subprocess.run(["git", "show", f"{a.commit}:tools/macos/bundle.sh"], cwd=REPO, capture_output=True)
            if r.returncode:
                sys.exit(f"{a.commit} has no tools/macos/bundle.sh (pass --bundle)")
            open(bundle, "wb").write(r.stdout)
        appdir = os.path.join(t, "app")
        os.makedirs(appdir)
        env = dict(os.environ)
        env.pop("LMA_BUNDLE_ICON", None)
        run(["bash", bundle, os.path.join(unz, root), appdir, a.version.lstrip("v")], env=env)
        if os.path.exists(app_zip):
            os.remove(app_zip)
        run(["ditto", "-c", "-k", "--keepParent", os.path.join(appdir, "LMA.app"), app_zip])
    print()
    return release_check.main(["--zip", folder_zip, app_zip, mods_zip])


def selftest():
    with tempfile.TemporaryDirectory() as d:
        mods = os.path.join(d, "m", "mods")
        os.makedirs(os.path.join(mods, "fc27b"))
        os.makedirs(os.path.join(mods, "fc27a"))
        open(os.path.join(mods, "menu.txt"), "w").write("# versions\nvanilla|Vanilla|x\nfc27b|Modded|y\n")
        assert first_mod(os.path.join(d, "m")) == "fc27b"  # vanilla isn't a folder; the first that is
        os.remove(os.path.join(mods, "menu.txt"))
        assert first_mod(mods) == "fc27a"
        # only what menu.txt offers ships: not the old builds and backups beside it
        open(os.path.join(mods, "menu.txt"), "w").write("vanilla|Vanilla|x\nfc27b|Modded|y\n")
        open(os.path.join(mods, "menu.txt.bak-1"), "w").write("old\n")
        kept, left = curated_mods(os.path.join(d, "m"), os.path.join(d, "c"))
        assert kept == ["fc27b"] and left == ["fc27a", "menu.txt.bak-1"], (kept, left)
        assert sorted(os.listdir(os.path.join(d, "c", "mods"))) == ["fc27b", "menu.txt"]
        os.remove(os.path.join(mods, "menu.txt"))
        os.remove(os.path.join(mods, "menu.txt.bak-1"))
        tpl = os.path.join(d, "t.in")
        open(tpl, "w").write("{version} at {commit}: {mod}, against {previous}: {gate}\n")
        assert readme("v9", "abc", "m", "p", "g", tpl) == "v9 at abc: m, against p: g\n"
        open(tpl, "w").write("{version} {extra}\n")
        try:
            readme("v9", "a", "m", "p", "g", tpl)
            raise AssertionError("an unknown placeholder must stop it")
        except SystemExit:
            pass
        open(os.path.join(mods, "fc27a", "mod.toml"), "w").write("base_year = 2026\n")
        zip_dir(mods, os.path.join(d, "mods.zip"))
        assert zipfile.ZipFile(os.path.join(d, "mods.zip")).namelist() == ["mods/fc27a/mod.toml"]
        if os.path.isfile(TEMPLATE):
            assert "{version}" in open(TEMPLATE).read()
    print("selftest ok")


def main(argv):
    if argv == ["--selftest"]:
        selftest()
        return 0
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("version")
    ap.add_argument("--bin", required=True, help="the gated build: lma-app and lma-cli (FFMPEG_STATIC=1)")
    ap.add_argument("--commit", required=True, help="the commit the build is from (the tag's)")
    ap.add_argument("--mods", required=True, help="a folder with mods/ (menu.txt and the mod spaces)")
    ap.add_argument("--previous", required=True, help="the build it was gated against")
    ap.add_argument("--gate", required=True, help="the gate's result, one line")
    ap.add_argument("--ffmpeg", default=os.path.expanduser("~/lma/ffmpeg-min"), help="tools/ffmpeg_min.sh's output")
    ap.add_argument("--out", help="where the zips go (default release-<version>)")
    ap.add_argument("--template", default=TEMPLATE, help="the README template (default tools/macos/README-mac.txt.in)")
    ap.add_argument("--bundle", help="a bundle.sh to use instead of the commit's own")
    return release(ap.parse_args(argv))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
