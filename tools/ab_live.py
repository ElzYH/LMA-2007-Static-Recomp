#!/usr/bin/env python3
"""Live A/B of two builds (or one build with two environments) on a save state, as the player sees
it: lma-app with skip off, cap 60, sound on but muted, no launcher; N interleaved rounds of `secs`;
per run the median fps as seen and the median 1% low of the status windows after the first 10 s;
then the median of run medians per side.

  tools/ab_live.py --state S [--xex X] [--rounds 3] [--secs 75] NAME_A DIR_A [ENV=V ...] -- NAME_B DIR_B [ENV=V ...]

DIR is a build's binaries folder (the one holding lma-app). Only compare on a quiet machine: no
CI job, no other game, no build (the Mac pauses its runner; the PC runs lma-app at Below Normal).
"""
import os, platform, re, statistics, subprocess, sys, tempfile

ANSI = re.compile(r'\x1b\[[0-9;]*m')
EXE = '.exe' if platform.system() == 'Windows' else ''
LOW = {'creationflags': 0x4000} if platform.system() == 'Windows' else {}


def side(args):
    return args[0], args[1], dict(kv.split('=', 1) for kv in args[2:])


def live(build, env, xex, state, secs, cwd):
    e = {**os.environ, 'LMA_INPUT_LIVE': '1', 'LMA_NO_LAUNCHER': '1', 'LMA_LOAD': state, 'LMA_RENDER_EVERY': '1',
         'LMA_MAX_SKIP': '1', 'LMA_FPS_CAP': '60', 'LMA_AUDIO': '1', 'LMA_AUDIO_MUTE': '1', **env}
    script = f'wait {secs * 50}\nquit\n'  # `wait` counts M ticks of the guest timebase: 50 a second
    try:
        out = subprocess.run([os.path.join(build, 'lma-app' + EXE), xex], input=script, env=e, cwd=cwd, capture_output=True, text=True, timeout=secs + 240, **LOW)
        text = ANSI.sub('', out.stdout + out.stderr)
    except subprocess.TimeoutExpired as ex:
        text = ANSI.sub('', ex.stdout if isinstance(ex.stdout, str) else (ex.stdout or b'').decode(errors='replace'))
    return summarise(text)


def summarise(text):
    """(median fps as seen, median 1% low) of the status windows after the first 10 s, or None for
    a panic or no status lines."""
    if 'panicked' in text:
        return None
    pairs = [(int(f), int(l)) for s, f, l in re.findall(r'(\d+)s: (\d+) fps \(1% low (\d+)\)', text) if int(s) >= 10]
    return (statistics.median(f for f, _ in pairs), statistics.median(l for _, l in pairs)) if pairs else None


def main():
    a = sys.argv[1:]
    opts = {'rounds': '3', 'secs': '75', 'xex': os.path.expanduser('~/lma/LMA_full/default.xex')}
    while a and a[0].startswith('--'):
        opts[a[0][2:]] = a[1]
        a = a[2:]
    if 'state' not in opts or '--' not in a:
        sys.exit(__doc__)
    i = a.index('--')
    sides = [side(a[:i]), side(a[i + 1:])]
    state, xex = os.path.expanduser(opts['state']), os.path.expanduser(opts['xex'])
    rounds, secs = int(opts['rounds']), int(opts['secs'])
    cwd = tempfile.mkdtemp(prefix='lma-ab-')
    res = {s[0]: [] for s in sides}
    for r in range(rounds):
        for name, build, env in (sides if r % 2 == 0 else sides[::-1]):
            got = live(build, env, xex, state, secs, cwd)
            res[name].append(got)
            print(f'  r{r + 1} {name}: {"no status lines / panic" if got is None else f"{got[0]} fps (1% low {got[1]})"}', flush=True)
    print()
    for name, _, _ in sides:
        runs = [x for x in res[name] if x]
        if runs:
            print(f'{name}: fps {" / ".join(str(x[0]) for x in runs)} median {statistics.median(x[0] for x in runs)}; '
                  f'1% low {" / ".join(str(x[1]) for x in runs)} median {statistics.median(x[1] for x in runs)}')
        else:
            print(f'{name}: no runs')


if __name__ == '__main__':
    main()
