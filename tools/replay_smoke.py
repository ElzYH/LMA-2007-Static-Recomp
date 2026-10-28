#!/usr/bin/env python3
"""Replay smoke test for LMA_RECORD sessions: replay each recording from its start state in
lma-app, take a screenshot every N seconds, and fail if the game stops or the app panics.
Recordings with a real-time journal (REC.journal, #108) replay exactly (LMA_REPLAY); older ones
replay their presses in real time (LMA_INPUT), which drifts from the original session, so for
those this only checks "plays through without a crash"; compare screenshots against the video.

  tools/replay_smoke.py REC [REC ...] [--every S] [--extra S] [--out DIR] [--app PATH] [--xex PATH]

REC is a recording (`LMA_RECORD=path`); its start state is REC.state. Each run lasts as long as
the recording plus --extra seconds (default 60). Screenshots go to DIR/<name>/ (default: a
folder under the system temp dir, never the repo). Exit status 1 if any replay failed.
"""
import argparse, os, queue, subprocess, sys, tempfile, threading, time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def span_seconds(rec):
    """Seconds between the first and last recorded event (1 M tick = 20 ms of guest time)."""
    ats = []
    for line in open(rec):
        line = line.split('#', 1)[0].strip()
        if line:
            ats.append(int(line.split(':')[0]))
    return (max(ats) - min(ats)) * 0.02 if ats else 0.0


def replay(rec, args):
    name = os.path.splitext(os.path.basename(rec))[0]
    out = os.path.join(args.out, name)
    os.makedirs(out, exist_ok=True)
    state = rec + '.state'
    if not os.path.exists(state):
        print(f'{name}: FAIL (no start state {state})')
        return False
    seconds = span_seconds(rec) + args.extra
    if os.path.exists(rec + '.journal'):
        # recorded with a real-time journal (#108): an exact replay
        env = {**os.environ, 'LMA_REPLAY': rec, 'LMA_INPUT_LIVE': '1'}
        env.pop('LMA_LOAD', None)
        env.pop('LMA_INPUT', None)
    else:
        env = {**os.environ, 'LMA_LOAD': state, 'LMA_INPUT': '@' + rec, 'LMA_INPUT_LIVE': '1'}
    env.pop('LMA_DETERMINISTIC', None)
    env.pop('LMA_RECORD', None)
    log = open(os.path.join(out, 'app.log'), 'w')
    app = subprocess.Popen([args.app, args.xex], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                           stderr=log, text=True, bufsize=1)
    lines = queue.Queue()
    finished = []  # lma-app's "replay finished ..." line (journal replays)

    def reader():
        for l in app.stdout:
            if 'replay finished' in l:
                finished.append(l.strip())
            else:
                lines.put(l)
    threading.Thread(target=reader, daemon=True).start()
    journal = 'LMA_REPLAY' in env

    def cmd(c, timeout=60):
        app.stdin.write(c + '\n')
        app.stdin.flush()
        end = time.time() + timeout
        while time.time() < end:
            try:
                l = lines.get(timeout=1)
            except queue.Empty:
                if app.poll() is not None:
                    return 'exited'
                continue
            if 'game stopped' in l:
                return l.strip()
            if l.startswith(('lma: ok', 'lma: error')) or (c == 'status' and l.startswith('lma: ')):
                return l.strip()
        return 'timeout'

    t0, shots, failure, status = time.time(), 0, None, ''
    end = None  # journal replays: when to stop, once it is played out
    while True:
        if journal:
            # until the journal is played out (at most --timeout), then --extra seconds more
            if finished and end is None:
                end = time.time() + args.extra
            if time.time() > (end if end is not None else t0 + args.timeout):
                break
        elif time.time() - t0 >= seconds:
            break
        time.sleep(min(args.every, max(0.0, seconds - (time.time() - t0))) if not journal else args.every)
        r = cmd(f'screenshot {os.path.join(out, f"{shots:03}_{int(time.time() - t0):04}s.png")}')
        if not r.startswith('lma: ok'):
            failure = r
            break
        shots += 1
    if failure is None:
        status = cmd('status')
        cmd('quit')
    try:
        app.wait(timeout=30)
    except subprocess.TimeoutExpired:
        app.kill()
    log.close()
    text = open(os.path.join(out, 'app.log'), errors='replace').read()
    if failure is None and 'panicked' in text:
        failure = 'panic (see app.log)'
    if failure is None and journal:
        if not finished:
            failure = f'journal not played out in {args.timeout:.0f} s'
        elif ' 0 divergences' not in finished[0]:
            failure = finished[0]
    verdict = 'ok' if failure is None else f'FAIL ({failure})'
    print(f'{name}: {verdict}: {time.time() - t0:.0f} s, {shots} screenshots in {out}')
    if finished:
        print(f'  {finished[0]}')
    if status:
        print(f'  {status[:110]}')
    return failure is None


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('recs', nargs='+')
    p.add_argument('--every', type=float, default=30.0, help='seconds between screenshots (30)')
    p.add_argument('--extra', type=float, default=60.0, help='seconds to keep running after the last input (60)')
    p.add_argument('--timeout', type=float, default=3600.0, help='journal replays: give up after this many seconds (3600)')
    p.add_argument('--out', default=os.path.join(tempfile.gettempdir(), 'lma-replay'))
    p.add_argument('--app', default=os.path.join(REPO, 'lma-rs', 'target', 'release', 'lma-app'))
    p.add_argument('--xex', default=os.path.expanduser('~/lma/LMA_full/default.xex'))
    args = p.parse_args()
    ok = [replay(os.path.abspath(r), args) for r in args.recs]
    sys.exit(0 if all(ok) else 1)


if __name__ == '__main__':
    main()
