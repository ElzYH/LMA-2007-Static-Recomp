#!/usr/bin/env python3
"""Profile-guided optimisation build of lma-cli and lma-app (play builds; CI and tests stay plain).

  tools/pgo_build.py [--states DIR] [--xex PATH] [--target-dir DIR] [--cpu NAME] [--lto off|thin|fat] [--features F] [-j N]

Needs `cargo install cargo-pgo` and `rustup component add llvm-tools-preview`. Steps:
  1. instrumented build (cargo pgo build) with -C target-cpu=<cpu> (default: native) and, with
     --lto, link-time optimisation across crates (CARGO_PROFILE_RELEASE_LTO; default off);
  2. training: the cases of <states>/regress.json (tools/regress.py), run deterministically with
     the instrumented lma-cli: each fingerprint case headless, each frames case with rendering on
     (LMA_BACKEND=wgpu, so the render thread's code is trained too), and each "live" case
     ({"state", "seconds"}) in the instrumented lma-app in real time (window loop, pacing, the
     threaded render path);
  3. optimised build (cargo pgo optimize build). Binaries: <target-dir>/<triple>/release/.
Bit-exact like the plain build (check with tools/regress.py); about 20-30% faster headless on
the Mac, 1.69x on the PC's night match (#6). Windows: the MSVC toolchain (RUSTUP_TOOLCHAIN=
stable-x86_64-pc-windows-msvc, Visual Studio Build Tools); the GNU one has no PGO runtime.
"""
import argparse, json, os, platform, re, shutil, subprocess, sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CRATE = os.path.join(REPO, 'lma-rs')


def sh(cmd, env=None, cwd=CRATE, check=True):
    print('+', ' '.join(cmd), flush=True)
    r = subprocess.run(cmd, env={**os.environ, **(env or {})}, cwd=cwd, check=check)
    if r.returncode:
        print(f'pgo_build: exit {r.returncode} (training run; its profile is checked below)', flush=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--states', default=os.path.join(CRATE, 'states'))
    ap.add_argument('--xex', default=os.path.expanduser('~/lma/LMA_full/default.xex'))
    ap.add_argument('--target-dir', default=os.path.join(CRATE, 'target-pgo'))
    ap.add_argument('--cpu', default='native', help='target-cpu (Mac: apple-m4, PC: znver2)')
    ap.add_argument('--lto', default='off', choices=['off', 'thin', 'fat'], help='link-time optimisation (#6 perf plan step 4)')
    ap.add_argument('--features', default='', help='cargo features for both builds, e.g. lma-app/xma-ffmpeg (game sound; needs FFMPEG_DIR)')
    ap.add_argument('-j', default=os.environ.get('CARGO_BUILD_JOBS') or str(os.cpu_count()), help='cargo jobs (default: CARGO_BUILD_JOBS as set by tools/build_queue, else every core)')
    a = ap.parse_args()
    env = {'CARGO_TARGET_DIR': a.target_dir, 'RUSTFLAGS': f'-C target-cpu={a.cpu}', 'CARGO_PROFILE_RELEASE_LTO': 'false' if a.lto == 'off' else a.lto}
    triple = subprocess.run(['rustc', '-vV'], capture_output=True, text=True).stdout
    triple = re.search(r'host: (\S+)', triple).group(1)
    if triple.endswith('windows-gnu'):
        # the GNU toolchain ships no profiler runtime (cargo pgo fails: can't find profiler_builtins)
        sys.exit('pgo_build: the windows-gnu toolchain has no PGO runtime; use MSVC (needs Visual Studio Build Tools):\n'
                 '  rustup toolchain install stable-x86_64-pc-windows-msvc && rustup component add llvm-tools-preview --toolchain stable-x86_64-pc-windows-msvc\n'
                 '  then run this with RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-msvc')
    exe = '.exe' if platform.system() == 'Windows' else ''
    # MSVC + PGO: with some profiles LLVM fails in naga's WGSL lexer ('Missing .seh_endepilogue',
    # an SEH codegen bug); naga at opt-level 1 avoids it and isn't on a hot path (#6)
    extra = ['--config', 'profile.release.package.naga.opt-level=1'] if triple.endswith('windows-msvc') else []
    if extra and a.lto != 'off':
        # LTO re-imports naga's code into the binary and optimises it at full level there, so the
        # bug can come back; on the PC LTO was worth ~2.5% (#6)
        print('pgo_build: warning: --lto on windows-msvc can still hit the naga SEH bug (use --lto off if the optimised build fails)', flush=True)
    cli = os.path.join(a.target_dir, triple, 'release', 'lma-cli' + exe)
    profiles = os.path.join(a.target_dir, 'pgo-profiles')
    shutil.rmtree(profiles, ignore_errors=True)
    # both stages build the same packages with the same LTO: cargo's feature unification and the
    # profile settings go into the crates' metadata, so into the symbol names, and a profile whose
    # names don't match is silently ignored (a "PGO" build at plain speed)
    ienv = env
    # macOS: an instrumented binary linked against Homebrew's FFmpeg dylibs (xma-ffmpeg) segfaults
    # in the profile writer at exit, even for --help, and leaves a corrupt profile (8 Oct). Train
    # without it there: only the small XMA shim goes unprofiled; the optimised build has sound.
    train_features = a.features
    if platform.system() == 'Darwin' and 'xma-ffmpeg' in a.features:
        train_features = ','.join(f for f in a.features.split(',') if 'xma-ffmpeg' not in f)
        print('pgo_build: training without xma-ffmpeg (its link breaks the profile runtime on macOS)', flush=True)
    sh(['cargo', 'pgo', 'build', '--', '-j', a.j, '-p', 'lma-cli', '-p', 'lma-app'] + (['--features', train_features] if train_features else []) + extra, ienv)
    cfg = json.load(open(os.path.join(a.states, 'regress.json')))
    run_dir = os.path.join(a.target_dir, 'pgo-train')
    os.makedirs(run_dir, exist_ok=True)
    for case in cfg.get('fingerprints', []):
        e = {'LMA_DETERMINISTIC': '1', 'LMA_NORENDER': '1', 'LMA_REGS': '0', 'LMA_LOAD': os.path.join(a.states, case['state'])}
        if case.get('input'):
            e['LMA_INPUT'] = case['input']
        sh([cli, 'run', a.xex, str(case['end'])], e, run_dir, check=False)
    for case in cfg.get('frames', []):
        state = os.path.join(a.states, case['state'])
        text = subprocess.run([cli, 'run', a.xex, '1'], env={**os.environ, 'LMA_NORENDER': '1', 'LMA_LOAD': state}, cwd=run_dir, capture_output=True, text=True)
        clock = int(re.search(r' at (\d+) instructions', text.stdout + text.stderr).group(1))
        e = {'LMA_DETERMINISTIC': '1', 'LMA_BACKEND': 'wgpu', 'LMA_RENDER_EVERY': '1', 'LMA_REGS': '0', 'LMA_LOAD': state}
        if 'xma-ffmpeg' in train_features:
            e['LMA_AUDIO'] = '1'  # with sound built in, train the audio code too
        sh([cli, 'run', a.xex, str(clock + case.get('run', 7_000_000_000))], e, run_dir, check=False)
    # the live game: lma-app in real time on each state of the config's "live" list for its
    # `seconds` (default 60). Deterministic runs never reach the window loop, real-time pacing or
    # the threaded render path's asynchronous fences, so PGO laid that code out as cold.
    app = os.path.join(a.target_dir, triple, 'release', 'lma-app' + exe)
    for case in cfg.get('live', []):
        e = {**os.environ, 'LMA_LOAD': os.path.join(a.states, case['state']), 'LMA_INPUT_LIVE': '1', 'LMA_NO_LAUNCHER': '1', 'LMA_PIPELINE_CACHE': '', 'LMA_QUIT_ATEXIT': '1'}
        if 'xma-ffmpeg' in train_features:
            e.update({'LMA_AUDIO': '1', 'LMA_AUDIO_MUTE': '1'})
        secs = case.get('seconds', 60)
        print(f'+ {app} (live, {secs} s)', flush=True)
        # `wait` counts M ticks of the guest timebase: 50 a second in real time
        try:
            r = subprocess.run([app, a.xex], input=f'wait {int(secs * 50)}\nquit\n', text=True, env=e, cwd=run_dir, capture_output=True, timeout=secs + 300)
            code = r.returncode
        except subprocess.TimeoutExpired:
            code = 'timeout'  # killed: it writes no profile, the others still count
        if code:
            print(f'pgo_build: lma-app exit {code} (live training; its profile is checked below)', flush=True)
    # each profile must read back: one corrupt file makes the optimised build fail after the
    # whole training
    sysroot = subprocess.run(['rustc', '--print', 'sysroot'], capture_output=True, text=True).stdout.strip()
    profdata = os.path.join(sysroot, 'lib', 'rustlib', triple, 'bin', 'llvm-profdata' + exe)
    good = 0
    for f in sorted(os.listdir(profiles)):
        if f.endswith('.profraw'):
            ok = subprocess.run([profdata, 'show', os.path.join(profiles, f)], capture_output=True).returncode == 0
            print(f"pgo_build: profile {f}: {'ok' if ok else 'corrupt, removed'}", flush=True)
            good += ok
            if not ok:
                os.remove(os.path.join(profiles, f))
    if not good:
        sys.exit(f'pgo_build: no usable profile in {profiles}')
    # one file per binary: with live cases lma-app's must be there too, or its training was lost
    # (#360: a quit through ExitProcess wrote none, and the build looked fine)
    if cfg.get('live') and good < 2:
        sys.exit(f'pgo_build: the live lma-app runs wrote no profile in {profiles} (only {good} file)')
    # no compiler cache here: sccache keys on -Cprofile-use's path, not the profile's contents, so
    # it hands back objects optimised with an older profile (or none): a "PGO" build at plain
    # speed, built in minutes (Mac, 8 Oct)
    oenv = {**env, 'RUSTC_WRAPPER': ''}
    sh(['cargo', 'pgo', 'optimize', 'build', '--', '-j', a.j, '-p', 'lma-cli', '-p', 'lma-app'] + (['--features', a.features] if a.features else []) + extra, oenv)
    print(f'PGO binaries in {os.path.dirname(cli)}')


if __name__ == '__main__':
    main()
