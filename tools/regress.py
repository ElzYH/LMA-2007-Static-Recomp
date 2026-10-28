#!/usr/bin/env python3
"""Local regression suite (needs your own game files and states; nothing here is game content).

  tools/regress.py [--cli PATH] [--ref PATH] [--states DIR] [--xex PATH] [--update] [--only NAME,...]
                   [--long] [--roundtrip]
  --only takes case names (match_night) and check kinds (fingerprints, prefilter, frames, replays).

Checks, each PASS/FAIL:
  fingerprints  deterministic headless runs whose guest memory, registers and kernel-call log
                must match the hashes stored in <states>/regress.json (bit-exact guest code).
  prefilter     each fingerprint case again with LMA_SEARCH_PREFILTER=check: the game decides every
                AI transfer-search call and any yes the native pre-filter would have refused is a
                MISMATCH (#646); none may appear.
  roundtrip     (--roundtrip) each fingerprint case again, saved halfway (LMA_SAVE) and resumed
                from that state: guest memory and registers must match the straight run, so a
                save state restores everything that decides what the game does next.
  frames        deterministic runs with rendering on: frames dumped from each match state must
                be byte-identical to the same frames from --ref (another lma-cli, e.g. the play
                build), so render changes that should not change pixels are proven not to.
  replays       (--long; can take an hour or more) recorded play sessions replayed in order on the
                build that recorded them, from a saved starting user folder; files the game
                saved (e.g. its career save) must come out byte-identical to the originals.
                A case with "exact": true must also play each journal out with 0 divergences
                (a recording with no saved game files to compare passes on that alone).
<states>/regress.json lists the cases (see --update to record the current hashes as the
baseline after an intended change):
  {"fingerprints": [{"name": "day", "state": "career_adv4.state", "input": "...", "end": N,
                     "peek": "...", "hash": "...", "kernel": "..."}, ...],
   "frames": [{"name": "match_night", "state": "match_night_5a4cbfb.state", "frames": 9,
               "every": 30, "run": 7000000000}, ...],
   "replays": [{"name": "sessions4-5", "cli": "~/lma/play/builds/550c491/lma-cli",
                "userdata": "~/lma/recordings/start/session4.userdata", "content_idx": "1\tSG01\t\n",
                "steps": [["~/lma/recordings/session4.rec", 1919314713541], ...],
                "compare": [["content/SG02/Career", "~/lma/play/userdata/content/SG02/Career"]]}]}
Exit status 1 if anything fails.
"""
import argparse, hashlib, json, os, re, shutil, subprocess, sys, tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run(cli, xex, env, end, cwd):
    out = subprocess.run([cli, 'run', xex, str(end)], env={**os.environ, **env}, cwd=cwd, capture_output=True, text=True)
    return out.stdout + out.stderr


def state_info(cli, xex, state, cwd):
    """(instruction clock, guest frame counter) of a state."""
    text = run(cli, xex, {'LMA_NORENDER': '1', 'LMA_LOAD': state, 'LMA_REGS': '0'}, 1, cwd)
    clock = int(re.search(r' at (\d+) instructions', text).group(1))
    text = run(cli, xex, {'LMA_NORENDER': '1', 'LMA_LOAD': state, 'LMA_REGS': '0'}, clock + 1, cwd)
    frames = int(re.search(r'Stats \{.* frames: (\d+),', text).group(1))
    return clock, frames


def fingerprint(cli, xex, case, states, cwd, load=None, save=None, end=None):
    env = {'LMA_DETERMINISTIC': '1', 'LMA_NORENDER': '1', 'LMA_REGS': '0', 'LMA_LOAD': load or os.path.join(states, case['state']), 'LMA_PEEK': case['peek']}
    if case.get('input'):
        env['LMA_INPUT'] = case['input']
    if save:
        env['LMA_SAVE'] = save
    text = run(cli, xex, env, end or case['end'], cwd)
    lines = [l for l in text.splitlines() if re.match(r'^  (0x|r|lr|v)', l)]
    h = hashlib.sha1(('\n'.join(lines) + '\n').encode()).hexdigest()[:12]
    log = os.path.join(cwd, 'last_run.log')
    k = hashlib.sha1(open(log, 'rb').read()).hexdigest()[:12] if os.path.exists(log) else '-'
    return h, k


def prefilter(cli, xex, case, states, cwd):
    """(mismatches, calls, answered natively) of the case run in the pre-filter's check mode."""
    env = {'LMA_DETERMINISTIC': '1', 'LMA_NORENDER': '1', 'LMA_REGS': '0', 'LMA_LOAD': os.path.join(states, case['state']),
           'LMA_SEARCH_PREFILTER': 'check', 'LMA_SEARCH_LOG': '1'}
    if case.get('input'):
        env['LMA_INPUT'] = case['input']
    text = run(cli, xex, env, case['end'], cwd)
    counts = re.findall(r'search filter: (\d+) calls, (\d+) answered natively', text)
    calls, native = map(int, counts[-1]) if counts else (0, 0)
    return text.count('search filter MISMATCH'), calls, native


def frames(cli, xex, case, states, cwd, frame_list, end):
    out = os.path.join(cwd, 'frames')
    shutil.rmtree(out, ignore_errors=True)
    env = {'LMA_DETERMINISTIC': '1', 'LMA_BACKEND': 'wgpu', 'LMA_RENDER_EVERY': '1', 'LMA_REGS': '0',
           'LMA_LOAD': os.path.join(states, case['state']), 'LMA_FRAMES': ','.join(map(str, frame_list))}
    run(cli, xex, env, end, cwd)
    return {f: open(os.path.join(out, f), 'rb').read() for f in sorted(os.listdir(out))} if os.path.isdir(out) else {}


def replay_exact(report):
    """Whether a replay's last report line says the journal played out with no divergences."""
    m = re.search(r'replay: finished at \d+ instructions, (\d+) divergences', report or '')
    return m is not None and int(m.group(1)) == 0


def replay_chain(case, xex, tmp):
    """Replay `steps` in order in one user folder; (identical files, differing files, each step's
    last replay report)."""
    ex = os.path.expanduser
    work = os.path.join(tmp, 'replay-' + case['name'])
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    shutil.copytree(ex(case['userdata']), os.path.join(work, 'userdata'))
    if 'content_idx' in case:
        open(os.path.join(work, 'userdata', 'content.idx'), 'w').write(case['content_idx'])
    reports = []
    for rec, end in case['steps']:
        env = {'LMA_BACKEND': 'wgpu', 'LMA_REGS': '0', 'LMA_REPLAY': ex(rec)}
        text = run(ex(case['cli']), xex, env, end, work)
        report = (re.findall(r'replay: (?:finished at \d+ instructions, )?\d+ divergences[^\n]*', text) or ['no replay report'])[-1]
        reports.append(report)
        print(f"  {os.path.basename(rec)}: {report}")
    same, diff = [], []
    for got, want in case['compare']:
        g, w = os.path.join(work, 'userdata', got), ex(want)
        (same if os.path.exists(g) and open(g, 'rb').read() == open(w, 'rb').read() else diff).append(got)
    return same, diff, reports


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--cli', default=os.path.join(REPO, 'lma-rs/target/release/lma-cli'))
    ap.add_argument('--ref', help='reference lma-cli for the frame comparison')
    ap.add_argument('--states', default=os.path.join(REPO, 'lma-rs/states'))
    ap.add_argument('--xex', default=os.path.expanduser('~/lma/LMA_full/default.xex'))
    ap.add_argument('--update', action='store_true', help='store the current fingerprints as the baseline')
    ap.add_argument('--only', help='comma-separated case names and/or check kinds (fingerprints, frames, replays)')
    ap.add_argument('--long', action='store_true', help='also replay recorded sessions (slow)')
    ap.add_argument('--roundtrip', action='store_true', help='also run each fingerprint case saved halfway and resumed')
    a = ap.parse_args()
    cfg_path = os.path.join(a.states, 'regress.json')
    cfg = json.load(open(cfg_path))
    only = set(a.only.split(',')) if a.only else None
    # --only takes case names (match_night) and check kinds (fingerprints, frames, replays)
    kind_of = {c['name']: k for k in ('fingerprints', 'frames', 'replays') for c in cfg.get(k, [])}
    skipped = lambda case: bool(only) and case['name'] not in only and kind_of.get(case['name']) not in only
    # the pre-filter check runs on the fingerprint cases: by their names, 'fingerprints' or 'prefilter'
    pre_skipped = lambda case: bool(only) and case['name'] not in only and not {'fingerprints', 'prefilter'} & only
    failed = 0
    with tempfile.TemporaryDirectory(prefix='lma-regress-') as tmp:
        for case in cfg.get('fingerprints', []):
            if not a.update and not pre_skipped(case):
                bad, calls, native = prefilter(a.cli, a.xex, case, a.states, tmp)
                failed += bad > 0
                verdict = 'FAIL' if bad else ('PASS' if calls else 'skip')
                print(f"{verdict} prefilter {case['name']}: {bad} mismatches in {calls} search calls checked ({native} answerable natively)")
            if skipped(case) or (only and only <= {'prefilter'}):
                continue
            h, k = fingerprint(a.cli, a.xex, case, a.states, tmp)
            if a.update:
                case['hash'], case['kernel'] = h, k
                print(f"fingerprint {case['name']}: recorded {h} / {k}")
                continue
            ok = (h, k) == (case.get('hash'), case.get('kernel'))
            failed += not ok
            print(f"{'PASS' if ok else 'FAIL'} fingerprint {case['name']}: {h} / {k} (baseline {case.get('hash')} / {case.get('kernel')})")
            if a.roundtrip:
                # the kernel-call log of the resumed half differs by construction; memory and registers must not
                state = case['state'] if os.path.isabs(case['state']) else os.path.join(a.states, case['state'])
                start, _ = state_info(a.cli, a.xex, state, tmp)
                mid = start + (case['end'] - start) // 2
                midstate = os.path.join(tmp, 'roundtrip.state')
                fingerprint(a.cli, a.xex, case, a.states, tmp, save=midstate, end=mid)
                h2, _ = fingerprint(a.cli, a.xex, case, a.states, tmp, load=midstate)
                ok = h2 == h
                failed += not ok
                print(f"{'PASS' if ok else 'FAIL'} roundtrip {case['name']}: saved at {mid}, resumed: {h2} (straight run {h})")
        for case in cfg.get('frames', []):
            if skipped(case):
                continue
            if not a.ref:
                print(f"skip frames {case['name']}: no --ref")
                continue
            state = os.path.join(a.states, case['state'])
            clock, f0 = state_info(a.cli, a.xex, state, tmp)
            every, n = case.get('every', 30), case.get('frames', 9)
            frame_list = [f0 + every * (i + 1) for i in range(n)]
            end = clock + case.get('run', 7_000_000_000)
            mine = frames(a.cli, a.xex, case, a.states, os.path.join(tmp), frame_list, end)
            os.makedirs(os.path.join(tmp, 'ref'), exist_ok=True)
            theirs = frames(a.ref, a.xex, case, a.states, os.path.join(tmp, 'ref'), frame_list, end)
            same = sum(1 for f in theirs if mine.get(f) == theirs[f])
            ok = len(theirs) > 0 and same == len(theirs) == len(mine)
            failed += not ok
            print(f"{'PASS' if ok else 'FAIL'} frames {case['name']}: {same}/{len(theirs)} identical to --ref ({len(mine)} dumped)")
        for case in cfg.get('replays', []) if a.long else []:
            if skipped(case):
                continue
            print(f"replaying {case['name']} ({len(case['steps'])} sessions)...", flush=True)
            same, diff, reports = replay_chain(case, a.xex, tmp)
            ok = not diff and len(same) == len(case.get('compare', []))
            # "exact": every step plays its journal out with 0 divergences (a recording with no
            # saved game files to compare, e.g. goal1, session6)
            exact = all(replay_exact(r) for r in reports) if case.get('exact') else None
            ok = ok and exact is not False
            failed += not ok
            print(f"{'PASS' if ok else 'FAIL'} replays {case['name']}: {len(same)} file(s) byte-identical" + (f", differ: {', '.join(diff)}" if diff else '')
                  + ('' if exact is None else f", journal {'played out with 0 divergences' if exact else 'NOT exact: ' + reports[-1]}"))
    if a.update:
        json.dump(cfg, open(cfg_path, 'w'), indent=1)
        print(f"baseline written to {cfg_path}")
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
