#!/usr/bin/env python3
"""Headless deterministic segment timing of two lma-cli builds, interleaved: the guest-speed
benchmark for a recompiler or runtime change (bit-exactness is regress.py's job).

  tools/seg_time.py [--xex X] [--rounds 3] [--run 20000000000] NAME_A CLI_A NAME_B CLI_B STATE [STATE ...]

Each state is run for `--run` instructions past its clock with LMA_DETERMINISTIC=1 LMA_NORENDER=1,
`--rounds` times per side, sides alternating; prints every run's wall seconds (the end line's time
less the state load's) and the medians. A run that stops before
its end (a panic, a kernel stop) gives no timing and says where it stopped. Quiet machine only.
"""
import os, platform, re, statistics, subprocess, sys, tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import play_gate  # noqa: E402

EXE = '.exe' if platform.system() == 'Windows' else ''
LOW = {'creationflags': 0x4000} if platform.system() == 'Windows' else {}


def clock(cli, xex, state, cwd):
    out = subprocess.run([cli, 'run', xex, '1'], env={**os.environ, 'LMA_LOAD': state, 'LMA_NORENDER': '1', 'LMA_REGS': '0'}, cwd=cwd, capture_output=True, text=True, **LOW)
    m = re.search(r' at (\d+) instructions', out.stdout + out.stderr)
    return int(m.group(1)) if m else None


def run(cli, xex, state, end, cwd, n):
    env = {**os.environ, 'LMA_LOAD': state, 'LMA_DETERMINISTIC': '1', 'LMA_NORENDER': '1', 'LMA_REGS': '0'}
    out = subprocess.run([cli, 'run', xex, str(end)], env=env, cwd=cwd, capture_output=True, text=True, **LOW)
    text = out.stdout + out.stderr
    open(os.path.join(cwd, f'run{n}.log'), 'w').write(text)
    t = seconds(text, end)
    if t is None:
        m = re.findall(r'^(\d+) instructions in', text, re.M)
        print(f'  run {n}: stopped at {m[-1] if m else "?"} of {end} (log {cwd}/run{n}.log)', flush=True)
    return t


def seconds(text, end):
    """The run's own wall seconds, or None when it stopped before `end`. lma-cli's timer starts
    before the state load, so its end line "<clock> instructions in T s" includes the load; the
    load's own "at N instructions in T" line (ms under a second) is subtracted, by the same
    parser the play-build gate uses (play_gate.seconds)."""
    m = re.findall(r'^(\d+) instructions in', text, re.M)
    return play_gate.seconds(text) if m and int(m[-1]) >= end else None


def main():
    a = sys.argv[1:]
    opts = {'rounds': '3', 'run': '20000000000', 'xex': os.path.expanduser('~/lma/LMA_full/default.xex')}
    while a and a[0].startswith('--'):
        opts[a[0][2:]] = a[1]
        a = a[2:]
    if len(a) < 5:
        sys.exit(__doc__)
    sides = [(a[0], a[1]), (a[2], a[3])]
    if a[0] == a[2]:
        sys.exit(f'both sides are named {a[0]}: their timings would land in one list')
    states = [os.path.expanduser(s) for s in a[4:]]
    xex, rounds, length = os.path.expanduser(opts['xex']), int(opts['rounds']), int(opts['run'])
    cwd = tempfile.mkdtemp(prefix='lma-seg-')
    print(f'logs in {cwd}', flush=True)
    n = 0
    for state in states:
        name = os.path.basename(state).replace('.state', '')
        start = clock(sides[0][1], xex, state, cwd)
        if start is None:
            print(f'{name}: the state did not load', flush=True)
            continue
        end = start + length
        res = {s: [] for s, _ in sides}
        for r in range(rounds):
            for sn, cli in (sides if r % 2 == 0 else sides[::-1]):
                n += 1
                res[sn].append(run(cli, xex, state, end, cwd, n))
        if any(t is None for sn, _ in sides for t in res[sn]):
            print(f'{name}: a run failed, no timing', flush=True)
            continue
        med = [statistics.median(res[sn]) for sn, _ in sides]
        print(f'{name}: ' + ' | '.join(f'{sn} {" / ".join(f"{t:.2f}" for t in res[sn])} (median {statistics.median(res[sn]):.2f} s)' for sn, _ in sides) + f' | {100 * (med[1] / med[0] - 1):+.1f}%', flush=True)


if __name__ == '__main__':
    main()
