//! The black box (the maintainer, 29 Oct: "a system for catching stuff going wrong ... if it occurs again
//! it's in a log or can be reported"). While a game runs, every few minutes (`LMA_BLACKBOX=<min>`,
//! default 3, 0 = off) it starts a segment in `reports/.blackbox/`: a state saved in the
//! background (the game thread only copies memory) and the pad's presses from there. The last two
//! segments are kept, so a bug report (F12 / Ctrl+B) carries the minutes before it: load
//! `before.state` and replay `before.input` to see the bug happen again. The report also gets the
//! event log (`runtime::events`: every sound started, repeats flagged). A sound started over and
//! over writes a report by itself, once per session.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DIR: &str = "reports/.blackbox";

struct Segment {
    state: PathBuf,
    input: PathBuf,
    /// The state's background write, until it is joined.
    writing: Option<std::thread::JoinHandle<std::io::Result<u64>>>,
}

pub struct BlackBox {
    every: Duration,
    last: Option<Instant>,
    n: u64,
    segments: VecDeque<Segment>,
    recorder: Option<crate::script::Recorder>,
}

/// `LMA_BLACKBOX` in minutes: unset = 3, 0 (or anything not a number) = off.
pub fn minutes(var: Option<&str>) -> Option<u64> {
    match var {
        None => Some(3),
        Some(v) => v.trim().parse().ok().filter(|&m| m > 0),
    }
}

impl BlackBox {
    /// None when switched off.
    pub fn from_env() -> Option<Self> {
        let every = minutes(std::env::var("LMA_BLACKBOX").ok().as_deref())?;
        // segments of an earlier session are useless (another build or state): start clean
        let _ = std::fs::remove_dir_all(DIR);
        Some(BlackBox { every: Duration::from_secs(every * 60), last: None, n: 0, segments: VecDeque::new(), recorder: None })
    }

    /// Called every loop turn with the pad: records presses, and starts a segment when one is due
    /// and the previous state is written.
    pub fn tick(&mut self, m: &mut runtime::machine::Machine, buttons: u16) {
        if let Some(r) = self.recorder.as_mut() {
            r.update(m.now(), buttons);
        }
        if self.last.is_some_and(|t| t.elapsed() < self.every) || self.segments.back().is_some_and(|s| s.writing.as_ref().is_some_and(|w| !w.is_finished())) {
            return;
        }
        self.join_all();
        self.n += 1;
        let state = PathBuf::from(DIR).join(format!("{}.state", self.n));
        let input = PathBuf::from(DIR).join(format!("{}.input", self.n));
        if let Err(e) = std::fs::create_dir_all(DIR) {
            eprintln!("black box: {DIR}: {e}; off");
            self.last = Some(Instant::now() + Duration::from_secs(1 << 30));
            return;
        }
        let writing = Some(m.save_state_background(state.clone()));
        // the presses so far go with the previous segment: close it at this moment
        if let Some(r) = self.recorder.as_mut() {
            r.update(m.now(), 0);
        }
        self.recorder = crate::script::Recorder::create(&input.to_string_lossy(), &state.to_string_lossy(), m.deterministic).map_err(|e| eprintln!("black box: {}: {e}", input.display())).ok();
        self.segments.push_back(Segment { state, input, writing });
        while self.segments.len() > 2 {
            if let Some(old) = self.segments.pop_front() {
                drop(old.writing.map(|w| w.join()));
                let _ = std::fs::remove_file(old.state);
                let _ = std::fs::remove_file(old.input);
            }
        }
        self.last = Some(Instant::now());
    }

    fn join_all(&mut self) {
        for s in &mut self.segments {
            if let Some(w) = s.writing.take() {
                match w.join() {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => eprintln!("black box: {}: {e}", s.state.display()),
                    Err(_) => eprintln!("black box: {}: the writer panicked", s.state.display()),
                }
            }
        }
    }

    /// Into a bug report's folder: the segments (oldest first: `before.state`/`before.input`, then
    /// `before2.*`), linked or copied, and how to replay them.
    pub fn add_to_report(&mut self, dir: &Path) -> String {
        self.join_all();
        let mut lines = String::new();
        for (k, s) in self.segments.iter().enumerate() {
            let name = if k == 0 { "before".to_string() } else { format!("before{}", k + 1) };
            let (st, inp) = (dir.join(format!("{name}.state")), dir.join(format!("{name}.input")));
            if !s.state.exists() {
                continue;
            }
            let linked = [(&s.state, &st), (&s.input, &inp)].into_iter().all(|(from, to)| std::fs::hard_link(from, to).or_else(|_| std::fs::copy(from, to).map(|_| ())).is_ok());
            if linked {
                lines += &format!("{name}: LMA_LOAD={name}.state LMA_INPUT=@{name}.input (the game and your presses from there)\n");
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_links_the_segments_oldest_first_and_skips_unwritten_ones() {
        let d = std::env::temp_dir().join(format!("lma-blackbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("bb")).unwrap();
        std::fs::create_dir_all(d.join("report")).unwrap();
        let seg = |n: u32, written: bool| {
            let (state, input) = (d.join(format!("bb/{n}.state")), d.join(format!("bb/{n}.input")));
            if written {
                std::fs::write(&state, format!("state {n}")).unwrap();
            }
            std::fs::write(&input, format!("input {n}")).unwrap();
            Segment { state, input, writing: None }
        };
        let mut b = BlackBox { every: Duration::from_secs(60), last: None, n: 3, segments: [seg(1, true), seg(2, true), seg(3, false)].into(), recorder: None };
        let lines = b.add_to_report(&d.join("report"));
        assert_eq!(lines.lines().count(), 2, "{lines}");
        assert!(lines.starts_with("before: LMA_LOAD=before.state LMA_INPUT=@before.input"));
        assert_eq!(std::fs::read_to_string(d.join("report/before.state")).unwrap(), "state 1");
        assert_eq!(std::fs::read_to_string(d.join("report/before2.input")).unwrap(), "input 2");
        assert!(!d.join("report/before3.state").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn minutes_default_three_zero_off() {
        assert_eq!(minutes(None), Some(3));
        assert_eq!(minutes(Some("5")), Some(5));
        assert_eq!(minutes(Some("0")), None);
        assert_eq!(minutes(Some("off")), None);
    }
}
