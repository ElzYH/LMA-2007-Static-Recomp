#!/usr/bin/env python3
"""Check that an LMA_RECORD session replays bit-exactly (#114): replay it headless with lma-cli up to
the instruction count of each state saved during the session (F5 / `save`), and compare guest
memory and registers with the state itself.

  tools/replay_check.py REC STATE [STATE ...] [--cli PATH] [--xex PATH] [--diff N]

REC is the recording (REC.state, REC.journal next to it). Exit status 1 if any state differs.
--diff N prints, for a state that differs, its first N differing memory ranges and registers
(where to look for what the journal doesn't cover, #296).
A session recorded with sound on (`<rec>.audio` = 1, written by play.sh) replays with LMA_AUDIO=1.
A session with state loads (F9 / `load`) repeats instruction counts, so each state is checked
against every stretch between loads (LMA_REPLAY_AFTER_LOADS) and passes if one of them matches.
Uses LMA_BACKEND=wgpu like lma-app (resolves land in guest memory).
"""
import argparse, hashlib, os, re, subprocess, sys, tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PEEK = '40000000:1000000,70000000:1000000,82000000:400000,a0000000:4000000'


def run(cli, xex, env, limit, cwd):
    out = subprocess.run([cli, 'run', xex, str(limit)], env={**os.environ, **env}, cwd=cwd,
                         capture_output=True, text=True)
    return out.stdout + out.stderr


def digest(text):
    lines = [l for l in text.splitlines() if re.match(r'^  (0x|r|lr|v)', l)]
    return hashlib.sha1('\n'.join(lines).encode()).hexdigest()[:12], len(lines)


def diff_ranges(saved, replay, limit=10):
    """Where two LMA_PEEK / LMA_REGS dumps differ: contiguous address ranges (16-byte rows merged)
    and the registers, the first `limit` of each, as printable lines."""
    def parse(text):
        rows, regs = {}, {}
        for l in text.splitlines():
            m = re.match(r'^  (0x[0-9a-f]+): (.*)$', l)
            if m:
                rows[int(m.group(1), 16)] = m.group(2)
            elif re.match(r'^  r\d', l):  # four GPRs a line
                f = l.split()
                for j, v in enumerate(f[1:]):
                    regs[f'r{int(f[0][1:]) + j}'] = v
            elif re.match(r'^  lr ', l):  # lr, ctr, cr in pairs
                f = l.split()
                regs.update(zip(f[0::2], f[1::2]))
            elif re.match(r'^  v\d', l):
                k, v = l.split()
                regs[k] = v
        return rows, regs
    (ra, ga), (rb, gb) = parse(saved), parse(replay)
    ranges = []
    for addr in sorted(set(ra) | set(rb)):
        if ra.get(addr) == rb.get(addr):
            continue
        if ranges and ranges[-1][1] == addr:
            ranges[-1][1] = addr + 16
        else:
            ranges.append([addr, addr + 16])
    out = [f'  memory differs in {len(ranges)} range(s)' + (f', first {limit}:' if len(ranges) > limit else ':')]
    out += [f'    {lo:#010x}..{hi:#010x} ({hi - lo} bytes)' for lo, hi in ranges[:limit]]
    regs = [k for k in sorted(set(ga) | set(gb)) if ga.get(k) != gb.get(k)]
    if regs:
        out.append(f'  registers differ: {" ".join(regs[:limit])}' + (' ...' if len(regs) > limit else ''))
    return out


def test_diff_ranges():
    a = ('  0x40000000: 00000000 00000000 00000000 00000000\n  0x40000010: 11111111 00000000 00000000 00000000\n'
         '  0x40000020: 00000000 00000000 00000000 00000000\n  r4  01 02 03 04\n  lr 0x10 ctr 0x0 cr 00000000\n')
    b = a.replace('0x40000010: 1111', '0x40000010: 2222').replace('0x40000020: 0000', '0x40000020: 9999').replace(' 03 04', ' 03 05').replace('ctr 0x0', 'ctr 0x1')
    assert diff_ranges(a, b) == ['  memory differs in 1 range(s):', '    0x40000010..0x40000030 (32 bytes)', '  registers differ: ctr r7'], diff_ranges(a, b)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('rec')
    p.add_argument('states', nargs='+')
    p.add_argument('--cli', default=os.path.join(REPO, 'lma-rs', 'target', 'release', 'lma-cli'))
    p.add_argument('--xex', default=os.path.expanduser('~/lma/LMA_full/default.xex'))
    p.add_argument('--diff', type=int, default=0, metavar='N', help='for a state that differs, print the first N differing memory ranges and registers')
    a = p.parse_args()
    rec = os.path.abspath(a.rec)
    if not os.path.exists(rec + '.journal'):
        sys.exit(f'{rec}.journal missing: only recordings made with a journal (#114) replay exactly')
    cwd = tempfile.mkdtemp(prefix='lma-replay-check-')  # last_run.log goes here, not the repo
    base = {'LMA_BACKEND': 'wgpu', 'LMA_REGS': '0', 'LMA_PEEK': PEEK}
    # a session recorded with sound on (play.sh writes `<rec>.audio` = 1) replays only with the
    # audio engine on; LMA_AUDIO in the environment still wins
    audio = rec + '.audio'
    if 'LMA_AUDIO' not in os.environ and os.path.exists(audio) and open(audio).read().strip() == '1':
        base['LMA_AUDIO'] = '1'
        print('sound-on recording: replaying with LMA_AUDIO=1')
    loads = sum(1 for l in open(rec + '.journal') if l.startswith('l '))
    ok = True
    for st in a.states:
        st = os.path.abspath(st)
        info = run(a.cli, a.xex, {'LMA_NORENDER': '1', 'LMA_LOAD': st}, 0, cwd)
        m = re.search(r'loaded .* at (\d+) instructions', info)
        if not m:
            print(f'{os.path.basename(st)}: FAIL (could not load: {info.strip()[-200:]})')
            ok = False
            continue
        clock = int(m.group(1))
        saved = run(a.cli, a.xex, {**base, 'LMA_LOAD': st}, clock, cwd)
        h2, _ = digest(saved)
        tried = []
        for k in range(loads + 1):
            replay = run(a.cli, a.xex, {**base, 'LMA_REPLAY': rec, 'LMA_REPLAY_AFTER_LOADS': str(k)}, clock, cwd)
            h1, n1 = digest(replay)
            tried.append(h1)
            if h1 == h2 and n1 > 0:
                break
        same = tried[-1] == h2 and n1 > 0
        ok &= same
        seg = f' after {len(tried) - 1} load(s)' if loads else ''
        print(f'{os.path.basename(st)} @ {clock}{seg}: {"ok" if same else "DIFFERENT"} (saved {h2}, replay {", ".join(tried)})')
        if not same and a.diff:
            print('\n'.join(diff_ranges(saved, replay, a.diff)))
    sys.exit(0 if ok else 1)


if __name__ == '__main__':
    main()
