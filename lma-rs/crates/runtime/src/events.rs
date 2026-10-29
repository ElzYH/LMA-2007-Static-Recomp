//! The black box's event log (the maintainer, 29 Oct: "a system for catching stuff going wrong"): the last
//! few thousand notable things the game did (sounds started, stalls), with the guest clock, kept
//! in memory and written into a bug report. A sound started over and over (the "long ball" shout
//! looping after a subs screen, 29 Oct) is flagged as it happens, so the app can save a report by
//! itself.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Events kept: the oldest go first.
pub const CAPACITY: usize = 4000;
/// A repeat: the same sound started this many times ...
pub const REPEATS: usize = 6;
/// ... within this many seconds of game time.
pub const REPEAT_WINDOW: f64 = 10.0;

/// One event: game time in seconds and what happened.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub secs: f64,
    pub text: String,
}

#[derive(Default)]
pub struct Log {
    events: VecDeque<Event>,
    /// Start times of each sound key's recent plays, oldest first.
    recent: std::collections::HashMap<String, VecDeque<f64>>,
    /// Repeats found and not yet taken by `take_repeat`.
    repeats: Vec<String>,
    /// Keys already reported as repeating: each is reported once a session.
    reported: std::collections::HashSet<String>,
}

impl Log {
    pub fn note(&mut self, secs: f64, text: String) {
        if self.events.len() == CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(Event { secs, text });
    }

    /// A sound `key` started at `secs`: noted, and flagged when it is the REPEATS-th start within
    /// REPEAT_WINDOW seconds. Game time can jump back (a state loaded): older starts are dropped.
    pub fn sound(&mut self, secs: f64, key: &str) {
        self.note(secs, format!("sound {key}"));
        let starts = self.recent.entry(key.to_string()).or_default();
        starts.retain(|&t| t <= secs && secs - t <= REPEAT_WINDOW);
        starts.push_back(secs);
        if starts.len() >= REPEATS && self.reported.insert(key.to_string()) {
            let msg = format!("sound {key} started {} times in {REPEAT_WINDOW:.0} s", starts.len());
            self.note(secs, format!("REPEAT {msg}"));
            self.repeats.push(msg);
        }
    }

    /// The first repeat found since the last call, if any.
    pub fn take_repeat(&mut self) -> Option<String> {
        (!self.repeats.is_empty()).then(|| self.repeats.remove(0))
    }

    /// The events as text, one per line: `<m:ss.s> <text>`.
    pub fn dump(&self) -> String {
        self.events.iter().map(|e| format!("{}:{:04.1} {}\n", (e.secs / 60.0) as u64, e.secs % 60.0, e.text)).collect()
    }
}

static LOG: Mutex<Option<Log>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Log) -> R) -> R {
    let mut g = LOG.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Log::default))
}

/// Seconds of game time from the guest timebase.
pub fn secs(timebase: u64) -> f64 {
    timebase as f64 / crate::threads::CLOCK_HZ as f64
}

pub fn note(secs: f64, text: String) {
    with(|l| l.note(secs, text));
}

pub fn sound(secs: f64, key: &str) {
    with(|l| l.sound(secs, key));
}

pub fn take_repeat() -> Option<String> {
    with(Log::take_repeat)
}

pub fn dump() -> String {
    with(|l| l.dump())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sound_started_six_times_in_ten_seconds_is_a_repeat_once() {
        let mut l = Log::default();
        for k in 0..5 {
            l.sound(100.0 + 1.3 * k as f64, "English_1/ENGLISH1_LONGBALL_024");
        }
        assert_eq!(l.take_repeat(), None);
        l.sound(106.5, "English_1/ENGLISH1_LONGBALL_024");
        assert_eq!(l.take_repeat().as_deref(), Some("sound English_1/ENGLISH1_LONGBALL_024 started 6 times in 10 s"));
        // reported once a session
        l.sound(107.8, "English_1/ENGLISH1_LONGBALL_024");
        assert_eq!(l.take_repeat(), None);
    }

    #[test]
    fn spread_out_or_mixed_sounds_are_no_repeat() {
        let mut l = Log::default();
        for k in 0..20 {
            l.sound(3.0 * k as f64, "MATCH/whistle_short");
            l.sound(3.0 * k as f64 + 0.5, if k % 2 == 0 { "MATCH/kickmed01" } else { "MATCH/kickmed02" });
        }
        assert_eq!(l.take_repeat(), None);
    }

    #[test]
    fn a_state_loaded_back_in_time_forgets_the_later_starts() {
        let mut l = Log::default();
        for k in 0..5 {
            l.sound(500.0 + k as f64, "a/b");
        }
        l.sound(100.0, "a/b");
        assert_eq!(l.take_repeat(), None);
    }

    #[test]
    fn times_are_minutes_and_seconds_past_an_hour() {
        let mut l = Log::default();
        l.note(5.5, "a".into());
        l.note(3725.5, "b".into());
        assert_eq!(l.dump(), "0:05.5 a\n62:05.5 b\n");
    }

    #[test]
    fn the_log_keeps_the_newest_events() {
        let mut l = Log::default();
        for k in 0..CAPACITY + 10 {
            l.note(k as f64, format!("e{k}"));
        }
        let d = l.dump();
        assert_eq!(d.lines().count(), CAPACITY);
        assert!(d.starts_with("0:10.0 e10\n"), "{}", &d[..20]);
        assert!(d.ends_with(&format!("e{}\n", CAPACITY + 9)));
    }
}
