//! Real-time journal (#108): what host timing decides in a real-time session, keyed by the
//! instruction clock, so the session replays exactly. LMA_RECORD writes it next to the recording
//! (`path.journal`); LMA_REPLAY=path plays the session back from `path.state`.
//!
//! A real-time run differs from a deterministic one only in what the host decides: guest time
//! (the host clock that `Machine::sync_time` reads at the start of every slice and while idle), the
//! adaptive frame skip (skipped frames skip their resolves, which land in guest memory), when
//! asynchronous GPU fences land, and when the player's pad changes. While recording, GPU fences
//! are synchronous (as in deterministic runs) and host time is quantized to `Q` ticks (0.66 ms), so
//! only its changes need writing; the rest is journaled as text, one event per line:
//!
//! ```text
//! t <dclock> <seq> <q>        guest time base Q*q at the seq-th sync_time call at this clock
//! s <dclock> <seq> <every>    render_every after an adaptive frame-skip change
//! p <dclock> <render> <buttons> <lt> <rt> <lx> <ly> <rx> <ry>   lma-app's pad and rendering switch
//! l <dclock> <n>              a state load (F9 / `load`): the state was copied to `path.load<n>.state`
//! e <dclock>                  a state was saved, or the session stopped: the recorded time holds up to here
//! ```
//!
//! `dclock` is relative to the previous line (negative after a load that went back in time). Replay consumes the same calls in the same order;
//! an event that is never reached counts as a divergence.

use crate::input::Pad;
use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// The game reads and writes the user folder (profile, saves, cache), so a recording keeps it as
/// it was at the start, in `<rec>.userdata` (on APFS the copy is a cheap clone). Call before the
/// machine touches the folder.
pub fn snapshot_userdata(userdata: &Path, rec: &str) -> std::io::Result<()> {
    let to = PathBuf::from(format!("{rec}.userdata"));
    if to.exists() {
        std::fs::remove_dir_all(&to)?;
    }
    if userdata.is_dir() { crate::content::copy_dir(userdata, &to) } else { std::fs::create_dir_all(&to) }
}

/// The user folder to replay `rec` with: a fresh copy of its snapshot (the replay writes saves
/// again), or None for recordings made before snapshots (they replay exactly only with the same
/// files in ./userdata).
pub fn replay_userdata(rec: &str) -> std::io::Result<Option<PathBuf>> {
    let from = PathBuf::from(format!("{rec}.userdata"));
    if !from.is_dir() {
        return Ok(None);
    }
    let to = std::env::temp_dir().join(format!("lma-replay-userdata-{}", std::process::id()));
    if to.exists() {
        std::fs::remove_dir_all(&to)?;
    }
    crate::content::copy_dir(&from, &to)?;
    Ok(Some(to))
}

/// Host time granularity while recording, in guest ticks (20 ns): 2^15 = 0.66 ms.
pub const Q: u64 = 1 << 15;

enum Event {
    Time(u64),
    Skip(u64),
    /// A state load: events after it belong to the loaded state's timeline (clocks restart lower),
    /// so nothing past it is matched or dropped until the replay has loaded the state.
    Load,
}

/// What lma-app does between runs: set the pad and rendering switch, or load a state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ui {
    Pad(bool, Pad),
    Load(u32),
}

struct Entry {
    clock: u64,
    seq: u32,
    ev: Event,
}

pub struct Journal {
    out: Option<std::io::BufWriter<std::fs::File>>,
    events: VecDeque<Entry>,
    pads: VecDeque<(u64, Ui)>,
    /// Clock of the previous line written (deltas).
    line_clock: u64,
    /// sync_time calls so far at `seq_clock`.
    seq_clock: u64,
    seq: u32,
    q: u64,
    every: u64,
    pad: Option<(bool, Pad)>,
    /// Replay: events passed without being reached (0 = the replay is exact so far).
    pub diverged: u64,
    /// Replay: the clock of the journal's last line.
    end: Option<u64>,
    /// Replay: the build that recorded the journal (`# build` line; None in older journals).
    pub recorded_build: Option<String>,
}

impl Journal {
    fn new() -> Self {
        Journal {
            out: None,
            events: VecDeque::new(),
            pads: VecDeque::new(),
            line_clock: 0,
            seq_clock: u64::MAX,
            seq: 0,
            q: 0,
            every: 1,
            pad: None,
            diverged: 0,
            end: None,
            recorded_build: None,
        }
    }

    /// Replay: a warning when this binary is not the build that recorded the journal (a replay
    /// is exact only on that build; play builds are kept in ~/lma/play/builds/<build>/).
    pub fn build_warning(&self) -> Option<String> {
        let rec = self.recorded_build.as_deref().unwrap_or("unknown (journal predates build ids)");
        (rec != crate::BUILD).then(|| format!("recorded on build {rec}, this is build {}: the replay will diverge; use that build's binaries", crate::BUILD))
    }

    /// Start recording to `path`.
    pub fn record(path: &str) -> std::io::Result<Self> {
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(out, "# lma real-time journal v1 (runtime/src/journal.rs)")?;
        // a journal replays exactly only on the build that recorded it
        writeln!(out, "# build {}", crate::BUILD)?;
        Ok(Journal { out: Some(out), ..Self::new() })
    }

    /// Load a journal for replay.
    pub fn replay(path: &str) -> std::io::Result<Self> {
        let mut j = Self::new();
        let mut clock = 0u64;
        let bad = |n: usize| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{path}:{n}: bad journal line"));
        for (n, line) in std::io::BufReader::new(std::fs::File::open(path)?).lines().enumerate() {
            let line = line?;
            if let Some(b) = line.strip_prefix("# build ") {
                j.recorded_build = Some(b.trim().to_string());
            }
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<i64>().ok()).ok_or_else(|| bad(n + 1));
            clock = clock.wrapping_add(num(1)? as u64);
            match f[0] {
                "t" => j.events.push_back(Entry { clock, seq: num(2)? as u32, ev: Event::Time(num(3)? as u64 * Q) }),
                "s" => j.events.push_back(Entry { clock, seq: num(2)? as u32, ev: Event::Skip(num(3)? as u64) }),
                "p" => {
                    let pad = Pad {
                        buttons: u16::from_str_radix(f.get(3).ok_or_else(|| bad(n + 1))?, 16).map_err(|_| bad(n + 1))?,
                        left_trigger: num(4)? as u8,
                        right_trigger: num(5)? as u8,
                        thumb_lx: num(6)? as i16,
                        thumb_ly: num(7)? as i16,
                        thumb_rx: num(8)? as i16,
                        thumb_ry: num(9)? as i16,
                    };
                    j.pads.push_back((clock, Ui::Pad(num(2)? != 0, pad)));
                }
                "l" => {
                    j.pads.push_back((clock, Ui::Load(num(2)? as u32)));
                    j.events.push_back(Entry { clock, seq: u32::MAX, ev: Event::Load });
                }
                // where the recording saved a state or stopped: the journal's end, so a replay keeps
                // the recorded time up to there (nothing to match)
                "e" => {}
                _ => return Err(bad(n + 1)),
            }
            j.end = Some(clock);
        }
        Ok(j)
    }

    pub fn is_replay(&self) -> bool {
        self.out.is_none()
    }

    /// Replay: nothing left to play back.
    pub fn finished(&self) -> bool {
        self.is_replay() && self.events.is_empty() && self.pads.is_empty()
    }

    fn line(&mut self, clock: u64, text: std::fmt::Arguments) {
        if let Some(out) = self.out.as_mut() {
            let _ = writeln!(out, "{} {} {}", &text.to_string()[..1], clock.wrapping_sub(self.line_clock) as i64, &text.to_string()[2..]);
            self.line_clock = clock;
        }
    }

    /// Number of sync_time calls so far at `clock`.
    fn calls(&self, clock: u64) -> u32 {
        if self.seq_clock == clock { self.seq } else { 0 }
    }

    /// Drop replay events that lie before (clock, seq): they were never reached.
    fn skip_missed(&mut self, clock: u64, seq: u32) {
        while self.events.front().is_some_and(|e| !matches!(e.ev, Event::Load) && (e.clock, e.seq) < (clock, seq)) {
            let e = self.events.pop_front().unwrap();
            self.diverged += 1;
            if self.diverged <= 3 && std::env::var_os("LMA_JOURNAL_DEBUG").is_some() {
                eprintln!("journal: missed ({}, {}) at ({clock}, {seq})", e.clock, e.seq);
            }
        }
    }

    /// `Machine::sync_time`: the host clock to use. Recording quantizes `host` and writes its
    /// changes; replay returns what the recording used at this call.
    pub fn time(&mut self, clock: u64, host: u64) -> u64 {
        let seq = self.calls(clock);
        (self.seq_clock, self.seq) = (clock, seq + 1);
        if self.is_replay() {
            self.skip_missed(clock, seq);
            if let Some(Entry { ev: Event::Time(q), .. }) = self.events.front().filter(|e| (e.clock, e.seq) == (clock, seq)) {
                self.q = *q;
                self.events.pop_front();
            }
        } else {
            let q = host / Q * Q;
            if q != self.q {
                self.q = q;
                self.line(clock, format_args!("t {seq} {}", q / Q));
            }
        }
        self.q
    }

    /// Recording: render_every after an adaptive frame-skip step.
    pub fn record_skip(&mut self, clock: u64, every: u64) {
        if every != self.every {
            self.every = every;
            let seq = self.calls(clock);
            self.line(clock, format_args!("s {seq} {every}"));
        }
    }

    /// Replay: the render_every to switch to here, if the recording changed it at this point.
    pub fn replay_skip(&mut self, clock: u64) -> Option<u64> {
        let seq = self.calls(clock);
        self.skip_missed(clock, seq);
        match self.events.front() {
            Some(Entry { clock: c, seq: s, ev: Event::Skip(e) }) if (*c, *s) == (clock, seq) => {
                let e = *e;
                self.events.pop_front();
                Some(e)
            }
            _ => None,
        }
    }

    /// Recording: lma-app's pad and rendering switch as set before running from `clock`.
    pub fn record_pad(&mut self, clock: u64, render: bool, p: Pad) {
        if self.pad != Some((render, p)) {
            self.pad = Some((render, p));
            self.line(
                clock,
                format_args!(
                    "p {} {:x} {} {} {} {} {} {}",
                    render as u8, p.buttons, p.left_trigger, p.right_trigger, p.thumb_lx, p.thumb_ly, p.thumb_rx, p.thumb_ry
                ),
            );
        }
    }

    /// Recording: lma-app loads state number `n` (copied next to the recording) before running
    /// from `clock`. Afterwards guest time and the instruction clock are the state's.
    pub fn record_load(&mut self, clock: u64, n: u32) {
        self.line(clock, format_args!("l {n}"));
        self.seq_clock = u64::MAX;
        self.flush();
    }

    /// Recording: lma-app saved a state or stops at `clock`. The time is only written when it
    /// changes (every `Q`), so without this the journal ended at its last change and a replay ran
    /// the rest on host time: a state saved just before quitting didn't replay (#296).
    pub fn record_end(&mut self, clock: u64) {
        if self.is_replay() {
            return;
        }
        self.line(clock, format_args!("e "));
        self.flush();
    }

    /// Replay: the next pad change or state load due at `clock`, in recorded order (call until
    /// None; after a load the clock is the loaded state's).
    pub fn replay_ui(&mut self, clock: u64) -> Option<Ui> {
        if self.pads.front().is_some_and(|p| p.0 <= clock) {
            let (_, ui) = self.pads.pop_front().unwrap();
            if matches!(ui, Ui::Load(_)) {
                self.seq_clock = u64::MAX;
                // pass the load's barrier (anything before it that was never reached diverged)
                while let Some(e) = self.events.pop_front() {
                    if matches!(e.ev, Event::Load) {
                        break;
                    }
                    self.diverged += 1;
                }
            }
            return Some(ui);
        }
        None
    }

    /// Replay: the pad and rendering switch for `clock` (the last change at or before it), if any
    /// change is due. Stops at a state load (see `replay_ui`).
    pub fn replay_pad(&mut self, clock: u64) -> Option<(bool, Pad)> {
        let mut due = None;
        while let Some((_, Ui::Pad(r, p))) = self.pads.front().filter(|p| p.0 <= clock).copied() {
            self.pads.pop_front();
            due = Some((r, p));
        }
        due
    }

    /// Replay: the clock of the next pad change (run no further before applying it).
    pub fn next_pad_clock(&self) -> Option<u64> {
        self.pads.front().map(|p| p.0)
    }

    /// Replay: the clock of the last journaled event (run at least past it before `finished`).
    pub fn end_clock(&self) -> Option<u64> {
        self.end
    }

    /// Replay: nothing left to play back and `clock` has reached the journal's end (its last line:
    /// a time change, or the end mark of a save or the stop). Up to there the recorded time holds;
    /// past it the session ran no further, so real time may take over (#296).
    pub fn played_out(&self, clock: u64) -> bool {
        self.finished() && self.end.is_none_or(|e| clock >= e)
    }

    pub fn flush(&mut self) {
        if let Some(out) = self.out.as_mut() {
            let _ = out.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_load_replays_in_order_with_a_barrier() {
        let path = std::env::temp_dir().join(format!("lma_journal_load_{}.journal", std::process::id()));
        let path = path.to_str().unwrap();
        let mut rec = Journal::record(path).unwrap();
        // before the load: clocks 1000..3000
        rec.time(1000, 5 * Q);
        rec.record_pad(1500, true, Pad { buttons: 1, ..Default::default() });
        rec.time(3000, 6 * Q);
        // a load at 3500 takes the clock back to 1200 (time restarts lower too)
        rec.record_load(3500, 1);
        rec.time(1200, 2 * Q);
        rec.record_pad(1300, true, Pad { buttons: 2, ..Default::default() });
        rec.time(2000, 3 * Q);
        rec.flush();
        drop(rec);
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.lines().any(|l| l.starts_with("t -")), "the first line after the load has a negative delta:\n{text}");

        let mut rep = Journal::replay(path).unwrap();
        assert_eq!(rep.time(1000, 0), 5 * Q);
        assert_eq!(rep.replay_ui(1500), Some(Ui::Pad(true, Pad { buttons: 1, ..Default::default() })));
        // calls past the last pre-load event (no line was written) must not swallow the
        // post-load events, whose clocks are lower: the barrier holds them back
        assert_eq!(rep.time(3000, 0), 6 * Q);
        assert_eq!(rep.time(3200, 0), 6 * Q, "no line at 3200: the time stays");
        assert_eq!(rep.time(3400, 0), 6 * Q);
        assert_eq!(rep.diverged, 0, "the post-load events (clocks 1200..2000) were not dropped");
        assert_eq!(rep.next_pad_clock(), Some(3500), "the load is the next thing to stop at");
        assert_eq!(rep.replay_ui(3500), Some(Ui::Load(1)));
        // after the load: the post-load events match at their (lower) clocks
        assert_eq!(rep.time(1200, 0), 2 * Q);
        assert_eq!(rep.replay_ui(1300), Some(Ui::Pad(true, Pad { buttons: 2, ..Default::default() })));
        assert_eq!(rep.time(2000, 0), 3 * Q);
        assert!(rep.finished());
        assert_eq!(rep.diverged, 0);
        assert_eq!(rep.end_clock(), Some(2000), "the end is the last line, not the largest clock");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn record_then_replay_gives_the_same_times() {
        let path = std::env::temp_dir().join(format!("lma_journal_test_{}.journal", std::process::id()));
        let path = path.to_str().unwrap();
        // calls: (clock, host)
        let calls = [(100, 5 * Q + 7), (100, 5 * Q + 9), (100, 6 * Q), (20_100, 6 * Q + 1), (20_100, 9 * Q), (40_000, 9 * Q + 3)];
        let mut rec = Journal::record(path).unwrap();
        let mut recorded = Vec::new();
        for (i, &(c, h)) in calls.iter().enumerate() {
            recorded.push(rec.time(c, h));
            if i == 4 {
                rec.record_skip(c, 2); // after the second call at 20_100
                rec.record_pad(30_000, true, Pad { buttons: 0x1000, thumb_lx: -5, ..Default::default() });
            }
        }
        rec.flush();
        drop(rec);
        let mut rep = Journal::replay(path).unwrap();
        let mut replayed = Vec::new();
        for (i, &(c, _)) in calls.iter().enumerate() {
            replayed.push(rep.time(c, 0));
            assert_eq!(rep.replay_skip(c), if i == 4 { Some(2) } else { None });
            if i == 4 {
                assert_eq!(rep.next_pad_clock(), Some(30_000));
                assert_eq!(rep.replay_pad(29_999), None);
            }
        }
        assert_eq!(recorded, replayed);
        let (render, pad) = rep.replay_pad(30_000).unwrap();
        assert!(render);
        assert_eq!((pad.buttons, pad.thumb_lx), (0x1000, -5));
        assert!(rep.finished());
        assert_eq!(rep.diverged, 0);
        let _ = std::fs::remove_file(path);
    }

    /// A state saved (or the session stopped) after the last time change: the journal ends at the
    /// mark, not at the change, so a replay holds the recorded time up to the save (#296). An idle
    /// end (the clock can't pass the mark) counts as played out at the mark itself.
    #[test]
    fn the_end_mark_extends_the_journal() {
        let path = std::env::temp_dir().join(format!("lma_journal_end_{}.journal", std::process::id()));
        let path = path.to_str().unwrap();
        let mut rec = Journal::record(path).unwrap();
        rec.time(1000, 5 * Q);
        rec.time(2000, 5 * Q + 1); // same quantum: no line
        rec.record_end(2500);
        drop(rec);
        let mut rep = Journal::replay(path).unwrap();
        assert_eq!(rep.end_clock(), Some(2500));
        assert_eq!(rep.time(1000, 0), 5 * Q);
        assert!(rep.finished() && !rep.played_out(1000), "events done, but the recorded time holds to the mark");
        assert_eq!(rep.time(2000, 0), 5 * Q);
        assert_eq!(rep.time(2499, 0), 5 * Q);
        assert!(!rep.played_out(2499));
        assert!(rep.played_out(2500) && rep.diverged == 0);
        let _ = std::fs::remove_file(path);
    }

    /// A journal without an end mark (older recordings) is played out at its last time change.
    #[test]
    fn without_an_end_mark_the_last_change_is_the_end() {
        let path = std::env::temp_dir().join(format!("lma_journal_noend_{}.journal", std::process::id()));
        let path = path.to_str().unwrap();
        let mut rec = Journal::record(path).unwrap();
        rec.time(1000, 5 * Q);
        rec.time(3000, 7 * Q);
        rec.flush();
        drop(rec);
        let mut rep = Journal::replay(path).unwrap();
        assert_eq!(rep.time(1000, 0), 5 * Q);
        assert!(!rep.played_out(2000), "an event is still ahead");
        assert_eq!(rep.time(3000, 0), 7 * Q);
        assert!(rep.played_out(3000));
        let _ = std::fs::remove_file(path);
    }

    fn journal_path(tag: &str) -> String {
        std::env::temp_dir().join(format!("lma_journal_{tag}_{}.journal", std::process::id())).to_str().unwrap().to_string()
    }

    /// An event the replay never reaches (a call at its clock never comes) is dropped and counted;
    /// the time it carried is not applied, so the replay keeps the previous time.
    #[test]
    fn a_missed_event_counts_as_a_divergence_and_is_not_applied() {
        let path = journal_path("missed");
        let mut rec = Journal::record(&path).unwrap();
        rec.time(1000, 5 * Q);
        rec.time(2000, 7 * Q);
        rec.time(3000, 9 * Q);
        drop(rec);
        let mut rep = Journal::replay(&path).unwrap();
        assert_eq!(rep.time(1000, 0), 5 * Q);
        // the replay went from 1000 straight to 3000: the change at 2000 was never reached
        assert_eq!(rep.time(3000, 0), 9 * Q);
        assert_eq!(rep.diverged, 1);
        assert!(rep.finished());
        // a second call at the same clock is a later seq: no event for it, the time stays
        assert_eq!(rep.time(3000, 123), 9 * Q);
        assert_eq!(rep.diverged, 1, "calls past the last event are not divergences");
        let _ = std::fs::remove_file(path);
    }

    /// Two sync_time calls at one clock are told apart by their seq; a replay that makes fewer
    /// calls at that clock misses the second one.
    #[test]
    fn calls_at_one_clock_are_matched_by_seq() {
        let path = journal_path("seq");
        let mut rec = Journal::record(&path).unwrap();
        assert_eq!(rec.time(100, 5 * Q), 5 * Q);
        assert_eq!(rec.time(100, 6 * Q), 6 * Q);
        assert_eq!(rec.time(100, 6 * Q + 5), 6 * Q, "quantized: no change, no line");
        drop(rec);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().filter(|l| l.starts_with("t ")).count(), 2, "{text}");
        let mut rep = Journal::replay(&path).unwrap();
        assert_eq!(rep.time(100, 0), 5 * Q);
        assert_eq!(rep.time(100, 0), 6 * Q);
        assert_eq!(rep.time(100, 0), 6 * Q);
        assert!(rep.finished() && rep.diverged == 0);
        let mut rep = Journal::replay(&path).unwrap();
        assert_eq!(rep.time(100, 0), 5 * Q);
        assert_eq!(rep.time(200, 0), 5 * Q, "the second call at 100 never came: its time is not applied");
        assert_eq!(rep.diverged, 1);
        let _ = std::fs::remove_file(path);
    }

    /// `replay_pad` hands out pad changes up to the clock but stops at a state load, which only
    /// `replay_ui` passes (lma-app loads the state first); after the load the lower clocks match.
    #[test]
    fn replay_pad_stops_at_a_load() {
        let path = journal_path("padload");
        let mut rec = Journal::record(&path).unwrap();
        rec.record_pad(100, true, Pad { buttons: 1, ..Default::default() });
        rec.record_pad(150, false, Pad { buttons: 1, ..Default::default() });
        rec.record_pad(150, false, Pad { buttons: 1, ..Default::default() }); // unchanged: no line
        rec.record_load(200, 1);
        rec.record_pad(50, true, Pad { buttons: 2, ..Default::default() });
        drop(rec);
        let mut rep = Journal::replay(&path).unwrap();
        // the last change at or before the clock wins; the load at 200 is not passed
        assert_eq!(rep.replay_pad(400), Some((false, Pad { buttons: 1, ..Default::default() })));
        assert_eq!(rep.replay_pad(400), None);
        assert_eq!(rep.next_pad_clock(), Some(200));
        assert_eq!(rep.replay_ui(199), None);
        assert_eq!(rep.replay_ui(200), Some(Ui::Load(1)));
        assert_eq!(rep.replay_pad(50), Some((true, Pad { buttons: 2, ..Default::default() })));
        assert!(rep.finished() && rep.diverged == 0);
        let _ = std::fs::remove_file(path);
    }

    /// A journal names the build that wrote it; a replay on another build warns, and so does a
    /// journal from before build ids. The end mark is only written while recording.
    #[test]
    fn the_build_line_and_the_end_mark() {
        let path = journal_path("build");
        let mut rec = Journal::record(&path).unwrap();
        rec.time(10, Q);
        rec.record_end(20);
        drop(rec);
        let mut rep = Journal::replay(&path).unwrap();
        assert_eq!(rep.recorded_build.as_deref(), Some(crate::BUILD));
        assert_eq!(rep.build_warning(), None);
        rep.record_end(30); // a replay never writes
        assert_eq!(rep.end_clock(), Some(20));
        std::fs::write(&path, "# lma real-time journal v1\n# build deadbeef\nt 10 0 1\n").unwrap();
        let rep = Journal::replay(&path).unwrap();
        assert!(rep.build_warning().unwrap().contains("deadbeef"));
        std::fs::write(&path, "t 10 0 1\n").unwrap();
        let rep = Journal::replay(&path).unwrap();
        assert!(rep.recorded_build.is_none() && rep.build_warning().unwrap().contains("predates"));
        let _ = std::fs::remove_file(path);
    }

    /// A line the parser doesn't know, or one with a missing field, is an InvalidData error
    /// naming the line; comments and blank lines are skipped.
    #[test]
    fn bad_lines_are_errors_with_the_line_number() {
        let path = journal_path("bad");
        std::fs::write(&path, "# comment\n\nt 10 0 1\nx 5\n").unwrap();
        let err = Journal::replay(&path).err().expect("an unknown line is an error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains(":4:"), "{err}");
        std::fs::write(&path, "t 10 0\n").unwrap();
        let err = Journal::replay(&path).err().expect("a missing field is an error");
        assert!(err.to_string().contains(":1:"), "{err}");
        std::fs::write(&path, "p 10 1 zz 0 0 0 0 0 0\n").unwrap();
        assert!(Journal::replay(&path).is_err(), "a non-hex button word");
        std::fs::write(&path, "# only comments\n").unwrap();
        let rep = Journal::replay(&path).unwrap();
        assert!(rep.finished() && rep.end_clock().is_none() && rep.played_out(0));
        let _ = std::fs::remove_file(path);
    }

    /// The frame-skip line is written only when render_every changes, and replays at the same
    /// (clock, seq) as recorded; a skip the replay passes without asking for is a divergence.
    #[test]
    fn skip_changes_replay_in_place_or_diverge() {
        let path = journal_path("skip");
        let mut rec = Journal::record(&path).unwrap();
        rec.record_skip(100, 1); // the initial value: no line
        rec.record_skip(100, 2);
        rec.record_skip(100, 2);
        rec.time(200, 3 * Q);
        rec.record_skip(200, 1);
        drop(rec);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().filter(|l| l.starts_with("s ")).count(), 2, "{text}");
        let mut rep = Journal::replay(&path).unwrap();
        assert_eq!(rep.replay_skip(100), Some(2));
        assert_eq!(rep.replay_skip(100), None);
        assert_eq!(rep.time(200, 0), 3 * Q);
        assert_eq!(rep.replay_skip(200), Some(1));
        assert!(rep.finished() && rep.diverged == 0);
        let mut rep = Journal::replay(&path).unwrap();
        assert_eq!(rep.time(200, 0), 3 * Q, "the skip at 100 was never asked for");
        assert_eq!(rep.diverged, 1);
        let _ = std::fs::remove_file(path);
    }
}
