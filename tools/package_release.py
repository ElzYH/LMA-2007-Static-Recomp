#!/usr/bin/env python3
"""Pack a release zip the same way on every machine (rc1 and rc2 were packed by hand, and rc2's
zips nearly shipped without the PlayStation glyphs, #741).

  python tools/package_release.py --out lma-v1.0-rc3-mac-arm64.zip --root lma-v1.0-rc3-mac-arm64 \\
      --bin lma-rs/target/release --mods lma-rs-v1.0-rc3-mods.zip --file README-mac.txt
  python tools/package_release.py --out lma-rs-v1.0-rc3-windows.zip --bin <dir> --mods <mods zip or dir> \\
      --file PLAY.bat --file README.txt --file BUILD.txt --ffmpeg <FFmpeg LGPL shared build folder>
                                                                  (no --root: the files at the top, as rc1)

The zip holds lma-app and lma-cli from --bin (.exe on Windows; executable bits kept), this
checkout's lma-rs/assets/glyphs/ps5 (lma-app looks for assets/glyphs/ps5 beside itself), the mods
folder (a mods zip's mods/... entries or a folder holding mods/ or the mod spaces) and every --file
at the top. With --ffmpeg (Windows, #766 b): the FFmpeg DLLs the binaries import, and the DLLs those
import in turn, beside lma-app.exe (Windows loads them from the exe's folder, so the player needs no
PATH line), with FFmpeg's licence as LICENSE-FFmpeg.txt (LGPL: an unmodified shared build may be
redistributed with its licence; only the libraries go, not ffmpeg.exe). With --ffmpeg-static
<tools/ffmpeg_min.sh output> instead (Route 1, the maintainer 22 Oct): the binaries must import no FFmpeg DLL
(built with FFMPEG_STATIC=1); the zip gets LICENSE-FFmpeg.txt (a notice naming the statically
linked version, then COPYING.LGPLv2.1) and FFmpeg-relink/ffmpeg_min.sh, the recipe that rebuilds the
libraries. With --source REF (the first-run build, #232): the tracked files of this repo at REF
(`git archive`, so nothing ignored, untracked or local) under source/ beside the binaries, where
lma-app's first run looks for the code it builds from the player's disc; with --ffmpeg-static as
well, that folder's include/ and lib/ go in as source/ffmpeg-min/, so the build has sound.
Paths use '/', not '\\'.
The result is checked with tools/release_check.py, and a zip with any game content is deleted
again (exit 1).
"""
import argparse, io, os, re, stat, subprocess, sys, tarfile, zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
sys.path.insert(0, HERE)
import release_check  # noqa: E402

GLYPHS = os.path.join(REPO, "lma-rs", "assets", "glyphs")
DLL_NAME = re.compile(rb"(?:av[a-z]+|sw[a-z]+|postproc)-[0-9]+\.dll", re.I)
FFMPEG_NOTICE = (
    "FFmpeg (https://ffmpeg.org) libraries under the GNU LGPL; the build's licence text follows.\n"
    "They are the unmodified DLLs of the LGPL shared build named below; FFmpeg's source is at\n"
    "https://git.ffmpeg.org/ffmpeg.git and the build's at https://github.com/BtbN/FFmpeg-Builds.\n"
)


def ffmpeg_dlls(bins, ffmpeg_dir):
    """[(path, name)] of the FFmpeg DLLs the binaries need: the names they import, then those DLLs'
    own imports, found in ffmpeg_dir or its bin/."""
    bin_dir = os.path.join(ffmpeg_dir, "bin") if os.path.isdir(os.path.join(ffmpeg_dir, "bin")) else ffmpeg_dir
    want, seen, todo = [], set(), [p for p, _ in bins]
    while todo:
        p = todo.pop()
        for raw in sorted(set(DLL_NAME.findall(open(p, "rb").read()))):
            name = raw.decode().lower()
            if name in seen:
                continue
            seen.add(name)
            dll = next((os.path.join(bin_dir, f) for f in os.listdir(bin_dir) if f.lower() == name), None)
            if dll is None:
                sys.exit(f"{name} (imported by {os.path.basename(p)}) is not in {bin_dir}")
            want.append((dll, os.path.basename(dll)))
            todo.append(dll)
    return sorted(want, key=lambda x: x[1].lower())


FFMPEG_STATIC_NOTICE = (
    "FFmpeg (https://ffmpeg.org) is linked statically into lma-app and lma-cli under the GNU LGPL\n"
    "2.1 or later; its licence text follows. Only libavcodec's XMA decoders (xma1, xma2, the wmapro\n"
    "code they share) and libavutil are in; no GPL or nonfree parts. FFmpeg's source is at\n"
    "https://git.ffmpeg.org/ffmpeg.git. FFmpeg-relink/ffmpeg_min.sh rebuilds these libraries (from\n"
    "that or a modified FFmpeg source); linking them into the port is\n"
    "`FFMPEG_DIR=<its output> FFMPEG_STATIC=1 cargo build --release --features xma-ffmpeg`.\n"
)
RECIPE = os.path.join(HERE, "ffmpeg_min.sh")


def ffmpeg_static_files(bins, lib_dir):
    """(licence, recipe) for --ffmpeg-static, after checking the binaries import no FFmpeg DLL."""
    for p, name in bins:
        found = sorted(set(DLL_NAME.findall(open(p, "rb").read())))
        if found:
            sys.exit(f"{name} imports {b', '.join(found).decode()}: build it with FFMPEG_STATIC=1 for --ffmpeg-static")
    licence = os.path.join(lib_dir, "COPYING.LGPLv2.1")
    for f in (licence, RECIPE):
        if not os.path.isfile(f):
            sys.exit(f"{f} is missing: a static FFmpeg ships only with its licence and its rebuild recipe")
    return licence, RECIPE


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


def source_entries(ref):
    """[(data, path, executable)] of the repo's tracked files at ref (git archive)."""
    r = subprocess.run(["git", "archive", "--format=tar", ref], cwd=REPO, capture_output=True)
    if r.returncode != 0:
        sys.exit(f"git archive {ref}: {r.stderr.decode().strip()}")
    out = []
    with tarfile.open(fileobj=io.BytesIO(r.stdout)) as t:
        for m in t.getmembers():
            if m.isfile():
                out.append((t.extractfile(m).read(), m.name, bool(m.mode & 0o111)))
    return out


def ffmpeg_min_entries(lib_dir):
    """[(path, name under ffmpeg-min/)] of a tools/ffmpeg_min.sh output's include/ and lib/."""
    out = []
    for sub in ("include", "lib"):
        top = os.path.join(lib_dir, sub)
        if not os.path.isdir(top):
            sys.exit(f"{top} is missing: --source with --ffmpeg-static ships the build's FFmpeg too")
        for dirpath, _, names in os.walk(top):
            for f in sorted(names):
                p = os.path.join(dirpath, f)
                out.append((p, os.path.relpath(p, lib_dir).replace(os.sep, "/")))
    return sorted(out, key=lambda x: x[1])


def add(z, name, data, executable=False):
    info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
    info.compress_type = zipfile.ZIP_DEFLATED
    info.external_attr = ((0o755 if executable else 0o644) | stat.S_IFREG) << 16
    z.writestr(info, data)


def pack(out, bin_dir, mods=None, files=(), root="", ffmpeg=None, ffmpeg_static=None, source=None):
    prefix = root.strip("/") + "/" if root else ""
    entries = 0
    bins = binaries(bin_dir)
    # FFmpeg checked before the zip is opened: its DLLs ship only with their licence
    dlls = ffmpeg_dlls(bins, ffmpeg) if ffmpeg else []
    licence = os.path.join(ffmpeg, "LICENSE.txt") if ffmpeg else None
    if ffmpeg and not os.path.isfile(licence):
        sys.exit(f"{licence} is missing: FFmpeg's DLLs ship only with their licence")
    if ffmpeg and ffmpeg_static:
        sys.exit("--ffmpeg and --ffmpeg-static: one or the other")
    static = ffmpeg_static_files(bins, ffmpeg_static) if ffmpeg_static else None
    src = source_entries(source) if source else []
    fmin = ffmpeg_min_entries(ffmpeg_static) if source and ffmpeg_static else []
    with zipfile.ZipFile(out, "w") as z:
        for p, name in bins:
            add(z, prefix + name, open(p, "rb").read(), executable=True)
            entries += 1
        for p, name in dlls:
            add(z, prefix + name, open(p, "rb").read())
            entries += 1
        if ffmpeg:
            head = FFMPEG_NOTICE + f"Build: {os.path.basename(os.path.normpath(ffmpeg))}; DLLs: {', '.join(n for _, n in dlls)}\n\n"
            add(z, prefix + "LICENSE-FFmpeg.txt", head.encode() + open(licence, "rb").read())
            entries += 1
        if static:
            vfile = os.path.join(ffmpeg_static, "VERSION")
            version = open(vfile, encoding="utf-8").read().strip() if os.path.isfile(vfile) else "(see ffmpeg_min.sh)"
            head = FFMPEG_STATIC_NOTICE + f"Version: FFmpeg {version}\n\n"
            add(z, prefix + "LICENSE-FFmpeg.txt", head.encode() + open(static[0], "rb").read())
            add(z, prefix + "FFmpeg-relink/ffmpeg_min.sh", open(static[1], "rb").read(), executable=True)
            entries += 2
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
        # the first-run build's input: lma-app looks for source/ beside itself
        for data, name, x in src:
            add(z, prefix + "source/" + name, data, executable=x)
            entries += 1
        for p, name in fmin:
            add(z, prefix + "source/ffmpeg-min/" + name, open(p, "rb").read())
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
    ap.add_argument("--ffmpeg", help="an FFmpeg LGPL shared build (Windows): the DLLs the binaries import go beside them, with the licence")
    ap.add_argument("--ffmpeg-static", help="tools/ffmpeg_min.sh's output, for binaries built with FFMPEG_STATIC=1: its licence and the rebuild recipe go in, no DLLs")
    ap.add_argument("--source", metavar="REF", help="this repo's tracked files at REF (a tag or commit) as source/, for lma-app's first-run build")
    a = ap.parse_args(argv)
    n = pack(a.out, a.bin, a.mods, a.file, a.root, a.ffmpeg, a.ffmpeg_static, a.source)
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
