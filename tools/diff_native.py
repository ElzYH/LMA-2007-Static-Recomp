#!/usr/bin/env python3
"""Differential test for the native track (#6): two lma-cli builds, or one build with two switch
settings, run the same deterministic segment from a save state; the guest memory and the saved
state they end in must be byte-identical. This is the gate every native replacement passes before
its switch may default on (memory-write diff: a native function that writes anything different
from the recompiled original shows up as a differing range).

  tools/diff_native.py [--xex X] [--run N] [--work DIR] [--keep] [--mask A-B ...] NAME_A CLI_A [ENV=V ...] -- NAME_B CLI_B [ENV=V ...] -- STATE [STATE ...]

Each side runs `lma-cli run <xex> <clock+N>` with LMA_DETERMINISTIC=1 LMA_NORENDER=1 from STATE and
LMA_SAVE=<end state>; then `lma-cli state-mem` dumps each end state's guest memory and the two
dumps are compared. The registers lma-cli prints at the end and the kernel-call log (last_run.log)
of each side are compared too (first differing line reported). Differences are reported as ranges (guest physical offsets, first 32 of them)
with the bytes on each side. `--mask A-B` (hex) excludes ranges the two sides may legitimately
write differently (e.g. the emulated runtime's own packet buffers once HLE graphics skips them);
masked ranges are listed in the report, never silent. The two 4 GB dumps and end states of a state
are deleted once it is compared (`--keep` retains them; `--work DIR` puts them on a drive with room,
default the system temp). A side whose run stopped short of the target clock (the final `N instructions
in` line; a halt or crash), or the two sides ending at different clocks, fails the state. Exit status 1 on any unmasked difference or failed state.
Quiet machine not needed: the comparison is exact, not timed.
"""
import os, platform, re, subprocess, sys, tempfile

EXE = '.exe' if platform.system() == 'Windows' else ''
LOW = {'creationflags': 0x4000} if platform.system() == 'Windows' else {}


def side(args):
    return args[0], args[1], dict(kv.split('=', 1) for kv in args[2:])


def clock(cli, xex, state, cwd):
    out = subprocess.run([cli, 'run', xex, '1'], env={**os.environ, 'LMA_LOAD': state, 'LMA_NORENDER': '1', 'LMA_REGS': '0'}, cwd=cwd, capture_output=True, text=True, **LOW)
    m = re.search(r' at (\d+) instructions', out.stdout + out.stderr)
    return int(m.group(1)) if m else None


def run_side(name, cli, env, xex, state, end, cwd):
    save = os.path.join(cwd, f'{name}_end.state')
    e = {**os.environ, 'LMA_LOAD': state, 'LMA_SAVE': save, 'LMA_DETERMINISTIC': '1', 'LMA_NORENDER': '1', 'LMA_REGS': '0', **env}
    out = subprocess.run([cli, 'run', xex, str(end)], env=e, cwd=cwd, capture_output=True, text=True, **LOW)
    text = out.stdout + out.stderr
    open(os.path.join(cwd, f'{name}.log'), 'w').write(text)
    if not os.path.exists(save):
        print(f'{name}: no end state written (run stopped early? see {cwd}/{name}.log)')
        return None, [], os.path.join(cwd, f'{name}_kernel.log'), None
    # the registers lma-cli prints at the end, and the kernel-call log (one file per run: moved)
    regs = [l for l in text.splitlines() if re.match(r'^  (r|lr|v)', l)]
    kernel = os.path.join(cwd, f'{name}_kernel.log')
    if os.path.exists(os.path.join(cwd, 'last_run.log')):
        os.replace(os.path.join(cwd, 'last_run.log'), kernel)
    # the clock the run ended at: lma-cli's final "N instructions in T s" line
    ended = re.findall(r'^(\d+) instructions in ', text, re.M)
    ended = int(ended[-1]) if ended else None
    dump = os.path.join(cwd, f'{name}_mem.bin')
    r = subprocess.run([cli, 'state-mem', xex, save, dump], cwd=cwd, capture_output=True, text=True, **LOW)
    if r.returncode != 0:
        sys.exit(f'{name}: state-mem failed: {r.stderr.strip()}')
    return dump, regs, kernel, ended


def first_difference(la, lb):
    """(index, line a, line b) of the first differing line of two lists, or None."""
    for i, (x, y) in enumerate(zip(la, lb)):
        if x != y:
            return i, x, y
    if len(la) != len(lb):
        i = min(len(la), len(lb))
        return i, la[i] if i < len(la) else '<end>', lb[i] if i < len(lb) else '<end>'
    return None


def ranges(a, b, limit=32, base=0):
    """Differing [start, end) ranges of two byte strings (offsets from `base`)."""
    out = []
    i, n = 0, min(len(a), len(b))
    while i < n and len(out) < limit:
        if a[i] == b[i]:
            i += 1
            continue
        j = i
        while j < n and a[j] != b[j]:
            j += 1
        out.append((base + i, base + j))
        i = j
    if len(a) != len(b):
        out.append((base + n, base + max(len(a), len(b))))
    return out


CHUNK = 16 << 20


def diff_files(pa, pb, limit=32):
    """(size, differing ranges, a function giving the bytes of a range on each side): the dumps
    are the whole guest address space (4 GB), compared chunk by chunk, never read whole."""
    out = []
    size = max(os.path.getsize(pa), os.path.getsize(pb))
    with open(pa, 'rb') as fa, open(pb, 'rb') as fb:
        off = 0
        while off < size and len(out) < limit:
            a, b = fa.read(CHUNK), fb.read(CHUNK)
            if a != b:
                out.extend(ranges(a, b, limit - len(out), off))
            off += CHUNK

    def peek(path, s, e):
        with open(path, 'rb') as f:
            f.seek(s)
            return f.read(min(e - s, 16))
    return size, out, peek


def main():
    a = sys.argv[1:]
    opts = {'run': '2000000000', 'xex': os.path.expanduser('~/lma/LMA_full/default.xex')}
    masks = []
    keep = False
    while a and a[0].startswith('--') and a[0] != '--':
        if a[0] == '--keep':
            keep = True
            a = a[1:]
            continue
        if a[0] == '--mask':
            lo, hi = a[1].split('-')
            masks.append((int(lo, 16), int(hi, 16)))
        else:
            opts[a[0][2:]] = a[1]
        a = a[2:]
    try:
        i = a.index('--')
        j = a.index('--', i + 1)
    except ValueError:
        sys.exit(__doc__)
    sa, sb, states = side(a[:i]), side(a[i + 1:j]), [os.path.expanduser(s) for s in a[j + 1:]]
    xex, n = os.path.expanduser(opts['xex']), int(opts['run'])
    work = os.path.expanduser(opts['work']) if 'work' in opts else None
    if work:
        os.makedirs(work, exist_ok=True)
    cwd = tempfile.mkdtemp(prefix='lma-diff-', dir=work)
    print(f'logs and dumps in {cwd}', flush=True)
    bad = 0
    for state in states:
        c0 = clock(sa[1], xex, state, cwd)
        if c0 is None:
            sys.exit(f'{state}: could not read the state clock')
        end = c0 + n
        pa, ra, ka, ea = run_side(sa[0], sa[1], sa[2], xex, state, end, cwd)
        pb, rb, kb, eb = run_side(sb[0], sb[1], sb[2], xex, state, end, cwd)
        # a run ends at the first slice boundary at or past the target (both sides at the same
        # clock when they agree); a side that stopped short (a halt, a crash) fails the state
        short = [(nm, e) for nm, e in ((sa[0], ea), (sb[0], eb)) if e is None or e < end or pa is None or pb is None]
        if not short and ea != eb:
            short = [(sa[0], ea), (sb[0], eb)]
        if short:
            print(f'{os.path.basename(state)}: FAILED, the runs did not end at the target clock {end} together: ' + ', '.join(f'{nm} ended at {e}' for nm, e in short))
            bad += 1
            if not keep:
                for f in (pa, pb, os.path.join(cwd, f'{sa[0]}_end.state'), os.path.join(cwd, f'{sb[0]}_end.state')):
                    if f and os.path.exists(f):
                        os.remove(f)
            continue
        size, diffs, peek = diff_files(pa, pb)
        regs = first_difference(ra, rb)
        klines = lambda p: open(p, errors='replace').read().splitlines() if os.path.exists(p) else []
        kern = first_difference(klines(ka), klines(kb))
        if regs:
            print(f'{os.path.basename(state)}: registers differ at line {regs[0]}: {sa[0]} {regs[1].strip()} | {sb[0]} {regs[2].strip()}')
        if kern:
            print(f'{os.path.basename(state)}: kernel-call logs differ at line {kern[0]}: {sa[0]} {kern[1].strip()[:80]} | {sb[0]} {kern[2].strip()[:80]}')
        bad += bool(regs) + bool(kern)
        unmasked = [(s, e) for (s, e) in diffs if not any(lo <= s and e <= hi for lo, hi in masks)]
        masked = [(s, e) for (s, e) in diffs if (s, e) not in unmasked]
        tag = 'IDENTICAL' if not diffs else ('MASKED' if not unmasked else 'DIFFERENT')
        print(f'{os.path.basename(state)}: {n / 1e9:.1f} G instructions from {c0}: memory {tag} ({size >> 20} MB compared, {len(diffs)} differing ranges, {len(masked)} masked); registers {"same" if not regs else "DIFFERENT"}, kernel log {"same" if not kern else "DIFFERENT"}')
        for s, e in unmasked[:32]:
            print(f'  {s:#010x}-{e:#010x} ({e - s} B): {sa[0]} {peek(pa, s, e).hex()} | {sb[0]} {peek(pb, s, e).hex()}')
        for s, e in masked[:8]:
            print(f'  masked {s:#010x}-{e:#010x} ({e - s} B)')
        bad += bool(unmasked)
        if not keep:   # the two 4 GB dumps and end states: gone once compared (logs stay)
            for f in (pa, pb, os.path.join(cwd, f'{sa[0]}_end.state'), os.path.join(cwd, f'{sb[0]}_end.state')):
                if os.path.exists(f):
                    os.remove(f)
    sys.exit(1 if bad else 0)


if __name__ == '__main__':
    main()
