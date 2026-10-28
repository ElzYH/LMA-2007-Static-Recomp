#!/usr/bin/env python3
"""Pack a release zip the same way on every machine (rc1 and rc2 were packed by hand, and rc2's
zips nearly shipped without the PlayStation glyphs, #741).

  python tools/package_release.py --out lma-v1.0-rc3-mac-arm64.zip --root lma-v1.0-rc3-mac-arm64 \\
      --bin lma-rs/target/release --mods lma-rs-v1.0-rc3-mods.zip --file README-mac.txt
  python tools/package_release.py --out lma-rs-v1.0-rc3-windows.zip --bin <dir> --mods <mods zip or dir> \\
      --file PLAY.bat --file README.txt --file BUILD.txt          (no --root: the files at the top, as rc1)

The zip holds lma-app and lma-cli from --bin (.exe on Windows; executable bits kept), this
checkout's lma-rs/assets/glyphs/ps5 (lma-app looks for assets/glyphs/ps5 beside itself), the mods
folder (a mods zip's mods/... entries or a folder holding mods/ or the mod spaces) and every --file
at the top. Paths use '/', not '\\'. The result is checked with tools/release_check.py, and a zip
with any game content is deleted again (exit 1).
"""
import argparse, os, stat, sys, zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
sys.path.insert(0, HERE)
import release_check  # noqa: E402

GLYPHS = os.path.join(REPO, "lma-rs", "assets", "glyphs")


def binaries(bin_dir):
    """[(path, name in the zip)] of lma-app and lma-cli (with .exe if that's what is there)."""
    out = []
    for base in ("lma-app", "lma-cli"):
        for name in (base + ".exe", base):
            p = os.path.join(bin_dir, name)
            if os.path.isfile(p):
                out.append((p, name))
                break
        else:
            sys.exit(f"{base} not found in {bin_dir}")
    return out


def mods_entries(src):
    """[(reader, name in the zip under mods/)]: from a mods zip or a folder."""
    out = []
    if zipfile.is_zipfile(src):
        z = zipfile.ZipFile(src)
        for i in z.infolist():
            n = i.filename.replace("\\", "/")
            if i.is_dir() or "/mods/" not in "/" + n:
                continue
            n = n[("/" + n).index("/mods/"):]  # from mods/ on, whatever folder the zip has above it
            out.append((lambda i=i: z.read(i), n))
        return out
    root = os.path.join(src, "mods") if os.path.isdir(os.path.join(src, "mods")) else src
    for dirpath, _, files in os.walk(root):
        for f in files:
            p = os.path.join(dirpath, f)
            rel = os.path.relpath(p, root).replace(os.sep, "/")
            out.append((lambda p=p: open(p, "rb").read(), "mods/" + rel))
    return out


def add(z, name, data, executable=False):
    info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
    info.compress_type = zipfile.ZIP_DEFLATED
    info.external_attr = ((0o755 if executable else 0o644) | stat.S_IFREG) << 16
    z.writestr(info, data)


def pack(out, bin_dir, mods=None, files=(), root=""):
    prefix = root.strip("/") + "/" if root else ""
    entries = 0
    with zipfile.ZipFile(out, "w") as z:
        for p, name in binaries(bin_dir):
            add(z, prefix + name, open(p, "rb").read(), executable=True)
            entries += 1
        # every glyph set (ps5, keyboard): lma-app looks for assets/glyphs/<set> beside itself
        for gset in sorted(os.listdir(GLYPHS)):
            gdir = os.path.join(GLYPHS, gset)
            if not os.path.isdir(gdir):
                continue
            for f in sorted(os.listdir(gdir)):
                if f.lower().endswith(".png"):
                    add(z, prefix + f"assets/glyphs/{gset}/" + f, open(os.path.join(gdir, f), "rb").read())
                    entries += 1
        if mods:
            for read, name in mods_entries(mods):
                add(z, prefix + name, read())
                entries += 1
        for f in files:
            add(z, prefix + os.path.basename(f), open(f, "rb").read(), executable=f.lower().endswith((".sh", ".command")))
            entries += 1
    return entries


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--out", required=True)
    ap.add_argument("--bin", required=True, help="folder with lma-app and lma-cli (e.g. lma-rs/target/release)")
    ap.add_argument("--mods", help="the release's mods zip, or a folder with the mod spaces")
    ap.add_argument("--file", action="append", default=[], help="a file for the top of the zip (README, PLAY.bat, BUILD.txt)")
    ap.add_argument("--root", default="", help="a folder for everything (the Mac zips); none = the top (the Windows zip)")
    a = ap.parse_args(argv)
    n = pack(a.out, a.bin, a.mods, a.file, a.root)
    fails, reviews, _ = release_check.check_zip(a.out)
    print(f"{a.out}: {n} files, {os.path.getsize(a.out) / 1e6:.1f} MB; {len(fails)} game content, {len(reviews)} mod/data files to review")
    if fails:
        for f in fails[:20]:
            print(f"  FAIL {f}")
        os.remove(a.out)
        print("deleted: a release zip must not hold game content")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
