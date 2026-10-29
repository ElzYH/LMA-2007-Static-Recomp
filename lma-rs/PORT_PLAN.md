# LMA Manager 2007 – native Rust port (recompilation, Skate 3 style)

Goal: run LMA Manager 2007 (Xbox 360, title 434D07D2) natively on Windows, macOS (Intel and
Apple Silicon/ARM) and Linux,
built from the user's own disc files, with the database fully moddable (new teams, modern squads).

The Skate 3 port this mirrors is ReXGlue: XenonRecomp turns the PowerPC code into C++
(`skate3.exe`, 100 MB) and a Xenia-derived runtime (`rexruntime.dll`) supplies the Xbox
kernel, GPU and audio. Here the same layers are written in Rust; Bevy hosts the window, input,
audio output and the mod tools, and the GPU layer renders through wgpu (Bevy's renderer).

Nothing from the game is committed: every tool reads the user's `default.xex` and data files.

## Layers

| crate | job | status |
|---|---|---|
| `lma-data` | Export.mng database: players, clubs, squads, stadiums, managers, names | done, byte-identical round trip, edits verified in game |
| `xex` | XEX2 loader: AES decrypt, basic decompression, sections, imports | done |
| `ppc` | Xenon decoder (generated from Xenia's table, 455 instructions) | done |
| `runtime::cpu/vmx` | reference interpreter, scalar + AltiVec + VMX128 | runs the game for billions of instructions |
| `runtime::machine/threads` | kernel/XAM HLE, threads, events, critical sections, VFS | 9 game threads running |
| `runtime::gpu` | PM4 command processor: ring, IBs, predication/tiling, fences, interrupts | full render loop, thousands of frames |
| `runtime::shader*` | Xenos microcode decoder, disassembler, interpreter | vertex + pixel shaders execute |
| `runtime::raster` | software rasteriser, EDRAM, resolves, textures (8888, DXT, ...) | **renders real frames**; multithreaded pixels, cached textures (~4x faster) |
| `lma-app` | Bevy window: live frames, keyboard + mouse + gamepad input, frame skip | **boots to the main menu; keyboard drives menus** (title → main menu → exhibition → team select) |
| `recomp` + `runtime::rec` | PowerPC → Rust: every function (+ gaps outside .pdata) → one Rust fn, split into 32 crates | **done**: whole game, 95 % native, 221 M instr/s (5x interpreter), call sequences identical |
| `gpu (wgpu)` | Xenos shaders → WGSL on the GPU | next (speed) |

## Current status (2026-10-05)

Playable on the Mac (M4 Max, wgpu on Metal) and the PC. Verified by a player-like driver and by
the maintainer's own sessions:
- **Career:** new game, inbox, transfers, calendar, day updates, Football One, matches simulated
  or watched in 3D, cups and the European Cup. Two seasons played with no crash: Arsenal from
  20 Aug 06 through the end of season (manager performance, sacking, job offers, high scores),
  then Reading through skip pre-season, squad numbers and 2007/08 to Feb 08.
- **Matches:** loading takes ~6 s, then the game's own intro (kick-off ~26 s after "watch
  match"); a full watched match plays to the end. Real time on the Mac: ~44 fps guest, menus 60.
- **Exhibition match**, title menu, and **saves:** in-game save/overwrite/new slot, Load Game
  (#122: each package in `userdata/content/<package>`, listed in `userdata/content.idx`).
- Not yet: attract video (XMV), audio (XMA), Xbox Live features.

Speed: the guest runs ~650-800 M instr/s natively (98.6% recompiled); a live match needs
~21.5 M instructions per frame. On the Mac the match is mostly guest-bound (see #61 for the
recompiler register-caching proposal); on the PC it is bound by GPU fence round trips (fence
waits per frame x readback latency). Rendering runs on its own thread in real time (#95).
Benchmark (Mac, 9 Oct): the night segment (20 G from match_night_5a4cbfb, headless, deterministic)
went from 16.83 s on 8 Oct to ~11.6 s (#318, #331, #332, #334), and the day segment (career_adv4
to 110 G, one day's update) from 7.35 to 5.99 s. Live, the maintainer's night match is CPU-bound on both
threads (guest ~13-14 ms, render thread ~14-16 ms a frame, GPU waits ~0). `LMA_GPU_TIMING`'s
pass times include waiting and add up to the whole frame: they are not GPU cost.

Frame pacing in matches (#6, 6 Oct): the game is vsync'd like the console. After a Swap the
command processor waits (`WAIT_REG_MEM` on 0x0a40b004 until 0) for the flip, which D3D's vblank
handler completes, and D3D's present path blocks the game (block-on-fence at lr 0x8233fa18,
polling 0x82339080) until then. So a frame whose game-thread work (guest code + command
processor) misses 16.7 ms waits for the next vblank: frame times are 16.7 or 33.3 ms, and the
"spin" in profiles is that wait, not removable work. 60 fps with good 1% lows needs nearly every
frame's game-thread work under 16.7 ms, and the render thread and GPU keeping up. Live (non-PGO,
night match): guest ~11-14 ms + command processor ~3 ms a frame; render thread busy 15-17 ms.

Command-processor thread (`LMA_CP_THREAD=1`, `mac/cp-thread`, live only; deterministic runs and
recordings keep it inline): `Gpu::pump` runs on its own thread over a view of guest memory
(`cp_thread::CpView`) that queues the run's writes; the game thread starts a run at a slice
boundary, collects it at the first boundary after it finished (applying the writes in order,
then vblank and interrupt delivery), and skips GPU work at boundaries while a run is going. The
game sees the inline order, a few slices later. Gate before it's the default (the maintainer): match
frame comparisons vs inline, a watched full match, a week of day updates, the season regression.

Driving a live window from the shell (macOS, `tools/live_drive.sh`): the game's stdin is a FIFO kept
open by a sleeping writer, so `live_drive.sh cmd "press A"` works one call at a time, and `cap NAME`
queues the app's own `screenshot` and OCRs the file (a live `wait N` is in M ticks of the 50 MHz
timebase, 1 M = 20 ms, so a long wait before it looks like a stall: write `wait 25s`). Routes that worked: new game > one player > database > full game > difficulty >
staff > highlights > select team (DOWN DOWN, LEFT/RIGHT to the club) > A x3 > full season > ~20 s > hub;
Information > League Table > Y > A = club details (stadium, manager); SQUAD > RT = Team Tactics.
From the hub (LAPTOP): RB x2 = TRANSFER MARKET > Player Search; Y moves focus to the left list,
DOWN x7 = Perform Search Criteria, A runs it (every player, sorted by Overall); Y again focuses the
results and each row's card shows Valuation, Wage, Time at Club and Contract Expiry. LB/RB change
category, LT/RT the sub-screen (FINANCE > RT x4 = Player Contracts; RECORDS > RT x3 = League
Champions; LT/RT exist in scripts since #309, so a main build, not the stable 4aaeaa6 play build). Team select starts on a random club in a fresh profile: step with RIGHT and read
the row (`tools/live_drive.sh cap`) rather than counting presses.

The black box (lma-app, the maintainer 29 Oct): every `LMA_BLACKBOX=<min>` minutes (default 3, 0 = off) a state saved in the background (~0.2 s on the game thread) and the pad's presses from there go to `reports/.blackbox/` (the last two kept); a bug report (F12 / Ctrl+B) gets them as `before.state` + `before.input` (replay: `LMA_LOAD=before.state LMA_INPUT=@before.input`) and `events.txt` (`runtime::events`: every sound started, by bank/cue name). A sound started 6 times in 10 s writes a report by itself, once a session. lma-cli: `LMA_EVENTS=<file>` writes the event log at exit, `LMA_SAVE_BG=1` saves `LMA_SAVE` the black box's way.

Scripted headless testing: `LMA_INPUT="4900:10:100,5300:2:8"` (at M instr : hex buttons : length
M instr, and an optional `:port` 1-4 for another controller, lma-cli only, #693), `LMA_FRAMES=4400` dumps frames, `LMA_RENDER_EVERY=10` skips frames. `LMA_SKIP_LAND=<hex dest,...>|all|0` keeps those resolves on the GPU with no read-back into
guest memory (#864: identical frames on 12 states through a whole match, fingerprints unchanged, no fps change: the GPU cost is
the resolve passes themselves). lma-cli skips all by default (`0` lands them again); lma-app lands all until tested live. `LMA_PRESENT_LOG=1` logs each software present (the one reader of landed front-buffer bytes),
`LMA_RESOLVE_USE=1` prints per present how many of last frame's resolves a draw sampled (a match: 19 of 45; the other 26
are the frame's own tile bands, composed by the GPU present). `LMA_VBLANK_HZ=120` sets the vblank rate (#863: the match loop follows it; the sim steps by the timebase),
`LMA_FRAME_GAPS=1` prints the three longest swap intervals with their position, `LMA_PRESENT_ALL=1` makes lma-cli present every frame as
lma-app does (the threaded backend throttles the game per frame only there).
In the live window `LMA_FRAME_DIR=<dir>` writes every presented frame at a quarter size
(`<guest frame>.png`), to find one-frame flashes (#205): `tools/flash_detect.py <dir>` lists
frames where the players turn darker or brighter for exactly one frame.

The app's icon is LMA's own, the title icon in the player's default.xex (`xex::Xex::title_icon_png`,
read at run time: window icon, and the Dock icon on macOS; `lma-cli xex-icon <xex> <out.png>` for a
local bundle's .icns; #841). While the window can't be seen (minimised, covered, another Space) the
game and its sound run on but nothing is drawn, and Bevy's loop runs 4 times a second (#841:
drawing is ~65% of a match's CPU).

Profiling (#6): `LMA_FRAME_PROFILE=1 lma-app ...` logs every 2 s where each guest frame's wall
time goes: game thread (guest code, kernel, command processor and its waits, sleep), render
thread (busy, GPU waits, idle), and how long GPU interrupts hold the command processor.
`LMA_GPU_TIMING=1` adds GPU time per pass category. The profile line also breaks the render thread's
draw path down (CPU transform, ps setup, vwin, consts, record; setup misses / refreshed / re-validated /
kept by the write watch); `LMA_ENCODE_LOG=1` prints encode_batch's phases every 300th batch and
`LMA_SETUP_LOG=1` names the pixel shaders that miss the setup cache, why a hit was invalid, the fast
path's reasons and the resolve reuse counts (#549, #561).
A guest frame slower than `LMA_SLOW_MS` (default 30) also gets a slow-frame line (draws, setup
misses, pipeline and texture time); `LMA_TEX_STATS=1` adds the texture decodes per format since
the last one: count, texels and time (#304).
On Windows, `tools/wprof.py <pid> render|"command processor"|@busiest` samples one thread of a
running lma-app or lma-cli with stack walks (dbghelp; no installs): self time, each leaf's first
caller in our code, and inclusive time. Build MSVC with debuginfo and `-C link-arg=/OPT:NOICF`
for clean names (PGO play builds fold identical functions under one name). `--lines REGEX` splits
the time in matching functions by source line (inlined code included; deeper leaves count at their
call site), e.g. `--lines 'draw_gpu_vs$|::put_consts$'` (#333).

Recording play (#80, #108): `LMA_RECORD=path lma-app ...` saves `path.state`, the presses
(`path`) and, in real time, a journal of everything host timing decided (`path.journal`: guest
time, frame skip, pad; GPU fences are synchronous while recording). `LMA_REPLAY=path` replays the
session exactly in lma-app, or headless with `LMA_BACKEND=wgpu LMA_REPLAY=path lma-cli run <xex> N`
(compare against any state saved during the session at the same instruction count);
`tools/replay_smoke.py` replays sessions with screenshots and fails on a crash.
Caveats (#296): a session recorded with sound on replays with `LMA_AUDIO=1` (lma-cli's default is
off; play.sh's `replay` sets it from `<name>.rec.audio`). The time is only written when it changes
(every 0.66 ms), so lma-app writes an end mark (`e`) at each state save and at quit, and a replay
(lma-cli and lma-app alike) holds the recorded time up to the last mark before going live; a replay
whose held time nothing advances any more (it diverged earlier) gives up after 5 s without progress
and says at which clock. A recording made while the Mac was saturated (a PGO build) differed with
0 journal divergences: `replay_check.py --diff N` shows where.

Audio (#199): `LMA_AUDIO=1` (lma-cli: off by default; lma-app: on unless `LMA_AUDIO=0`) runs the
game's XAudio render-driver callback every 256 samples of guest time and plays the submitted mix (6 channels downmixed to
stereo, 48 kHz) on the default output in lma-app. Guest threads then report the processor their
creation flags or `KeSetAffinityThread` pick (PCR+0x10C), which the audio engine's worker barrier
needs. `LMA_AUDIO_WAV=out.wav lma-cli run ...` writes the mix to a file; lma-cli also prints
callbacks, frames and each thread's share of guest instructions. Save states keep the driver only
when saved with audio on; states made without it stay silent, so start from boot for sound.
The game needs that sound engine: in a session without it (booted with audio off, or loaded from a
state saved without it, which lma-cli and lma-app report on load) Football One's Start Highlights
waits forever for a commentary cue to be prepared (#280). Replay a recording made with audio off
with `LMA_AUDIO=0`.
Almost all of the game's sound (music, commentary, cutscenes: 29,255 of 29,547 wave-bank entries)
is XMA, decoded by the console's audio hardware: `xma.rs` models it (contexts, input buffers,
output ring; kick/lock/clear registers at 0x7FEA1940/1A40/1A80) and decodes through FFmpeg's xma2
decoder when built with `--features xma-ffmpeg` (Homebrew FFmpeg on the Mac, `FFMPEG_DIR` elsewhere).
`FFMPEG_DIR=<dir> FFMPEG_STATIC=1` links the decoder-only static libraries `tools/ffmpeg_min.sh`
builds (xma1/xma2, LGPL 2.1+, no DLLs beside the exe; the script is also the LGPL relink recipe).
Without the feature XMA sounds stay silent. The kick/lock/clear registers are commands per store
(the game writes them one context at a time), so the guest queues `stwbrx` stores to them
(`guest::mmio`, only while the runtime's trap is on) and the runtime replays them in order. A sound
of several streams (the 5.1 crowd) uses one context per stream, following each packet header's skip
count. With `LMA_AUDIO=1` the kernel also answers `ExGetXConfigSetting` (language English,
widescreen): unanswered, the game sets match sound up at about -100 dB. `LMA_XMA_LOG=1` traces
the hardware; lma-cli prints per-context decode counts and per-channel peaks.

Physical memory (#199): a new block from `MmAllocatePhysicalMemoryEx` reads as zero, also where
an earlier block lived (as Xenia's heap, which decommits freed pages). The game relies on it: its
sound thread copies every voice's XMA settings from the voice record to the hardware context each
frame, also for a voice not set up yet, and a reused dirty block started the decoder on garbage
(the pre-match screech, the stutter loop, the broken analysis speech). Freed blocks also wait 60 s
of guest time before reuse (quarantine, default on; fresh memory first, the oldest blocks first when
it runs out; `LMA_PHYS_QUARANTINE=0` restores immediate first-fit reuse). `service_xma` logs the
first time a running context uses a buffer that isn't a live physical block. Loading a state saved
without audio starts a fresh XMA context array.

Audio tests: `lma-cli xma-check <game dir>` (every XMA clip through the hardware model vs ffmpeg;
`tests/xma_clips.rs` with `LMA_GAME_DIR`), `LMA_XMA_DUMP=dir` + `tools/audio_check.py` (a run's
decoder sequences vs ffmpeg), `LMA_AUDIO_DUMP=out.wav` + `tools/audio_glitch.py` (loops, gaps,
clipping in what the game played). lma-app's status line counts stale ring reads, decode errors,
output underruns and audio frames dropped when any occur, and shows how late the callback came
and how much sound the output has queued; `LMA_AUDIO_NULL=1` plays into nothing at the real rate,
so a run counts underruns without sound.

Audio timing: lma-app plays ~250 ms behind the game and steers back to that by playing up to 2%
slower or faster, because each game stall (100-280 ms when a cutscene or replay starts) takes
sound out of the cushion; running dry it resumes at ~100 ms. In real time the callback's guest
thread gets every other slice while it is ready (`LMA_AUDIO_PRIORITY=0` turns it off), as on the
Xbox where it has a hardware thread, without starving a thread it waits for; after a stall the
callback catches up to ~0.5 s of frames, further behind it skips (logged).

Guest threads on host threads (#39, `LMA_THREADS=N`, off by default): in real time, Ready guest
threads other than the current one run their recompiled code on N-1 host workers (`runtime/par.rs`)
until their budget (`LMA_PAR_SLICE`, 500k) runs out or the next pc needs the machine (a kernel
import, a hook, interpreted code, a stop); kernel calls, scheduling, GPU and audio stay on the
machine's thread. Atomics are value-based LL/SC (`Memory::cas32`/`cas64`, #344) and guest
barriers become host fences while workers run (`rec::barrier`). Workers are joined before
thread-management kernel calls, save/load states and at the end of `run()`. Serial when
deterministic, recording or replaying. Live day updates (the driver, career_adv4): ~38% shorter
with 4 threads; the game only uses its day-update worker threads there, so measure real days with
the driver, not lma-cli's instruction-timed presses (they never start the update in real time).
Default only after a full-season comparison (the maintainer's condition).

Fence nap and wait (#312, #502, #810; the native wait is on by default in real time, `LMA_FENCE_NAP=0`
off): the title screen and menus spin the game
thread in D3D's fence waits (the watchdog 0x82339080 polled with no sleep: 100% of a core). The hook
on the watchdog sleeps the thread 0.25 ms (`hooks::FENCE_NAP_TICKS`) on every other poll with an
unmoved GPU read pointer: title 96% -> 39% CPU, menu 100% -> 42%, 60 fps kept; in matches the day
match's 1% lows dropped in one A/B round (fences land within a few polls there), hence opt-in until
#861: with the 2,000-poll threshold a quiet-machine real-time pair showed the night match's game thread
at 66% instead of 100% of a core, fps and 1% lows unchanged (#171); what is left on that thread is 35%
asleep, 40% guest code of which 7% the D3D range (the commit 0x82381f68 4%) and the rest match logic. Rules
for hooks that sleep: they set `Machine::nap_until` and the dispatcher parks the thread as `Wait::Nap`
(a wake writes no status into r3, unlike a kernel call's `Sleep`); they never call `service_gpu`
(it switches threads to deliver GPU interrupts, sound only at a slice boundary); and a napping thread
resumes at the hooked address, so the hook must let the function run between naps or the caller's
own exit test never runs. With the hook installed the real-time idle loop returns to lma-app as soon
as a frame was presented. `LMA_FENCE_NAP=2` installs the hook without napping, `LMA_FENCE_NAP_LOG=1`
logs every 5000th poll. `LMA_FENCE_NAP=wait`, the default in real time (unset; deterministic runs install no hook, so their
fingerprints are unchanged), is the native wait instead of a timed nap:
the thread parks on its second unmoved poll as `Wait::GpuMoved` and wakes the moment the GPU writes its
fence or moves its read pointer (at most 0.25 ms later anyway), so a match's fences, which land at the
next command-processor run, cost no oversleep; deterministic runs spin as without the hook. Test scheduler changes live: the headless deterministic run hid two of the
three bugs the first build had (LESSONS).

Native player overall (#595, on by default, `LMA_NATIVE_OVERALL=0` off): the day update's top guest
function is a player's overall (0x8211f4c8: the role 0x8211be88, then 13 ratings x the role's weights
at 0x82005468, divided by the weights' sum), and its record getter 0x820dad10 is the hottest code.
`hooks::native_blocks` puts native versions of the overall and the role in the block table, so
`rec::call` runs them like recompiled code (a hook would unwind to the dispatcher on every call):
~2.2x the day segment's overalls per second. Same r3 for every player (`lma-cli overall-check <xex>
<state>` interprets the game's functions per id and compares), but not the same instruction count,
volatile registers or stack bytes: turning it on moved the regress fingerprints (re-recorded).
`=hook` uses the hook path, `=count` only counts the calls (the A/B baseline: fixed-instruction
segments can't time a change that removes guest instructions, the run just gets further).
The mods' hooks on hot functions run the same way (#623, `LMA_NATIVE_MOD=hook` keeps them hooks): the
valuation (`values_as_stored`) and the club lookup (squad_overflow.csv) are native blocks with the
game's recompiled function behind them for the cases they don't answer (`valuation_block_for`,
`player_club_block_for`; the first modded day's update 17.6 -> 9.4 s live). A native block for a
function on `guest::hooks::ADDRESSES` also makes its same-part callers go through the block table,
which can cost more than it saves: #635 (the player record lookup) made matches 2% faster and the day
2% slower. A block that calls the game's function behind it (to log its answer or adjust it) only
sees the answer when the call returns to it: a hook inside unwinds to the dispatcher, and so does the
slice budget running out mid-call, which in a live game cut short 92-95% of personal-terms calls,
the agreements (the longer calls) above all (#663). `hooks::call_to_the_end` lengthens the slice for
the call (budget, `slice_len` and `slice_end` alike, so the time base reads the same) and charges
its instructions; #665's AI offer and the terms log run through it. `LMA_STATEMENT_LOG=<file>` (#620) logs the bank statement lines recorded through
0x82104cd8; the wage bill and the monthly commercial credit are booked another way, so read the
bank screen for totals.
The AI's transfer search (#646) was the vanilla day's biggest cost: `0x82179e48` walks the player list for
each search and asks `0x821794c8` about every player, which prices the player (`0x821208e0`) before its
cheap tests. A native block in front of it (`hooks::SEARCH_FILTER`, the game's function behind it)
answers "no" from necessary conditions of the game's yes: own club, flagged player or club, unwanted
country (the club's stadium's), club tier range, minimum ability, position. 95% of calls never reach
the game; the search work takes ~90% fewer guest instructions. `LMA_SEARCH_PREFILTER=check` runs the
game on every call and reports any yes the pre-filter would have refused (0 in 8.27 M); `=count` and
`=0` are the baselines, `LMA_SEARCH_LOG=1` prints the counts with the guest time.
After its cheap tests the game's filter checks, in order: the criteria's trait flags (+28/+32/+36:
player record +0x18 bits 2/1/0), tier range, max wage (+4, unsigned), max value (+20, the
valuation), minimum ability (+0), flags 0x8211c9a0 / 0x82119540 (record +0x20 bit 7), the club's
league status against the searcher's (0x820ea990: 1-5 by which of the nation's five division ids,
World DB +(nation x 0x1a7f + 0x3aa) x 4, the club's division matches, England [19, 20, 21, 11, 12];
0 outside the 8 nations; a club with a lower status number than the searcher's, i.e. a higher league or
another nation, is refused), the club's tier above the searcher's + 1 (with contract years,
+0x34 & 7), then record +0x20 bit 8 and 0x8217c8c0. Vanilla day: 2,327 yes, 20,416 no on value,
50,560 later (#653's diagnostic, branch `mac/search-criteria-log`, not merged).
A transfer's negotiation (`0x82188750`, deal record: +0 player, +2 his club, +4 kind, +0x14 the offer
word: weekly wage << 13, contract kind in bits 9-12) has two stages. First `0x821884c0`, the
player's stature check: the buying club's tier (`0x820e3e68`, club word 0 >> 28, the clubs.csv
wealth) against his club's tier (+1, or -5 for some players), lowered by the club's status, his
mood (`0x821231f0`) and Champions League football; a buyer below refuses with 1, 4 or 5. Only an
answer of 0 or 0xb goes on to personal terms, `0x82187de0`: his demand starts from `0x82118e88`,
capped at GBP 250,000 a week, moves with the tier and status differences (tables at `0x8200b244`
and `0x8200b298`) and the contract kind, and is refused with 1 above the status wage ceiling
(`0x8200af9c`), 6-9 above the offer, 0xc when he won't talk (#663's diagnostic `LMA_TERMS_LOG`,
branch `mac/terms-log2`, not merged).
**AI and the modded economy (#704 audit, 21 Oct).** Every site where the AI reads a 2006-scale
constant, what it compares, the constant and the mod key that scales it. Sites marked *to find*
come out of the `LMA_SEARCH_LOG` / `LMA_TERMS_LOG` / `LMA_STATEMENT_LOG` runs on fc27final27 that
settle them; each missing adjustment is its own PR with a driven-days check (AI signings per
window, agreements %, wage distribution) against the vanilla game at 2006 scale.

| site | what the AI compares | 2006 constant | mod key |
|---|---|---|---|
| valuation `0x821208e0` (`VALUATION`), the price every search, bid and sale starts from | the player's ratings against the value ladder (22 words / 3, `tools/value_ladder.py`); capped at GBP 40m (`VALUE_CAP_SITE`) | stored u16 = GBP 1,000s; 40m | `value_unit` (unit and cap), `values_as_stored`, the ladder patches |
| the human offer screen's cap (`OFFER_CAP_SITES`, seven `lis/ori` pairs) | offer max = min(2 x value, 80m) | GBP 80m | `value_unit` |
| transfer budget `0x820eb058` | funds x board share - committed fees | 0.65 | `board_share`, `funds.csv` |
| AI wage offer `0x82187738` (`AI_OFFER`), the wage from the rating `0x82118e88` (`WAGE_ROUND`) | a 2006 wage from the rating, rounded up to 250,000 | the rating-to-wage table, 250,000 | `ai_wage_offers` (current wage + 10%, rounded to `WAGE_MAX`), `wage_cap` |
| personal terms `0x82187de0` | his demand (tier and status tables `0x8200b244`, `0x8200b298`) against the offer; refused above the status wage ceiling `0x8200af9c` | 100k/50k/25k/10k/10k/5k a week; `WAGE_CAP_SITE` 250,000 | `wage_cap`; the six `[[patch]]` words for the ceiling (no key yet) |
| deal completion `0x821889c0` (stage 0xd of `0x82184658`) | for an AI buyer, the agreed wage against a fixed ceiling (code 6 drops the deal; codes 2/8 squad minimums 16/11, 3 fee over funds, 4/5 32 players) | GBP 100,000 a week (`AI_SIGNING_WAGE_CAP_SITE` 0x82188d20/24) | `ai_signing_wage_cap` (#704) |
| stature check `0x821884c0` | the buyer's wealth tier (club word 0 >> 28) against the player's club's, his mood, Champions League football | none (relative): the tiers are clubs.csv data | the importer's wealth tiers (live squad value) |
| wage budget `0x820e6b80` / `0x820e6be0` (`WAGE_BUDGET_GET/SET`) | the club's weekly bill against its budget | cap 3,200,000 a week | `wage_budget_cap`, `wage_budgets.csv` |
| income `0x820e4018` (`ADD_FUNDS`): gate `0x82226b1c` `0x82226b58`, sponsorship `0x8212c624`, prize money `0x82162b98`, `0x820eca28` | 2006 GBP amounts added to the funds | the 2006 tables | `income_scale`, `gate_scale` |
| sponsorship offers (`SPONSOR_SHIRT_GET/SET` `0x821071c8` `0x82107200`, hoarding `0x821075a0` `0x82107608`, totals `0x82107db8` `0x82107e60`) | the offer table by division and country; the deal's weekly amount in stored units | best ~GBP 80k a week; 10 GBP per unit | `sponsor_offer_scale`, `sponsor_offer_country`, `sponsor_unit`, `sponsors.csv` |
| transfer search filter `0x821794c8` (`SEARCH_FILTER`, native pre-filter) | minimum ability, league status `0x820ea990` against the searcher's, the club's tier above the searcher's + 1, contract years, then the valuation | ratings and tiers (relative); the ability floor is relative too: per position `0x8218c430` sets it to the buyer's own team rating (`0x820e4db8`) x 0.9 x a random factor within +-2(7-n)% (n from `0x820f61e8`); only when no position qualifies, the full team rating x factor for positions whose starter is below 80% of it, then his weakest starter's rating x 1.05 x factor (and before that, a squad below 18 + tier/4 returns the shortfall as the count to search) | `value_unit` through the valuation; the floor follows the buyer's own squad, so `[scale] curve` (#709) lowers it with the league (no key needed) |
| substitutions `0x82213fd4` (`SUBS_SITE`), the simulated match `0x822a64a8` `0x822a64b0`, the bench (`BENCH_EXTRA`) | subs allowed, bench size | 3 subs, 5 on the bench | `subs`, `bench` |
| squad array (32 ids) and the club lookup `0x82118fc0` (`PLAYER_CLUB`); the squad count `0x820e4b88` | players a club owns; the AI's squad-size targets: no buying at 32 (`0x8218cd50`, purchases capped at 32 - count), the search fills up to 18 + tier/4 first (`0x8218c430`), a club won't sell below 17 players or below 12 of `0x820e4c40` (sale check `0x821889c0`, codes 2 and 8), the buyer at 32 refuses (code 5) | counts, 32 | `squad_overflow.csv` (ownership); no scaling needed, a key only if a mod wants bigger squads (32 is also the squad array's size) |
| European places `0x82138e68` (`EUROPE_PLACES`) | the nation's places record | England [2, 2, 1] | `europe_places` |
| AI bids `0x82189088` and the asking price `0x82188f38`; a deal's worth `0x8217c410`; the sale check's wage room (`0x821889c0` code 6) | asking price: the club's own price for him (`0x8210c380`) or the valuation, x 1.8 at status 5; bid: base / 100 x a random 80-170% by status, capped at 2 x valuation, at least the best open offer (`0x8217ce40`); deal worth: fee + instalments x 0.9 + sell-on x 0.25 + an exchange player's valuation x 0.75-1.5; wage room: the deal's added weekly wage against the buyer's room | fees relative to the valuation; the AI buyer's wage room is a flat GBP 100,000 a week (human clubs: wage budget minus the wage bill) | `value_unit` through the valuation (fees); `ai_signing_wage_cap` (#851) for the AI's 100k wage room; check: a driven window, fees in `LMA_STATEMENT_LOG` against values |
| regens and youth intake wages (`WAGE` `0x82123dc8`; the wage from the rating `0x82120d38`) | a new player's wage: a 10-step rating table (immediates in the code, 2..480) x (club tier x 15 + rating), by contract kind and nation, rounded (`WAGE_ROUND`), then capped by league status and kind | the cap table `0x820055a8`..`0x82005607` (league status 0-5 x contract kind 0-3, words): status 0 60k/70k/80k/80k, 1 40k/45k/50k/50k, 2 2k/4.5k/6k/6k, 3, 4 and 5 1k/1.5k/2k/2k a week; floor 150 | **none yet:** the 24 cap words are data, so `[[patch]]` reaches them today (no fc27final mod does), but they only clip: regens below the 2006 cap keep 2006-scale wages. A real scale needs the rating table's immediates (recompiler sites, like `wage_cap`) or a hook on `0x82120d38`'s result (#704). AI offers stay at current + 10% (cheap regens); human offers meet 2006 demands |
| contract lengths (deal +0x14 bits 9-12) | years offered | years, not money | none needed; confirm with `LMA_TERMS_LOG` on a modded window |
| loans (wage share, loan fees) | **to find** |  | `LMA_STATEMENT_LOG` over a modded season names the sites |
| the board's funds figure `0x820eaa10` (`FUNDS`): stored per club at +0x54 (`0x820eb8c8`), read by the board's verdict `0x822bd6b8` | the club is in trouble when its balance (`0x820e3f48`) is at most -FUNDS/2 | absolute: a 16-word table at `0x82703118` by wealth tier (300-20000) x 10 x a byte per country at `0x820033ac`, England's byte is 150, so an English club's figure is the tier word x 1,500 (the 0.15 of the shown funds); three clubs are hard-coded by uid: Chelsea (0x1716) x 2,000 (its 0.2), Celtic and Rangers (0x170e, 0x186c) x 800; a country-type-0xe club in competition 0xb/0xc x 1,000; about GBP 150k-40m | **none yet:** a `[[patch]]` on the 16 table words (data, x the mod's money scale), or a `funds_scale` key |
With #646 the vanilla day segment (fp.sh's career_adv4 input, deterministic, headless, 8.5 G
instructions) takes 4.36 s on the Mac (7.6 s on 7 Oct). Its native profile (macOS `sample`, 13 Oct,
#39) is no longer the day logic: game code 63%, of which the top functions are the graphics runtime
and UI drawing the processing screen (`0x823d4500`, `0x823b0020`, `0x823c27a0`, `0x824009d0`, the
28-byte `0x8233ae48`, `0x82343ff0` under the ring functions); our command processor 15% even with
LMA_NORENDER (`Gpu::execute_packet` 8.3%, `write_registers_from` 4.5%); `rec::call` 7%; memory
copies 6%. The next day-speed levers are fewer frames drawn while a day processes, a cheaper command
processor for UI-only frames, and #145 on those runtime functions.

Match stalls (#6): `LMA_RT_SLOW=<ms>` with `LMA_FRAME_PROFILE=1` logs, whenever more than <ms> pass between
two presents the render thread executes, its own steps in that window (landings and their KB, resolves,
present and its GPU wait, setup) and the game side's (slices, CP runs and how many returned blocked, the
deferred interrupt's wait). `LMA_LAND_LOG=1` logs every landing by the render thread's frame,
`LMA_RESOLVE_LOG=1` every resolve by the command processor's frame (`threaded::GUEST_FRAME`: render-thread
work shows up a frame late, so a late render frame looks like two frames' work), and `LMA_ALIAS_LOG=1` now
also the frames where a full-screen texture couldn't be composed from its resolve tiles. Count stalls as
frames over 33 / 50 ms over a whole run: the status line's `1% low` is per 5-second window.
GPU memory (#746): `LMA_GPU_COUNTERS=1` logs the wgpu backend's caches every 5 s (decode cache, GPU
textures and their Mpx, pipelines, setups, surfaces, pending read-backs); built with
`--features runtime/gpu-counters` it adds wgpu's live buffers, textures, views and bind groups. Pair it
with macOS `footprint --pid <pid>`: a night match's spikes are pending resolve read-backs.
The stalls themselves (#639): the front-buffer read-back queue (`PRESENT_QUEUE`) was the only bound on
how far the GPU runs behind, and at 3 it drifted 3-4 frames back. Fast fences hold a fence for a resolve
under 1 MB issued just before it (`threaded::fast_ticket`, for #166's shirt scratch), and the luminance
targets (80x48, 320x184) are that size, so the end of the luminance pass held a GPU interrupt until the
whole backlog landed: 60-130 ms frames 20-30 times a minute in a night match, the render thread idle and
the game spinning in the D3D fence wait. Depth 1 (`LMA_PRESENT_QUEUE`) caps them near 60 ms with the same
fps. The guest never reads those landed match targets back (`LMA_RWATCH`: 0 loads), and a third of the
match's game thread is the ring-space wait `0x82335fc8` (our command processor consumes the ring between
guest slices); deterministic runs show neither, since their fences are synchronous.
`LMA_DRAW_TRACE=<file>` (#645) writes a 32-byte record per draw (shader, float-constant and texture-fetch
hashes); `tools/draw_trace_diff.py <trace> <frame>` names the draws that differ for one frame only, e.g.
#205's flash, which is the far-side advert boards showing the wrong brightness state for one frame.

Render and timing debugging (#205): `LMA_RT_TEXCHECK=1` hashes every texture of up to 1 MB when
a draw is sent to the render thread and again when it is drawn, and logs any that changed between
(a texture the game rewrote under a pending draw; resolve targets landing in order show up too).
`LMA_SEAM_LOG=2` (#526, the row-384 seam): the game draws each frame twice, once per EDRAM tile,
with PM4 predication, and the CPU transform reads the vertex buffers when the command processor
reaches each draw; per frame it hashes the first 4 KB of every vertex stream a draw reads in the
first pass and compares the same draw in the second, printing the frames where they differ (the
guest rewrote geometry between the passes, so the lower tile shows newer geometry). 
`LMA_SINGLE_PASS=1|0` (#171, step 1 of single-pass rendering behind HLE) renders the tiled scene once; lma-cli
does so by default (`0` replays the tiles as the game issues them), lma-app replays until tested live (#876).
The game replays one command buffer per EDRAM band: tile 0 (select 0x80000002) with the full 720-row
viewport, window offset 0 and the scissor at rows 0..384, resolved into a 384-row texture; tile 1
(select 0x8) with the offset moved to (0,-384) and the scissor at rows 384..720, resolved into its own
336-row texture; the present composes the bands. Single-pass keeps tile 0 with its band scissor widened
to the viewport's height (from `PA_CL_VPORT_YSCALE`; resolves keep theirs, so tile 0's resolve of the
other band stays empty as before), drops the later tiles' predicated draws (`Gpu::pass_dropped`), and
runs their resolves and clears with the window offset zeroed for that packet: the resolve reads EDRAM
rows 384..720 of the full-height surface and writes its band texture from row 0 as before. Every other
packet runs (fences, events, the swap). Other predicated passes (the shadow map, the 384-row pass, the
luminance targets) keep offset 0 and are untouched. A match frame loses ~930 of ~3,100 draws and the
second pass's clears; the seam draws take the first pass's geometry, so the row-384 seam (#526) is
gone by construction. `LMA_SINGLE_PASS_LOG=1` prints each distinct (scissor, viewport, offset, select)
a predicated draw runs under. Step 2 (after #731): the decision in `hle_draw` and no replay at all.
`LMA_ALIAS_LOG=1` logs resolve copies the texture path stops sampling (guest memory changed) or
can't use (a fetch of another size or format). `LMA_SWAP_LOG=1` prints guest time and the
instruction clock at each frame swap. `LMA_SLICE=<instructions>` changes the scheduling slice
(20,000), to test whether something depends on how guest threads interleave.

Texture checks (#166): `LMA_TEX_LOG=file` logs every texture decoded from guest memory (cache
generation, address, size, format, hash of the guest bytes); `LMA_TEX_DUMP=dir` also writes the
decoded pixels as `.ppm`. `tools/tex_compare.py ref.log run.log...` flags textures that are static
in a reference run (`LMA_FENCE_FAST=0 LMA_RT_ENCODER=0`) but decode to other contents in a run: it
caught the shirt backs built from the previous player's read-back (a fence right after a resolve to
a target under 1 MB now waits for it).

Guest memory (`guest/mem.rs`): on Apple platforms and Windows one flat 4 GB host mapping (a guest
access is base + translated address; Windows commits it once, demand-zero, with a write watch,
#245); elsewhere a table of 64 KB pages. Save states are the same list of non-zero pages either way.
On Apple the mapping also maps guest 0xC0000000–0xDFFFFFFF as an MMU alias of 0xA0000000– (the same
pages, `mach_vm_remap`), so below 0xE0000000 the guest address is the host offset: the recompiled
code's loads and stores are one compare + load + byte swap (no translate, no 64 KB page check) and
bulk `read`/`write` one copy (#317: −19.6% on the night segment). 0xE0000000– (physical + 0x1000)
keeps the translating path; save states skip the alias range.
The one-word accessors' fallback to the general `read`/`write` goes through `read_cold`/`write_cold`
(`#[cold]`, `#[inline(never)]`), so the recompiled code is laid out for the fast path (#332: -5%
on the night segment for -0.7% instructions).
Windows has no alias (a section view would lose the write watch below, worth ~3 ms of render time
a frame live), but below 0xC0000000 the guest address already is the host offset: `fast` is one
compare there and bulk copies one copy, and only 0xC0000000– translates (#320: day −24%, night
−35%, match-day kickoff −39%, bit-exact).
The render thread uses the write watch to skip re-hashing textures nobody wrote since their last
check (per 4 KB page stamps, ~1 us a query; #246 on Windows, #355 on Apple by page protection of the
flat mapping); `LMA_TEX_WATCH=0` turns that off (back to a full hash of every texture each frame).
Landed resolves stay hashed on Apple (`LMA_LAND_HASH=0` watches them: a fault marks a whole 16 KB host
page, so a landed band sharing a page with guest writes would be dropped and decoded every frame). `LMA_RT_CHECK=1` panics if the render thread's
registers or shaders differ from the command processor's (a missed dirty mark or `shaders_dirty`).
Pipeline pre-warm (#304, #330): `LMA_PIPELINE_CACHE=<file>` lists every shader (microcode) and
pipeline key the wgpu backend creates, and the next start builds them all on a background thread
(a match kickoff's shader modules cost 3-7 ms each). lma-app keeps the file next to its config
(`wgpu-pipelines.txt`); an empty value turns it off, and lma-cli doesn't use it.
Render-thread per-draw work (#304, PC night match): float constants not written since the previous
draw are reused without gathering (the render loop passes each draw's written register blocks to
`regs_dirty`, #335); indices decode into one reused buffer (#336); present's frame-sized buffers
are pooled across frames (present 22% -> 7% of the render thread, #341); cached setups whose
render targets were drawn again get the new views patched in (setup misses 5-6% -> 0-1%, #342).
Tried and not kept: pooling the draw snapshots' buffers (the hand-back cost the render thread
what it saved the guest thread).
Setup re-validation (12 Oct, #6): after the alias fix (#549: a landing resolve invalidates only
setups with a decoded texture over it, 1,250 -> ~4 misses a frame), 17% of a night match's draws
still re-validated their cached setup once per texture generation, and 2.0 of those 2.2 ms a frame
were `prepare_textures` re-hashing decoded textures whose pages had not changed, because Apple had no
write watch (`MemReader::write_watch` was Windows-only). Two changes: a resolve to the same dest at
the same size writes into the previous GPU texture (`Resolved.gen` is the texture's identity,
`content` the texels: identity views stay valid, swizzled copies and composed textures follow the
content), and a cached setup records its decoded textures' ranges and page stamp (`SetupWatch`), so
with a write watch the check skips `prepare_textures` when nothing wrote those pages. With #355's
Apple watch (landings stay hashed): render busy 12.7 -> 9.2 ms, ps setup 2.9 -> 0.8 ms on the night
match; `LMA_TEX_WATCH=0` is the old path for A/Bs. A resolve never reuses a texture made or reused in the
same frame (a band resolved twice in a frame is composed from both snapshots later in it);
`LMA_RESOLVE_REUSE=0` turns the reuse off, and `LMA_SETUP_LOG=1` counts reuses and refusals.

### Testing changes (both agents)
- **Bit-exact checks** for anything in the guest/recompiler path: run the same deterministic
  segment (`LMA_DETERMINISTIC=1 LMA_NORENDER=1 LMA_REGS=0 LMA_PEEK=...`) on main and on the branch
  and compare the dumped memory/registers and `last_run.log`; any difference must be explained.
  `tools/diff_native.py NAME_A CLI_A [ENV=V ...] -- NAME_B CLI_B [ENV=V ...] -- STATE ...` does
  that as one command (#377): both sides run the segment from the state with `LMA_SAVE`, the end
  states' guest memory (`lma-cli state-mem`, 4 GB each, compared in chunks), the registers
  printed at the end and the kernel-call logs are compared, differing ranges listed with the
  bytes on each side, `--mask A-B` for ranges the sides may legitimately differ in (reported,
  never silent); `--work DIR` for the 4 GB dumps, deleted per state unless `--keep`; a side that
  stops short of the target clock fails the state. It is the NATIVE track's gate: a native replacement behind a switch must come
  out IDENTICAL against the emulated original before its switch may default on. To find *where*
  two builds split, bisect the run length with the two lma-cli builds from the same state and
  compare the thread table and registers they print; the first differing run names the slice.
- **A player's session as a test:** record it (`LMA_RECORD=name.rec`, or `~/lma/play/play.sh
  record name` on the Mac), quick-save (F5) at interesting moments, then
  `tools/replay_check.py name.rec quick*.state` replays it headless and requires every saved
  state to be reached bit-for-bit, also across F5/F9 loads (`LMA_REPLAY_AFTER_LOADS`).
  `LMA_JOURNAL_DEBUG=1` prints the first events a replay missed.
- **Long runs:** drive the career like a player (read messages, calendar, match days, end of
  season, sackings) with `LMA_INPUT_LIVE=1` commands, recording every run so any stop replays
  exactly. Judge frame rate by the first status number (frames shown); `game N fps` is the
  guest's own rate; the adaptive skip (`LMA_MAX_SKIP=2..6`, off by default) trades one for the
  other.
- **Measuring on the Mac** (10 Oct): `tools/ab_live.py` (live A/B: a build dir and extra env per
  side, interleaved rounds, medians of the status windows) and `tools/seg_time.py` (two lma-cli
  builds on the deterministic day and night 20 G segments, interleaved), both from #369 and
  machine-neutral; the install decision is `tools/play_gate.py`. Local in `~/lma/driver`, outside
  the repo: `fone_table.py` and `compare_tables.py` (the Football One tables the driver saves,
  read by macOS Vision OCR through `vid/ocrtool`, consistency-checked and compared between two
  runs: #346's month comparison). Timing numbers count only on a quiet machine: no CI job
  (`cd ~/actions-runner-lma && ./svc.sh stop`, start again after), no other game, no build.
- **Save states** are bincode: new fields go after the existing appended items, never inside the
  serde structs, so older states keep loading. A state holds the guest (CPU, threads, handles,
  memory pages, allocators, VFS, GPU, input) and, appended in order: guest time, the physical
  allocator, mounted packages, the audio driver, the running XMA contexts, the physical
  quarantine (#292). Host-only things come from the machine it's loaded into: recompiled code,
  Since #864 a state saved by lma-cli holds resolve destinations as they were before the last skipped
  read-back (nothing lands headless): frames and fingerprints are unaffected, but a byte comparison of
  two lma-cli states across that commit differs there (`LMA_SKIP_LAND=0` for a landing run).
  caches, logs, real-time scheduling fairness, frame skip. Two checks keep it complete:
  `regress.py --roundtrip` (state left out: a resumed run ends differently, as the quarantine
  did) and `machine::tests::a_loaded_state_saves_the_same` (state saved but not restored).
- **Benchmarks without save states** (a build that can't load the others' states, or to
  include boot): `LMA_INPUT` timelines are in M instructions under `LMA_DETERMINISTIC=1`, so a
  scripted path replays exactly on any build. New game with every default:
  `6000:10:8` (START) then twelve A presses (`:1000:8`) every 1,800 M from 7,000 M; the career's
  creation ("updating", game logic like a day's processing) then runs from about 30 G to 45 G.
  Run to 30 G and to 45 G with `LMA_NORENDER=1`; the difference is the segment (13.2 s on an M4
  Max, 7 Oct). Boot to 10 G measures the intro (video decoding, vector code); for loading, measure
  boot to the title (~3.3 G, "Please press START"): past it the game idles at the title (#303).
- **Nightly regression** (#232): `.github/workflows/nightly.yml` runs `tools/nightly.py` on both
  self-hosted runners at 02:30 UK time (parts regenerated from the local default.xex, fingerprints,
  replays, fps on the canonical match states, an audio test, a season soak), then posts one summary
  on #232 and to the phone. Each machine keeps its paths in `~/.lma-nightly.json`
  (`tools/nightly.example.json`); only numbers leave the machine.
  The fps and audio steps open live windows: on the PC (the runner is a logon task in the maintainer's
  session) they show on his desktop at 02:30. On the PC the build command calls
  `tools\build.ps1 -NoWait`, since the nightly job is itself a job on lma-pc and build.ps1
  would otherwise wait for it.
- **Tools** (`tools/`, Python 3; none contain game content):
  - `regress.py`: the local regression suite (your own game files and states). `--roundtrip` also
    runs each fingerprint case saved halfway (`LMA_SAVE`) and resumed: memory and registers must
    equal the straight run's (#239). lma-cli skips `LMA_INPUT` presses timed before a loaded
    state's clock: the state brings its own pad. Baselines are each machine's
    `states/regress.json`: when a change moves the fingerprints on purpose (#224's fctiw/fctid
    rounding), both machines re-record them (`--update`) and must agree on the new hashes.
  - `replay_check.py`, `replay_smoke.py`: recorded sessions replay bit-exactly / without crashing.
  - `audio_check.py` (an `LMA_XMA_DUMP` run vs ffmpeg), `audio_glitch.py` (gaps, loops, clipping
    in an `LMA_AUDIO_DUMP` WAV).
  - `tex_compare.py` (`LMA_TEX_LOG` runs), `flash_detect.py` (one-frame flashes in an
    `LMA_FRAME_DIR` dump; `--selftest`).
  - `pgo_build.py` (profile-guided play builds), `ppcdis.py` (read guest loops).
  - `xiso.py`, `god.py` (extract the disc or a Games-on-Demand copy), `make_stub_bank.py`
    (stand-in sound bank for an incomplete rip).
  - `lma_db.py`, `lma_edit.py` (the database; reference only, `lma-cli db-export`/`db-import`
    replace them).
  - `agent_inbox` (Agent sync, status board, hooks), `build.ps1` (PC builds at Below Normal).
- **Play builds** (what the maintainer plays; CI and tests stay plain): `tools/pgo_build.py`, PGO trained on
  the regress cases (give it the full set with `--states`: the day case with its input and both
  match states, or day processing is profiled as cold), plus `--cpu` and sound
  (`--features lma-app/xma-ffmpeg,lma-cli/xma-ffmpeg`).
  - Both PGO stages must use the same settings (LTO, packages `-p lma-cli -p lma-app`), or the
    profile silently doesn't apply and the result runs at plain speed (#250).
  - **PC:** MSVC toolchain (`RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-msvc`, Visual Studio Build
    Tools; the GNU one has no PGO runtime), `--cpu znver2 --lto off` (thin LTO trips an LLVM SEH
    bug in naga on Windows), `FFMPEG_DIR` = a portable FFmpeg shared build, and its `bin\` on
    PATH when running. The launcher sets `LMA_AUDIO=1` and that PATH.
  - **Mac:** `--lto off` for now. `pgo_build.py` trains without `xma-ffmpeg` on macOS: an instrumented
    build linked against Homebrew's FFmpeg dylibs segfaults in LLVM's profile writer at exit and
    leaves a corrupt profile. The feature doesn't change the game parts' crate hashes, so their profile
    still applies. The optimised stage bypasses sccache, which keys on the profile's path, not its
    contents (#275). Headless 60 G of `match_night_5a4cbfb`: ~52 s plain, ~41 s PGO. the maintainer's play build
    is gated by a live A/B against the current one (cutscene underruns, match fps and 1% lows) and,
    since 8 Oct, the picture: one-frame flashes per live run (`flash_detect.py`), on the canonical
    states plus one from the maintainer's latest session. Render-path changes also get a look before install:
    8e37dfe-pgo passed fps and audio but showed tearing and billboard flicker live (rolled back).
  - **Before installing one**, check: the binary isn't an instrumented leftover (lma-cli ~161 MB
    on the PC since the recompiler rework, guard 170 MB), the headless night-match segment (5 G
    from `match_night_5a4cbfb`) is PGO-fast (~4.6 s on the PC), `regress.py` day + match fingerprints
    pass, a boot with `LMA_AUDIO_WAV` has 0 decode errors and `audio_glitch.py` finds no glitches,
    and a live run shows no panic.
  - **PC gate (9 Oct):** the candidate is timed against the *installed* build, interleaved, 2
    rounds: day, night match, the maintainer's Barça v Real night (10 G), day-match, boot. It installs only
    if the match isn't slower and the day is within 5% (a PGO rebuild of the same code moved the
    day 4% and the match segments under 0.3%, #6; render-only changes don't show here, so
    they ride along with the next build that wins). Flashes: two live runs per build, at most one
    more than the installed build's total (one-frame flashes are random, #205: the installed build
    showed one too). Live fps from a single verify run is noted, never decisive.
    `tools/play_gate.py --old <installed> --new <candidate> --install <launcher>` runs all of it,
    plus interleaved live rounds on the config's match states (#363); the states and paths are in
    a local `~/.lma-gate.json`.
  - **PGO training set (PC, `lma-states/train/regress.json`):** the regress cases plus a rendered
    run of the maintainer's Barça v Real night state. Adding it made the same code ~2% faster on the night
    segments (play #14 vs #13).
  - **Live training (#360):** a `"live": [{"state", "seconds"}]` list in the training
    `regress.json` runs the instrumented lma-app in real time, so the window loop, pacing and the
    threaded render path are profiled too (deterministic lma-cli runs never reach them). On
    Windows `std::process::exit` is `ExitProcess`: no atexit handlers, so no PGO profile. The
    training sets `LMA_QUIT_ATEXIT=1`, and lma-app's `quit` then leaves through the C runtime's
    `exit`. Check that the training lists a profile for lma-app as well as lma-cli.

## Facts measured from this game

- Image base 0x82000000, entry 0x82329E90, image 0x810000, retail-encrypted, basic compression.
- `.text` 6.4 MB; `.pdata` lists 14,571 functions covering 94 % of `.text`.
- 1,512,927 instructions, 160 scalar mnemonics; vector code ≈ 15.6k words (1 %).
- Imports: 130 functions from xboxkrnl.exe, 49 from xam.xex – the whole HLE surface.
- Xenia Canary runs the game to menus/careers; matches need the complete disc copy.
- Matches (night test state): ~2,300 draws a frame in 11 batches; D3D rewrites ~13 of the 64-
  register blocks per draw, ~3.7 with new values; skinned shaders address float constants
  relatively (whole 256-register block per draw before #194); ~860 command-processor runs a
  frame; ~19 block-on-fence calls a frame, one of them (the present path) waiting for vsync.
- GPU surfaces are 2048x2048 (multisampled for the 2x MSAA main pass) and every pass loads and
  stores the whole attachment; a 720p target uses about a quarter of it. On Apple GPUs
  `LMA_GPU_TIMING` sums overlapping pass intervals (more than the frame time), so per-pass
  numbers show where time lands, not what each pass costs.
- Formations: 14, named by strings 3322-3335 (4-4-2, 4-2-4, 4-3-3, 5-4-1, 3-4-3, 3-5-2, 1-4-4-1,
  1-3-4-2, 1-2-5-2, 4-3-1-2, 4-4-1-1, 3-4-1-2, 1-3-3-3, 4-2-3-1), each 11 slot roles (u32) in the
  table at 0x82710f58 + formation*44 + slot*4. Roles: 0 GK, 1 FB, 2 SW, 3 CB, 4 WB, 5 CM, 6 WM,
  7 AM, 8 CF, 9 wide forward; there is no DM role. A team keeps its formation index at +144 of its
  match/tactics record (0x822a2288 reads the roles through it). Modern shapes are [[patch]] words
  plus the strings: 4-3-3 slots 6/10 WM->9, 7/8 CF->5 (three CMs, wide forwards, one striker);
  4-2-3-1 slots 3/9 AM->9; 5-4-1 slot 3 CB->5 (4-1-4-1); 1-4-4-1 slot 4 SW->3 (5-4-1). How the
  AI picks its formation is still to find (Managers.bin holds no field that matches known
  managers: Wenger 4-4-2, Mourinho 4-3-3, Benitez 4-2-3-1).
  Adding shapes (#691, #749, 21 Oct): a team's choice is a 4-bit field (the top nibble of word
  +0x24 of its 1472-byte tactics record, club object + 0x1da0 + slot x 1472, written by key 0 of
  the setter 0x8210de88, the next nibble its second shape (key 1), saved as such by 0x822fbb10:
  16 shapes at most) and live data follows both tables. mod.toml's `formations =
  "formations.csv"` (`id,name,roles`: ids 0-13 change a shape, 14 and 15 add one) copies both
  tables into the section gap 0x826ff400 (`guest::rec::FORMATION_REGION`: nothing addresses it,
  zero in a late save) with the rows applied, and every reader follows: the 36 lis/addi pairs
  that build the role table's address (30 in the match engine), the two name-table pairs
  (0x827023f0, and its copy 0x827135bc behind the getter 0x82326280 the Team Tactics row and
  the match-day screens call), the caption's string-id list pair (0x82027dcc: 14 ids 3322-3335
  indexed by the nibble at 0x822dc7b4; the new names are appended to every language's table as
  new ids 128 past its end, `ModSpace::build_strings`: the game passes the table's length as "no
  string" at 388 sites), the Team Tactics pitch diagram's spot table pair (0x8200b370: 14 x 11
  spots, each an index into the 34 (x, y) grid pairs at 0x82705160, read only by the diagram's
  slot placement 0x82191b40; a mod row gives its 11 spots or gets them laid out from its name,
  `runtime::mods::default_spots`) and the nine count sites (three `li rD,14`: the count
  getter 0x823262a8 behind the vtable entry 0x823243b8, the Team Tactics screen's option counts
  for its Formation row and its four situation pickers; six `cmpwi ..,14`: the name getter's
  bound, the setter's two clamps, the situation pickers' wrap, the match engine's "GP: Error -
  Formation being set to F_442" guard and its own-formation fallback) read
  `guest::rec::FORMATION_*` in the recompiled code and get their words rewritten for the
  interpreter (`runtime::mods::patch_formations`). The other 14s around them are the
  tactic-instruction code for "none" (13 instructions; 0x82261b48/0x82261cc8, the match-day
  screen) and the squad of 11 + 3. A `[[patch]]` of a code word alone only reaches the
  interpreter: the parts bake addresses in. The AI's choice among the shapes is still unlocated.
  The match engine places players by role (the 30 role-table sites), not by the diagram's spots.
- Match players (#530): the match object is obj = *(0x82755900) + 20640 (zero until kick-off). Each
  team (stride 19408 = 0x4BD0) has a matchday squad of 16 ids at +64 (-1 empty) and 16 player slots
  of 0x400 bytes at +3008: 0-10 on the pitch, 11-15 the bench. Slot word 0 is the player id, word 1
  the slot index, +1012 the on-pitch flag. 0x8225edc0(obj, team, i) returns 0 past i = 15, and the
  stride is a constant in about 217 places, so the 16 cannot grow in place. A substitution swaps two
  whole slot images (0x8226bf30, called by 0x8226c130 after the slot-by-id lookup 0x8225edf0,
  which returns 0 at 0x8225ee2c for an id not in the 32 slots); 0x8224cb30 lists bench slots
  11-15. A bench of 9 therefore keeps subs 6-9 as stored slot images outside the object and swaps
  one into a bench slot on the lookup's not-found exit. Per frame, 0x82265910 sorts the active
  players into two lists (22 by distance at obj + 0xC8A0, 11 of one team at obj + 0xC8F8).

## Player-facing requirements

- **Input**: keyboard + mouse and any gamepad (Xbox, PlayStation, generic) on Windows, macOS
  and Linux. Bevy input feeds the HLE `XamInputGetState`/`XamInputGetKeystrokeEx`; menus also
  get mouse hover/click by mapping the cursor to the game's focus navigation, with key
  rebinding stored per user.
  Keyboard layout (a PC layout, not letters matching the pad): WASD or the arrows move, Enter or
  Space = A, Esc or Backspace = B, F = X, R = Y, Q / E = LB / RB (tabs), Z / C = stick clicks,
  Tab = START, V = BACK; keys held with Cmd/Ctrl are the app's hotkeys (Cmd+S save state,
  Cmd+L load, Cmd+T turbo, Cmd+F frame cap; F5/F9/F1/F3 too), F2 the settings menu.
  Rebinding: a `[keys]` table in the config file (see Launcher) gives a button exactly the keys
  listed, e.g. `A = ["Enter", "J"]`; the others keep the defaults (`lma-app/src/keys.rs`).
- **Two players** (#206): LMA 2007 offers New game → two players (one or two only; three or four
  local managers would need changes to the game's career code, #186). The game enables it when
  a second controller is connected and signed in: lma-app gives the first gamepad port 1 with
  the keyboard and mouse, and a second, third and fourth gamepad ports 2-4 (connected while
  plugged in); `LMA_PADS=2..4` connects ports without pads. Each connected port has a signed-in
  profile ("Player 2", its own XUID). Player 2 picks their controller with START, managers take
  each day in turn, only Player 1 can save the career, and a two-player save asks Player 2 for
  START again when loaded. The live script drives another port after `port N`; recordings
  journal port 1 only.
- **Launcher** (#4): `lma-app` with no arguments shows a launcher (Continue from
  `states/quick.state`, New boot, a save state, the mod space, windowed / fullscreen) and runs
  the game folder remembered from the last run that was given a path (config file:
  `~/Library/Application Support/lma-rs/config.toml`, `%APPDATA%\lma-rs`, XDG). A path argument
  or any of `LMA_LOAD`, `LMA_INPUT[_LIVE]`, `LMA_REPLAY`, `LMA_RECORD`, `LMA_NO_LAUNCHER` skips
  it, so scripts start as before; `LMA_LAUNCHER_PICK=<n>` chooses item n (tests).
  The config file also takes `render_every` and `turbo_until` (the env vars still win) and the
  `[keys]` table; `lma-app/src/config.rs` has an example.
- **Lost window GPU device** (#302, #321): lma-app recreates its renderer (bevy's `Recover`,
  with the sprite renderer's buffers reset) and the game runs on; after 3 losses, or any other
  render error, it saves `states/device_lost.state` and exits with code 1.
  `LMA_LOSE_DEVICE=<frames>` destroys the window's device every that many frames, to test it.
- **Vanilla is never modified.** The game folder is read-only to the port. Mods live in
  separate *mod spaces*:

  ```
  mods/
    vanilla/                      (implicit: the untouched disc data)
    2025-26/
      mod.toml                    name, base database (start-of-season / deadline), version,
                                  base_year = 2026 (the season the game starts in),
                                  value_unit = 5000 (GBP per stored value unit, #475/#479),
                                  income_scale = 4.0 (gate, sponsorship, prize money, #478),
                                  subs = 5 (substitutions a match allows, 3..5; written at match
                                  setup, so a state saved mid-match keeps 3; #530/#533),
                                  values_as_stored = true (a player is worth his stored value,
                                  moved by ratings and match form, #481/#491/#501),
                                  wage_budget_cap = 10000000 (the weekly wage budget cap, #486),
                                  board_share = 1.0 (the share of the funds the board releases for
                                  transfers, vanilla 0.65; #522),
                                  wage_cap = 2000000 (the most a contract offer may pay a week,
                                  vanilla 250,000; #536; but the offered wage is a 19-bit field,
                                  `[0x8275478c] >> 13`, so 524,287 is the real ceiling; the contract
                                  screen also clamps offers to a per-status table at 0x8200af9c,
                                  100,000/50,000/25,000/10,000/10,000/5,000 a week, raised with
                                  six [[patch]] words, the maintainer's 100k board limit, 12 Oct), sponsor_unit = 100 (GBP per stored unit
                                  of a sponsorship deal's weekly amount, vanilla 10; #535),
                                  sponsor_offer_scale = [24.0, 6.0, 3.0, 2.0, 2.0] (the shirt
                                  sponsorship offers' base table by division, top first, the last
                                  value for the rest; entries capped at 6000 x GBP 250; written
                                  again after a state load; #582),
                                  sponsor_offer_country = [1.0, 0.15, 0.6, 0.9, 0.85, 0.7, 0.15, 0.15]
                                  (with the above, x this per country: England, Scotland, Italy,
                                  Spain, Germany, France, Netherlands, Portugal; vanilla's table is
                                  flat across countries; #591),
                                  gate_scale = 8.0 (the gate receipts' own factor over income_scale: the
                                  two ADD_FUNDS gate sites and the gate's four statement lines; a driven
                                  fc27 career showed the gate at about half a modern matchday, #619, #630),
                                  europe_places = [[4, 0, 1], ...] (8 nations in LMA's country order,
                                  [group stage, qualifying round, UEFA Cup]: a hook on 0x82138e68 writes them
                                  into the nation's places record before the game hands them out, #607, #631),
                                  from the first season end on: a new career's first-season
                                  entrants are not from the tables but read from the start
                                  state (0x82137d40 fills the lists at 0x827413e0..0x827415e4
                                  with 16-bit club uids from the save stream at 0x827b0c24),
                                  so they stay 2006/07's clubs until a mod writes them (#607),
                                  keys above the first [[patch]] table (after it, TOML puts
                                  them inside that table),
                                  and only keys the *installed* build knows: mod.toml is `deny_unknown_fields`, so a key from a
                                  newer branch makes an older binary refuse the mod with a status only the window shows (12 Oct:
                                  `sponsor_unit` on a rolled-back build looked like a broken launch),
                                  [[patch]] addr/from/to (single words of the loaded image)
      database/players.csv ...    lma-data CSVs, applied on top of the original Export.mng
      wages.csv                   id,wage,years: real weekly wages and contract seasons (#486)
      funds.csv                   id,funds_m: each club's exact starting funds (#500)
      strings.csv                 id,text[,lang]: replaces game strings in the LMA_?.lng tables (sponsor
                                  names, ...; `lma_data::lang`, #532)
      sponsors.csv                id,club,shirt_gbp,ground_gbp: each club's real shirt deal per
                                  season, paid weekly; no sponsor_unit: the biggest deal picks it (#552)
      wage_budgets.csv            id,bill_gbp_week,budget_gbp_week: each club's least weekly wage
                                  budget (the importer's wage_headroom x its real bill, #552)
      squad_overflow.csv          id,club_id: players a club owns beyond its 32-id squad array (the
                                  player's club lookup is hooked; not in squad screens, selection or
                                  the wage budget; #538/#546)
      history.csv, cups.csv       league champions and cup finals by club id (db-export's format;
                                  #578, #600)
      last_tables.csv             last season's final tables by club id: a new career's European
                                  entrants (#666)
      competitions/*.toml         rule sets (see below)
      assets/                     optional badges, kits, faces, stadium names
  ```

  The launcher lists mod spaces; choosing one builds its Export.mng in memory and serves it
  through the VFS overlay, so saves are kept per mod space too. Code patches (#31 item 7):
  `base_year` rewrites every `li`/`addi`/`cmpi`/`cmpli` immediate 2006 in `.text` to the year in
  guest memory and sets `guest::rec::BASE_YEAR`, which the recompiler reads at those sites
  (`rec::base_year()`), so dates, season labels, ages and contract years count from it; the
  vanilla game is 2006 in code (61 loads, 3 compares, no data field), and its date validators
  cap years at 2026 = 2006 + the 20 diary seasons (3 compares, 1 load), which move by the same
  offset. The rewrite is by immediate, not by meaning: every `addi`/`cmpi`/`cmpli` with 2006 or
  2026 in `.text` shifts, so a non-year constant would too; the sites, for tracing a surprise:
  2006: 0x820d79d4, 0x820dcce4 (cmpi), 0x820e1978, 0x820ef0c8, 0x820ef5a4, 0x820f00f4, 0x820f0184, 0x820f01ec, 0x820f09e0, 0x820f0b68, 0x820f1474, 0x820f1648, 0x820f16b0, 0x820f17cc (cmpi), 0x820f17e4, 0x820f1890, 0x820f19c0, 0x820f1bec, 0x820f1c18, 0x820f202c, 0x820f2210, 0x820f24cc, 0x820f2578, 0x820f2608, 0x820f265c, 0x820f2700, 0x820f2798, 0x820f2994, 0x820f2a84, 0x820f2df8, 0x820f2f24, 0x820f3848, 0x820f39a8, 0x820f3be0, 0x820f3cf8, 0x820f3d88, 0x820f4014, 0x820f70fc, 0x820f8f34, 0x820f928c, 0x820f9290, 0x820f98b0, 0x820faa34, 0x820fab70, 0x8213dfc0, 0x8213e07c, 0x8213e270, 0x8213e558, 0x8213ed84, 0x82157bcc (cmpi), 0x8215e2f8, 0x8218685c, 0x821a131c, 0x821a339c, 0x821be1a4, 0x821bea80, 0x821bf6e4, 0x821bfe28, 0x821d5034, 0x821d70e8, 0x821fcd5c, 0x82221090, 0x82259de8, 0x822b1e24;
  2026: 0x820efd88 (cmpi), 0x820f0864 (cmpi), 0x820f32d8 (cmpi), 0x8257efec. Not every 2006 is a year:
  0x82221090 loads the resource id 2006 (group 2, item 6, split by 1000 in 0x822f12f8) for the
  match pitch; rewritten to 2026 the pitch lost its texture (sky through the grass), so it is in
  `guest::rec::NOT_YEAR_SITES`, which the recompiler and `patch_image` skip, with 0x820d79d4 (one
  arm of a switch loading the ids 2004..2012 in scrambled order for 0x823fa358). The other li sites
  store or print the year; the cmpi sites are the date validators. `[[patch]]` overwrites one
  word after checking the old one; the mod's own file carries the addresses, the repo none.
  Club records: `clubs.csv` carries `country` (the league's country, byte 3) and `division`
  (byte 7 >> 2, per country: England 0 Premiership, 19 Championship, 20 League One, 21 League
  Two, 11 Conference; Scotland 0-3; Italy 4/5; Spain 6/7; Germany 9/10/17/18; France 1/2/8;
  Netherlands 13/14; Portugal 15/16). **Leagues are club-id ranges** (proven 10 Oct: swapping
  only the two club records and name entries of ids 2 and 41 put Sunderland in the team-select
  Premiership; swapping the division bytes or the fixture ids changed nothing): ids 0-19 are
  the Premiership, 20-43 the Championship, 44-67, 68-91, 92-115 the next tiers, then each
  country's ladders in id order; 1136-1160 are the historic sides. A club moves by exchanging
  whole records between ids (the 84-byte Clubs.bin record with the 32 squad ids, wealth,
  division byte, kit colours, stadium/manager refs; the 26-byte `ClubNam{E,F,G,I,S}.bin` and
  8-byte `ShortNm*.bin` entries), the division byte then set to the destination's; fixtures
  stay. `clubs.csv` `record_of` is that move as a mod edit (#388: the row's id takes the whole
  club at `record_of`, a permutation applied before the other columns, which then name the new
  ids). `Diary20.mng` inside Export.mng holds `diaryNNNN.bin` + `TWNNNN.bin` per season (20,
  2006/07-2025/26): every league fixture is a 6-byte record `home u16 BE, away u16 BE, 0xff,
  0x00` (scores, unplayed) with 1-based club ids, grouped in rounds behind a 10-byte header
  ending in the match count (0x0a for a 20-club league, 0x0c for 24); the separate
  `Misc/Diary20.mng` (one partial season) is never opened. `World.bin` starts with counts
  (1136 clubs, 27290 players, ...) and the competition definitions. Its **league histories**
  (Records > League Champions, #424): tier t of country c (England .. Portugal = 0..7, 5 tiers)
  is 150 rows of 8 bytes at 0x21b0 + c * 0x2aa0 + t * 1200, newest season first, each (club uid
  u16 BE, W, D, L, F, A, Pts), 0 for a season without one; the screen shows row i as the season
  ending in base year - i (the disc's row 0: Chelsea 2005/06). Its **loan table** (#412, #429):
  700 slots of (player id u16, two u16, 0) sorted by player id then free slots (0xFFFF, 0, 0, 0),
  ending 32 bytes before the end of the file in Export.mng and at the very end in ExportPR.mng;
  281 loans on the disc, which the squad screen shows as a greyed "L". Clubs are named by uid
  through `ClubRmap.bin` (8300 u16: uid -> club record index; every unused uid maps to 0, so
  club 0's own uid is the one its history uses, Arsenal's 5800). lma-data exports and applies
  the histories as `history.csv` (by club id). The cup finals sit before the league histories:
  seven cups (`db::CUPS`: European Cup 0x70, UEFA Cup 0x520, Super Cup 0x9d0, FA Cup 0xef0, League
  Cup 0x13a0, Charity Shield 0x1850, League Trophy 0x1d00) of up to 150 eight-byte rows (winner
  uid, runner-up uid, goals, goals, two zero bytes: no shoot-out), newest first, exported and
  applied as `cups.csv` (#600). Last season's final tables follow at 0x17190: 445 u16 uids by
  position, the leagues in the order England, Scotland, Spain, Italy, France, Germany, Netherlands,
  Portugal, top tier first (`db::LAST_TABLE_NATIONS`, `LAST_TABLE_SIZES`). A new career builds its
  European entrants from them as a season end builds them from the final tables (0x8213a918 ->
  0x82138b50 -> 0x821333d0 -> 0x820e1a98, World DB +0x36c64), so the disc's careers and any mod
  without them start with 2005/06's qualifiers. The holders come from the European finals history.
  Exported and applied as `last_tables.csv` (#666); `squad-import` writes the mod's from the base's,
  each ladder league re-made from the clubs the mod puts in it: the latest Transfermarkt season's
  final table first (`--tm-games`, the competitions in `[transfermarkt.history.leagues]`; tied
  points head-to-head in `h2h`, Spain and Italy), the other members in the base's order, cut or
  filled with base rows to the game's size. `LMA_POKE=addr:hexbytes,...` (lma-cli and lma-app, after
  LMA_LOAD) tried a table on newgame_probe3 before any file change. Contract expiry and time at club are generated at
  new game, not stored. A mod folder may carry its
  own `Export.mng` as the base (served instead of the disc's, the CSVs still apply) for tables
  lma-data does not edit yet. Starting funds are code tables of (threshold, funds) words at
  0x82005244 (£3m..£60m) and 0x820053dc, patchable with `[[patch]]`. `base_year = 2026` boots to
  the day-1 hub on WED 1 JUL '26 in the normal time since the validators' 2026 bound moves with
  it (#390); a blanket 2006-only rewrite had made the day-1 update loop.
  **Player value** (what the card shows): the stored u16 at record byte 20 (£1000s) is rewritten by
  the game itself. Setter `f8211dcb0(player, value)` (values of 1000 or less become 0); its caller
  `f8220df78` walks a club's 32 slots, zeroes the field, takes a percentage factor from `f821208e0`
  and stores `100 * base / factor` (doubles, rounded); the factor multiplies percent tables next to
  the funds ladders, among them an age ramp at 0x8200575c (10, 20, ..., 90, 100 x5, 85, 70, 50, 35,
  15, 3, 2, 1), which is why an 18-year-old's £42.5m shows as ~£30m under a 24-year-old's £32m. The
  ramp is `[[patch]]`-able. The factor itself (`f821208e0`, read from the image, 11 Oct): the player's
  overall (`f8211f4c8`, clamped 25..100) x 22 / 100 indexes a 22-word ladder at 0x82005508..0x8200555c
  (287,500 .. 40,000, interpolated), then the position table at 0x8200556c and the age ramp scale it,
  so every shown value is linear in 1/ladder (it is the divisor): fc27final8 patches the 22 words / 3 (values x3, up
  to the stored u16's GBP 65.5m). The caller's x100 float at 0x820014f8 is a literal shared by 60
  sites and code words patched in memory don't reach recompiled code, so the ladder is the data patch point.
  **Value unit** (`value_unit = 5000` in mod.toml, #475, 11 Oct): the stored u16 is GBP 1,000s only through
  three code words, `mulli ..,1000` after the getter `f8211dc58` at 0x8220e200 and 0x8215da98 and the
  setter's `cmpwi ..,1000` at 0x8211dcd8; the recompiler emits `guest::rec::value_unit()` / `value_floor()`
  there and `runtime::mods` rewrites the words for the interpreter (regenerate the parts). The game's own
  valuation works in units, so at 5000 self-priced players come out x5; `tools/value_ladder.py` scales the
  ladder for the rest. The unit also scales the two GBP caps the transfer code hard-wires: the
  valuation's 40m (`VALUE_CAP_SITE`) and the human offer screen's 80m (`OFFER_CAP_SITES`, seven
  `lis/ori` pairs; the offer max is `min(2 x value, cap)`, #523), both `lis/ori` pairs the recompiler
  loads as a whole and `runtime::mods` rewrites for the interpreter. The transfer budget itself is
  `funds x board_share - committed fees` (0x820eb058; `board_share`, a float with one reader, #522).
  Found with `LMA_WATCH` on the field (names the writer) and
  `LMA_RWATCH` (#401, names the readers); never both at once. The day-1 Valuation the Player
  Search card shows is not that ramp: with the nine ramp words flattened to 100 it did not move
  (Olise £32m, Mbappé £32m, Haaland £31m, Pedri £30m, Vini £29m, Yamal £29m). It follows the
  Overall order and is capped by the stored value (vanilla Gerrard shows exactly his stored 30000;
  Pedri and Olise both store 32000 and show 30 and 32), so the top of the market is set by the
  Overall mapping, the stored value only from above. Time at Club and Contract Expiry are generated
  at new game (no Players.bin bit window matches them; Olise shows 14 years at 24).
  **Formations:** the 14 names are a pointer table at 0x827023f0 (4-4-2, 4-2-4, 4-3-3, 5-4-1, 3-4-3,
  3-5-2, 1-4-4-1, 1-3-4-2, 1-2-5-2, 4-3-1-2, 4-4-1-1, 3-4-1-2, 1-3-3-3, 4-2-3-1) and the slot table
  at 0x82710f58 (14 x 11 x u32, stride 44) gives each slot a role: 0 GK, 1 full back, 2 sweeper,
  3 centre back, 4 wing back, 5 central mid, 6 wide mid (LM/RM), 7 attacking mid, 8 centre forward,
  9 wide forward (LF/RF). Six `[[patch]]` words make 4-3-3 LB CB CB RB / CM CM CM / LF CF RF and
  give 4-2-3-1 LF/RF (proven in game; `squad-import --patches` copies them into the mod).
  Since #691 a mod's formations.csv grows both tables to 16 rows and renames or changes rows, so
  shapes are added rather than rewritten; the tactics screens show the pointer table's name
  (getter 0x82326280) and the strings 3322-3335 serve one caption through an id list (#749).
  Two more per-formation tables follow the shape (#749): the diagram's grid spots (0x8200b370) and
  the team-sheet order (0x8202cd38: slot -> place, searched by 0x82326220, which the match engine
  calls too); both are copied into the formations region with the rows added.
  **Badges:** `Common/Frontend/Textures/Badge.mng` (BG2, 323 entries of 34,048 bytes: TIM2, 128 x 256
  8-bit pixels with the crest in the top half, 256-colour CLUT in PS2 order; `tools/badge_tm2.py`
  converts) is addressed by entry number, never by name. At database load (0x822f4400) every club
  gets its country's crest, entry 200 + country (England 214), and the 72 clubs of a hard-coded
  uid table at 0x82712ad8 (the licensed Championship to League Two clubs of 2006/07) get
  [0x82712964] = 251 + i, through the setter 0x820e39d8(club uid, entry); the entry lives in a
  9-bit field of the in-memory club record (+0x58 bits 12..20), whose only reader is the getter
  0x820e3970 (the save keeps the same 9 bits). Club uids map to record indexes through the u16 table at
  [0x8273d6c8]. A mod's `badges/<ClubName>.tm2` replaces the disc's entry of that name in place,
  takes one of the 192 `ft_*` shield slots (8..199: no code path gives a club one) or becomes a
  new entry (323 onwards), in club order, and the runtime's hooks on the setter (`hooks::badge_set`) and the getter
  (`hooks::badge_get`, so entries above 511 work: 722 badges in fc27final22, 290 of them were left out before) point the club at
  it (`mods::ModSpace::build_badges`); the recompiler keeps direct calls off hooked addresses, so
  regenerate the parts after a hook address is added. Proven in game: appended entries load.
- **The 2026/27 squad import** (`lma-cli squad-import <db-export> <roster> <mapping> <mods> <name>
  [options]`, `lma-import`, #31). It writes a mod space: a sparse `players.csv` and `clubs.csv`, a
  `mod.toml`, `clubs_fm.csv` (club id, shown name, FM uid: badge packs are keyed by it) and
  `import-report.txt`. `mappings/fc27.toml` maps the sources and holds no data. The mod folder the
  game loads is then `mods/<name>/`: `mod.toml` (`base_year = 2026`, `[[patch]]` words, e.g. the
  16 wealth-tier funds words at 0x82703118: x4 in #447, per club from squad value in #451; the shown
  funds are tier word x 10,000 x a per-club factor, 0.15 for Arsenal, Forest, Palace and Man City
  (149,998,500) but 0.2 for Chelsea (GBP 160m at x4): the factor is `FUNDS` 0x820eaa10's, England's nation byte 150 x 10 for everyone and a hard-coded x 2,000 for Chelsea's uid, see the #704 audit row), `database/*.csv` (players, clubs, history),
  `clubs_fm.csv`, `badges/<ClubName>.tm2` (made by `tools/badge_pack.py <FMG pack.zip>
  <clubs_fm.csv> <mods/NAME/badges>`; `tools/badge_tm2.py selftest` checks the encoder). The inputs:

  | input | mapping | gives |
  |---|---|---|
  | `<roster>`: EA FC 27's ratings | `[columns]`, `[positions]`, `[sides]`, `[club_aliases]` (`"#id"` for one of two namesakes) | the squads of the clubs LMA has; positions (an alternate's second line counts), sides, foot |
  | `--fm`: `fmsave export ... players --all` + `tools/fm_skin.py` | `[fm]`, `[fm.skin]`, `[fm.fill]` | attributes, skin tone; as a roster: clubs FC 27 lacks, takeovers of LMA clubs with no source, free agents (women's sections left out by squad height) |
  | `--tm`: Transfermarkt players (dcaribou, CC0) | `[transfermarkt]` | the level (with FC 27's overall, `overall_weight`, onto vanilla's spread per group), the line, the stored value (`value_scale`) |
  | `--staff`: fmsave staff export | `[fm.managers]` | each club's manager (role 16, else a caretaker), by FM club uid |
  | `--tm-clubs`: Transfermarkt clubs | `[transfermarkt.stadiums]`, `[stadium_renames]` | stadium name and seats from the club most of its players are at (else by name); grounds replaced since 2006 renamed by hand |
  | `--tm-lineups`: Transfermarkt game lineups | `[transfermarkt.lineups]` | shirt numbers: each player's latest at his club |
  | - | `real_squads = true` | each club keeps its real squad up to 32 (the club record's id array), taking spare player records instead of dropping or loaning out the surplus (#538, #541) |
  | - | `live_add_missing = true`, `live_add_min_value_eur` | players at an LMA club that no LMA record has (Max Dowman) take spare records, rated from the 5 nearest same-line players; skipped when the surname matches within a year anywhere or the birth date matches at the same club (#571) |
  | `--history-extra`: `country,tier,season,club,won,drawn,lost,for,against,points` | - | fills the history rows no Transfermarkt season covers (2006/07-2011/12), matched by club name (#578) |
  | `--tm-games` finals, `--tm-events`: Transfermarkt game events | `[transfermarkt.history.cups]`, `round`, `game_id` | `cups.csv`: the base's finals moved to base_year, each cup's Transfermarkt final (round "Final") on its row; a final with a shoot-out scored in play from the events (Transfermarkt's goals count the kicks); a side not ours leaves the final out (#600) |
  | `--cup-extra`: `competition,season,winner,runner_up,winner_goals,runner_up_goals` | - | finals no source covers (2006/07-2011/12, missing League Cups), by club name; `competition` a Transfermarkt id of the cups table or a cup's name (#600) |
  | - | `[strings]` | game strings by LMA_?.lng index (`"396" = "Champions League"`, `"30:E"` English only), merged into the mod's strings.csv with the rows already there (sponsor brands); the game's own sentences are rewritten in the local mod only (#611) |
  | `--staff`: FM staff | `[fm.managers] reputation = [lo, hi]` | each club's manager's reputation (Managers.bin bits 70-76, 0-99, the stars) from FM ability, lo -> 1, hi -> 99; a manager FM lacks gets 20 + 5 x his club's wealth tier; written as clubs.csv `manager_rep` (#628) |
  | `--tm` | - | a Transfermarkt candidate on the other side of the goalkeeper line needs a shared forename word too ("Amigo" took Enzo Fernandez's value, #609); names fold every accented letter before matching (ı, ğ, ř, đ, ő, ă, ș, ą, ...: #615); value outliers (EUR 20 m+, average rating under 50) listed in the report |
  | - | `[club_codes]` | every club's short names a three-letter code (the scorebug showed 2006's "Ars'nl"): the table's, else the first word that isn't a prefix (#600) |
  | - | `squad_overflow = true` | with `real_squads`, players beyond 32 go to spare records outside the squad array, listed in `squad_overflow.csv` (#546) |
  | `--transfers`: a local `player,club` file | - | moves no source has yet, applied to the final squads |
  | `--patches`: any toml with `[[patch]]` tables | - | copied into mod.toml (the repo holds no addresses) |
  | `--budgets`: `club,budget_gbp` (FC 27 career budgets) | `budgets_scale`, `transfer_share`, `funds_factors`, `wealth_budgets` | each club's funds target (budget / 0.65, the board's transfer share); `funds_targets.csv` for `tools/funds_fit.py` (#485) or as the mod's `funds.csv` (#500) |
  | `--weights`: the game's overall weight table (8 roles x 13 ratings, from the user's game) | `overall_from_source` | ratings shifted until the game's own overall is the source's (#489) |
  | `[fm] wage`, `contract_end`; `[transfermarkt] contract_end` | - | `wages.csv`: FM26 weekly wages (else a wage/value ratio), contract seasons from Transfermarkt's expiry (#487) |

  FM 26's own database (`people_db.raw` of the `2620_fm` database, decompressed) has the save's person
  layout: u32 name length, the name, then the birth date as u16 day of year + u16 year; from that date,
  +8 is the skin tone (1-20; as `tools/fm_skin.py` reads the save) and +5 FM's ethnicity code (0 Northern
  European, 1 Mediterranean, 2 North African, 3 African/Caribbean, 9 mixed; 83-96% by region, 13 Oct).
  Matching by name + birth date finds 37,466 players. No byte in -64..+256 of the anchor behaves like a
  hair colour against players with known hair, so hair lies in a variable-length part or another table.

  Record fields written besides names, ratings, positions and height: `skin` (bit 276), `value`
  (u16 at byte 20, in `value_unit` GBP, a fixed +-8% per player), `sides` (bits 221-223), `foot`
  (bits 156-157), `hair` (bits 268-271, 0 = bald; from vanilla's mix, #497/#498); a single-name
  player's forename is cleared, never inherited. `[[ladders]]` re-seat the leagues as id ranges
  (`record_of` moves the record, names, stadium and manager): FC 27's clubs take their league's
  places, new clubs take a retired 2006 club's record as their shell, every division keeps its
  size. Clubs no source covers play with FM's free agents, so no record keeps a 2006 player;
  db-import clears the World.bin loan of every record given another player. A full name pool
  (the surname id is 15 bits) reuses the slots nothing uses. `base_year` goes into mod.toml.
  `import-report.txt` lists every match, move, takeover, new club, transfer, and the clubs keeping
  the database's manager or stadium. A full import takes ~13 s on the PC (~6 s without lineups).
- **The PS2 database** (#3, #324): `lma-data` reads and writes the PS2 disc's `Export.mng` too
  (`MG2`, little-endian, LSB-first bit fields, packed club/stadium names, 24-byte managers, its
  own nation ids), byte-identical on rebuild; `lma-cli db-export`/`db-import` take either file.
  `LMA_PS2_EXPORT=<path>` runs the PS2 real-file tests. The port itself runs the 360 game only.
- **Competition rule sets** (`competitions/*.toml`): league size, points, tiebreakers, promotion/
  relegation, cup rounds, and modern formats such as the UEFA Champions League / Europa League
  36-team league phase + knockout play-offs. Phase 1 maps them onto the fixture/competition
  tables the game already has (World.bin); formats the original engine cannot express get
  their own scheduler in the port, which is only possible because we own the runtime.
- **Custom teams and players**: new clubs and players beyond the 1,161/27,290 slots by
  re-using placeholder records first, then growing the tables once their in-game limits are
  mapped.

## Milestones

1. **Interpreter bring-up** – execute guest code from the entry point in Rust with stub
   kernel calls; log every import the game reaches. (Interpreter first: it is the reference
   the recompiler is tested against.)
2. **Kernel/XAM HLE** – memory, threads, events, file I/O over the extracted game folder,
   until the game's main loop is running headless.
3. **Recompiler** – generate Rust for all 14,571 functions; diff against the interpreter.
4. **GPU** – Xenos PM4 packets, render targets, shader microcode → WGSL, presented by Bevy.
5. **Audio + input** – XMA, XInput mapped from Bevy gamepad/keyboard.
6. **Mod layer** – load edited databases from a mods folder; in-app editor using `lma-data`.

Milestones 4–5 are the bulk of the work (Xenia's GPU code is the reference).

## Recompiler workflow

```
cargo build --release -p recomp
# optional: record pcs that still run in the interpreter (jump tables, function pointers)
LMA_NORENDER=1 LMA_MISSES=1 target/release/lma-cli run <default.xex> 1500000000   # writes misses.txt
target/release/lma-recomp <default.xex> all crates/game-blocks 999999 32 misses.txt
cargo build --release -j 8 -p lma-cli -p lma-app      # ~3 min, <1 GB per part crate
LMA_INTERP=1 ...                                       # run without recompiled code (reference)
LMA_INTERP_STATS=1 ...                                 # instructions still run through rec::interp, by mnemonic
# (lma-cli's "dispatches outside recompiled code" also counts kernel import thunks, not just interp)
```
Recompiled code runs every instruction natively (#235): the night match runs none in the
interpreter. What still calls `rec::interp` is data decoded in code gaps (opcode 0) and a trap
that fires. Vector instructions without a dedicated helper call the interpreter's own VMX routine
directly (`rec::vx`), so they give the same results by construction.

Shape of the generated code (#6 rework), each switch read by `lma-recomp` when it writes the parts:
- The slice budget lives in a function local (`bv`): calls get the parameter written back first
  and read after, and returns and errors break to the function's one exit, which writes it back
  (#331). `LMA_RECOMP_BUDGET_LOCAL=0` emits the old `*b` code.
- A `bl` to a function in the same part crate is a direct Rust call (`call_local`), and a branch
  into another function's start enters it with `call` (#146, #164; on by default since #334).
  `LMA_RECOMP_V2=0` turns both off. Import thunks and hooked functions stay on the table.
- The IR emitter (#145 M1, `LMA_RECOMP_IR=1`): each instruction is decoded into an `Op` with the
  registers it reads and writes (`recomp/src/ir.rs`; `live.rs` has the blocks, gen/kill and
  liveness), and a block is emitted over host locals (`emit.rs`): the registers it reads are
  loaded once at entry, the integer ops compute on the locals through `guest::native` (value
  functions with the interpreter's exact semantics), helpers get the locals they read stored
  before and the ones they write reloaded after, and every exit (the budget checks between
  leaders, the tail) stores back what was written, so `Cpu` is exact wherever anything else
  can look (dispatcher, calls, helpers, the interpreter, save states). Budget counts, check
  points and the schedule are unchanged. Property tests pin the IR to the interpreter: nothing
  outside an op's defs changes and nothing outside its uses matters
  (`ir::tests::ops_touch_what_the_interpreter_touches`), and the native evaluator leaves the
  interpreter's registers and memory (`native_eval_matches_the_interpreter`).
- Tried and not kept (9 Oct, branches kept): direct calls across part crates via `#[no_mangle]`
  and `extern "Rust"` (`mac/cross-crate-calls`, -0.5%: `rec::call`'s cost is its bookkeeping, not
  the lookup), a timebase computed on read (`mac/lazy-timebase`, no time gain), and broad guest
  register caching in locals (M0, LLVM spills them). LLVM already keeps `c.gpr` values across
  guest stores (`&mut Cpu` is noalias); reloads come after opaque calls.
- Tried and not kept (10 Oct, branches kept, numbers on the issues): a structured if/else entry
  for loop-free functions instead of the `match pc` dispatcher (`mac/recomp-leaf`, 82% of functions,
  bit-exact, +1.2% day / +0.6% night: the dispatcher isn't the cost, the registers in `c.gpr` are;
  #145), the last vector fallbacks natively (`mac/vmx-rest`: lvsl/lvsr/vminfp/vmaxfp/vrfin/vupkd3d
  were 0.006% of the day match's instructions, +1.2% / +0.4%; #367), an idle nap gated on an idle
  GPU (`mac/idle-nap`: no CPU saving, the title at 22 fps; #312), elastic frame pacing (no gain
  while the game averages under 60; #6), and the Apple write watch for landings (#355: a fault
  marks all of a 16 KB host page, so landed bands sharing one with guest writes were decoded
  again every frame; with landings hashed the branch is at parity, so it stays parked).
Measure a change with interleaved runs of the night segment against a build from the same
recompiler with the switch off, and judge time only on a quiet machine (host instructions
retired, from `/usr/bin/time -l`, don't depend on load; macOS only). On the PC, judge by
interleaved wall time with no CI job running (`tools/build.ps1` waits for them). PGO play builds
gain less from layout work, since PGO already moves cold paths out: #331+#332+#334 gave day
-5.8%, match -4%, day-match -2.5% on the PC's play build #10 against #8.

Debugging a divergence between two deterministic runs (A/B builds, a resumed state): diff the
kernel-call logs (`last_run.log`, one `[clock] tN Call` line per call, except the frequent
RtlEnter/Leave/TryEnterCriticalSection) after the point where
they split; the first line whose clock differs names the thread and the call before it. Don't
compare against `LMA_WATCH`/break runs: they leave the recompiled fast path, and the
interpreter's time base advances per instruction instead of per function entry.

Generated code (`crates/game-blocks/parts`, `src/lib.rs`, `Cargo.toml`) comes from the user's own
disc and is git-ignored.
