//! Guest threads, dispatcher objects (events, semaphores, threads) and critical sections.
//!
//! Every waitable object is a real X_DISPATCHER_HEADER in guest memory
//! (type u8 @0, signal_state i32 @4), so Ke* (pointer) and Nt* (handle) waits share one path.
//! Time is virtual: CLOCK_HZ guest instructions per second, fast-forwarded when all
//! threads sleep, which keeps runs deterministic.

use crate::cpu::Cpu;
use crate::machine::Machine;

pub const CLOCK_HZ: u64 = 50_000_000;
pub const SLICE: u64 = 20_000;
pub const THREAD_EXIT_MAGIC: u32 = 0xBCBC_BCC0;

pub const STATUS_TIMEOUT: u32 = 0x0000_0102;
pub const STATUS_ABANDONED: u32 = 0x0000_0080;

// dispatcher object types
pub const OBJ_NOTIFICATION_EVENT: u8 = 0;
pub const OBJ_SYNC_EVENT: u8 = 1;
pub const OBJ_SEMAPHORE: u8 = 5;
pub const OBJ_THREAD: u8 = 6;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Wait {
    /// Guest dispatcher objects; `all` = wait-all, else wait-any. Deadline in clock ticks.
    Objects { ptrs: Vec<u32>, all: bool, deadline: Option<u64> },
    Sleep(u64),
    CritSec(u32),
    /// KSPIN_LOCK at this address (appended last so older save states still load).
    SpinLock(u32),
    /// A sleep a hook imposed in the middle of guest code (the fence nap, #312): the thread resumes
    /// at the hooked function with its registers as they were, so waking writes no status into r3.
    Nap(u64),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum State {
    Ready,
    Suspended(u32),
    Waiting(Wait),
    Done,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct GThread {
    pub cpu: Cpu,
    pub state: State,
    pub kthread: u32,
    pub tls: Vec<u32>,
    pub name: String,
}

/// Result of a kernel call as seen by the scheduler.
pub enum Flow {
    Continue,
    Block(Wait),
    Yield,
    ExitThread,
}

impl Machine {
    pub(crate) fn signal_state(&mut self, obj: u32) -> i32 {
        self.mem.read32(obj + 4) as i32
    }

    pub(crate) fn set_signal(&mut self, obj: u32, v: i32) {
        self.mem.write32(obj + 4, v as u32);
    }

    /// Allocate and initialise a dispatcher object in guest memory.
    pub(crate) fn new_object(&mut self, kind: u8, signal: i32) -> u32 {
        let p = self.alloc_small(0x40);
        self.mem.write8(p, kind);
        self.mem.write8(p + 2, 4);
        self.set_signal(p, signal);
        p
    }

    pub(crate) fn new_handle(&mut self, obj: u32) -> u32 {
        self.next_handle += 4;
        self.handles.insert(self.next_handle, obj);
        self.next_handle
    }

    pub(crate) fn object_of(&self, handle: u32) -> Option<u32> {
        // pseudo handle for the current thread
        if handle == 0xFFFF_FFFE {
            return Some(self.threads[self.cur].kthread);
        }
        self.handles.get(&handle).copied()
    }

    fn is_signaled(&mut self, obj: u32) -> bool {
        self.signal_state(obj) > 0
    }

    /// Consume a signal after a successful wait (auto-reset events, semaphores).
    fn acquire(&mut self, obj: u32) {
        match self.mem.read8(obj) {
            OBJ_SYNC_EVENT => self.set_signal(obj, 0),
            OBJ_SEMAPHORE => {
                let s = self.signal_state(obj);
                self.set_signal(obj, s - 1);
            }
            _ => {}
        }
    }

    /// Try to satisfy a wait right now. Returns the status to hand back, or None to block.
    pub(crate) fn try_wait(&mut self, w: &Wait) -> Option<u32> {
        match w {
            Wait::Objects { ptrs, all, deadline } => {
                if *all {
                    if ptrs.iter().all(|&p| self.is_signaled(p)) {
                        for &p in ptrs {
                            self.acquire(p);
                        }
                        return Some(0);
                    }
                } else if let Some(i) = ptrs.iter().position(|&p| self.is_signaled(p)) {
                    self.acquire(ptrs[i]);
                    return Some(i as u32);
                }
                match deadline {
                    Some(d) if self.now() >= *d => Some(STATUS_TIMEOUT),
                    _ => None,
                }
            }
            Wait::Sleep(d) | Wait::Nap(d) => (self.now() >= *d).then_some(0),
            Wait::CritSec(cs) => {
                let me = self.threads[self.cur_or_waiter(*cs)].kthread;
                self.cs_try_enter(*cs, me).then_some(0)
            }
            Wait::SpinLock(lock) => self.spin_try_acquire(*lock, self.threads[self.cur].kthread).then_some(0),
        }
    }

    fn cur_or_waiter(&self, cs: u32) -> usize {
        // the waiter is whichever thread is blocked on this critical section
        self.threads
            .iter()
            .position(|t| t.state == State::Waiting(Wait::CritSec(cs)))
            .unwrap_or(self.cur)
    }

    /// Timeout pointer -> absolute deadline in clock ticks (None = infinite).
    pub(crate) fn deadline(&mut self, timeout_ptr: u32) -> Option<u64> {
        if timeout_ptr == 0 {
            return None;
        }
        let t = self.mem.read64(timeout_ptr) as i64;
        let ticks_per_100ns = CLOCK_HZ as f64 / 10_000_000.0;
        let rel = if t < 0 { (-t) as f64 } else { (t as f64 - self.system_time_100ns() as f64).max(0.0) };
        Some(self.now() + (rel * ticks_per_100ns) as u64)
    }

    /// Guest system time in 100 ns units (FILETIME), starting 1 Jan 2007.
    pub(crate) fn system_time_100ns(&self) -> u64 {
        0x01C7_2D53_0000_0000 + self.now() * 10_000_000 / CLOCK_HZ
    }

    // ---- critical sections: { header[16], lock_count i32, recursion i32, owner u32 } ----
    pub(crate) fn cs_try_enter(&mut self, cs: u32, me: u32) -> bool {
        let owner = self.mem.read32(cs + 24);
        if owner == 0 || owner == me {
            self.mem.write32(cs + 24, me);
            let rec = self.mem.read32(cs + 20);
            self.mem.write32(cs + 20, rec + 1);
            let lc = self.mem.read32(cs + 16) as i32;
            self.mem.write32(cs + 16, (lc + 1) as u32);
            true
        } else {
            false
        }
    }

    /// Spin locks really exclude: the guest relies on them across hardware threads, and our
    /// scheduler can switch threads inside one (the lock word holds the owner's KTHREAD).
    pub(crate) fn spin_try_acquire(&mut self, lock: u32, me: u32) -> bool {
        let owner = self.mem.read32(lock);
        if owner == 0 || owner == me {
            self.mem.write32(lock, me);
            true
        } else {
            false
        }
    }

    pub(crate) fn cs_leave(&mut self, cs: u32) {
        let rec = self.mem.read32(cs + 20).saturating_sub(1);
        self.mem.write32(cs + 20, rec);
        let lc = self.mem.read32(cs + 16) as i32;
        self.mem.write32(cs + 16, (lc - 1) as u32);
        if rec == 0 {
            self.mem.write32(cs + 24, 0);
        }
    }

    // ---- thread creation / scheduling -------------------------------------------------
    pub(crate) fn spawn(&mut self, pc: u32, r3: u32, r4: u32, suspended: bool, name: String) -> usize {
        // Reuse the slot (index, stack, PCR, KTHREAD) of a finished thread nobody holds a handle to:
        // the game starts worker threads every day, and with one slot per thread forever, thread
        // 270-odd's stack (0x7100_0000 + idx MB) reached the executable at 0x8200_0000 and
        // overwrote code (a stop on 21 Aug of the maintainer's career).
        let reuse = (1..self.threads.len()).find(|&i| {
            let k = self.threads[i].kthread;
            self.threads[i].state == State::Done && Some(i) != self.interrupt_thread && !self.handles.values().any(|&o| o == k)
        });
        let idx = reuse.unwrap_or(self.threads.len());
        // stacks below the PCR area (0x7F00_0000) only; past that (224 live threads) the heap
        let stack_top = if 0x7100_0000 + (idx as u64 + 1) * 0x0010_0000 <= 0x7F00_0000 {
            0x7100_0000 + (idx as u32) * 0x0010_0000
        } else {
            Self::alloc(&mut self.heap, 0x0010_0000, 0x0001_0000) + 0x0010_0000
        };
        let pcr = 0x7F00_0000 + (idx as u32) * 0x2000;
        let kthread = pcr + 0x1000;
        self.mem.write(stack_top - 0x10_0000, &[0; 16]);
        self.mem.write(pcr, &[0; 0x200]);
        let tls = self.alloc_small(self.tls_size.max(0x100));
        if !self.tls_init.is_empty() {
            let init = self.tls_init.clone();
            self.mem.write(tls, &init);
        }
        self.mem.write32(pcr, tls); // compiler TLS block
        self.mem.write32(pcr + 0x70, stack_top);
        self.mem.write32(pcr + 0x74, stack_top - 0x10_0000);
        self.mem.write32(pcr + 0x100, kthread);
        self.mem.write(kthread, &[0; 0x200]);
        self.mem.write8(kthread, OBJ_THREAD);
        // ThreadId: unique even when the slot is reused (D3D compares them for device ownership)
        self.next_tid = self.next_tid.max(self.threads.len() as u32 + 1);
        self.mem.write32(kthread + 0x14C, self.next_tid);
        self.next_tid += 1;

        let mut cpu = Cpu::default();
        cpu.pc = pc;
        cpu.gpr[1] = (stack_top - 0x100) as u64;
        cpu.gpr[3] = r3 as u64;
        cpu.gpr[4] = r4 as u64;
        cpu.gpr[13] = pcr as u64;
        cpu.lr = if idx == 0 { crate::machine::RETURN_MAGIC } else { THREAD_EXIT_MAGIC } as u64;
        let state = if suspended { State::Suspended(1) } else { State::Ready };
        let thread = GThread { cpu, state, kthread, tls: Vec::new(), name };
        if reuse.is_some() {
            self.threads[idx] = thread;
        } else {
            self.threads.push(thread);
        }
        idx
    }

    pub(crate) fn thread_of_kthread(&self, kthread: u32) -> Option<usize> {
        self.threads.iter().position(|t| t.kthread == kthread)
    }

    pub(crate) fn finish_thread(&mut self, idx: usize, code: u32) {
        self.threads[idx].state = State::Done;
        let k = self.threads[idx].kthread;
        self.set_signal(k, 1);
        self.mem.write32(k + 0x150, code);
    }

    /// Wake waiters whose condition now holds; returns true if any thread is runnable.
    pub(crate) fn wake(&mut self) -> bool {
        let mut any = false;
        for i in 0..self.threads.len() {
            if let State::Waiting(w) = self.threads[i].state.clone() {
                let prev = self.cur;
                self.cur = i;
                if let Some(status) = self.try_wait(&w) {
                    if !matches!(w, Wait::Nap(_)) {
                        self.threads[i].cpu.gpr[3] = status as u64;
                    }
                    self.threads[i].state = State::Ready;
                }
                self.cur = prev;
            }
            any |= self.threads[i].state == State::Ready && !self.par_running(i);
        }
        any
    }

    /// Earliest deadline among waiting threads (for fast-forwarding the clock).
    pub(crate) fn next_deadline(&self) -> Option<u64> {
        self.threads
            .iter()
            .filter_map(|t| match &t.state {
                State::Waiting(Wait::Sleep(d)) | State::Waiting(Wait::Nap(d)) => Some(*d),
                State::Waiting(Wait::Objects { deadline: Some(d), .. }) => Some(*d),
                _ => None,
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> Machine {
        let x = xex::Xex { module_flags: 0, entry_point: 0x8200_0000, image_base: 0x8200_0000, image: vec![0; 0x1000], libraries: vec![], used_devkit_key: false };
        let dir = std::env::temp_dir();
        let mut m = Machine::new(&x, &dir, &dir);
        m.deterministic = true; // now() is the instruction clock
        m
    }

    fn wait_any(ptrs: &[u32], deadline: Option<u64>) -> Wait {
        Wait::Objects { ptrs: ptrs.to_vec(), all: false, deadline }
    }

    /// Notification events stay signalled for every waiter, synchronisation events wake one and
    /// reset, semaphores count down; wait-any reports which object, wait-all takes all or none.
    #[test]
    fn dispatcher_objects() {
        let mut m = machine();
        let note = m.new_object(OBJ_NOTIFICATION_EVENT, 1);
        let sync = m.new_object(OBJ_SYNC_EVENT, 1);
        let sem = m.new_object(OBJ_SEMAPHORE, 2);
        let off = m.new_object(OBJ_SYNC_EVENT, 0);
        assert_eq!(m.try_wait(&wait_any(&[note], None)), Some(0));
        assert_eq!(m.try_wait(&wait_any(&[note], None)), Some(0), "a notification event stays set");
        assert_eq!(m.try_wait(&wait_any(&[sync], None)), Some(0));
        assert_eq!(m.try_wait(&wait_any(&[sync], None)), None, "a sync event resets after one wait");
        assert_eq!(m.try_wait(&wait_any(&[sem], None)), Some(0));
        assert_eq!(m.try_wait(&wait_any(&[sem], None)), Some(0));
        assert_eq!(m.try_wait(&wait_any(&[sem], None)), None, "the semaphore's count ran out");
        // wait-any: the index of the first signalled object
        assert_eq!(m.try_wait(&wait_any(&[off, note], None)), Some(1));
        // wait-all: nothing is consumed unless all are signalled
        m.set_signal(sync, 1);
        let all = |ptrs: &[u32]| Wait::Objects { ptrs: ptrs.to_vec(), all: true, deadline: None };
        assert_eq!(m.try_wait(&all(&[sync, off])), None);
        assert_eq!(m.signal_state(sync), 1, "not consumed by a failed wait-all");
        m.set_signal(off, 1);
        assert_eq!(m.try_wait(&all(&[sync, off])), Some(0));
        assert_eq!((m.signal_state(sync), m.signal_state(off)), (0, 0));
    }

    /// A wait with a deadline times out once the clock reaches it; a relative timeout (negative,
    /// in 100 ns units) counts from now, an absolute one in the past means now.
    #[test]
    fn timeouts() {
        let mut m = machine();
        let off = m.new_object(OBJ_SYNC_EVENT, 0);
        m.clock = 1_000_000;
        let t = m.alloc_small(8);
        m.mem.write64(t, (-1_000_000i64) as u64); // 100 ms
        let d = m.deadline(t).unwrap();
        assert_eq!(d, 1_000_000 + CLOCK_HZ / 10);
        assert_eq!(m.try_wait(&wait_any(&[off], Some(d))), None);
        m.clock = d;
        assert_eq!(m.try_wait(&wait_any(&[off], Some(d))), Some(STATUS_TIMEOUT));
        m.mem.write64(t, 1); // absolute, long past
        assert_eq!(m.deadline(t), Some(m.now()));
        assert_eq!(m.deadline(0), None, "no timeout: wait forever");
        assert_eq!(m.try_wait(&Wait::Sleep(d)), Some(0));
        assert_eq!(m.try_wait(&Wait::Sleep(d + 1)), None);
    }

    /// A hook's nap (`Wait::Nap`) wakes like a sleep but leaves r3 alone: the thread resumes at the
    /// hooked function with its argument, where a kernel call's wake writes its status there.
    #[test]
    fn a_nap_keeps_r3_a_sleep_returns_a_status() {
        let mut m = machine();
        m.clock = 1_000;
        let a = m.spawn(0x8200_0100, 0x40, 0, false, "nap".into());
        let b = m.spawn(0x8200_0200, 0x41, 0, false, "sleep".into());
        m.threads[a].cpu.gpr[3] = 0x1234;
        m.threads[a].state = State::Waiting(Wait::Nap(2_000));
        m.threads[b].cpu.gpr[3] = 0x1234;
        m.threads[b].state = State::Waiting(Wait::Sleep(2_000));
        assert_eq!(m.next_deadline(), Some(2_000), "a nap is a deadline too");
        assert!(!m.wake() || m.threads[a].state != State::Ready, "not before its time");
        m.clock = 2_000;
        assert!(m.wake());
        assert_eq!(m.threads[a].state, State::Ready);
        assert_eq!(m.threads[a].cpu.gpr[3], 0x1234, "the nap kept the function's argument");
        assert_eq!(m.threads[b].state, State::Ready);
        assert_eq!(m.threads[b].cpu.gpr[3], 0, "the sleep returned its status");
    }

    /// Critical sections: the owner may enter again (recursion), another thread may not until
    /// the owner has left as often as it entered.
    #[test]
    fn critical_sections() {
        let mut m = machine();
        let cs = m.alloc_small(28);
        let (a, b) = (0x1000, 0x2000); // two KTHREADs
        assert!(m.cs_try_enter(cs, a));
        assert!(m.cs_try_enter(cs, a), "recursion");
        assert!(!m.cs_try_enter(cs, b));
        m.cs_leave(cs);
        assert!(!m.cs_try_enter(cs, b), "still held once");
        m.cs_leave(cs);
        assert_eq!(m.mem.read32(cs + 24), 0, "no owner");
        assert!(m.cs_try_enter(cs, b));
        assert_eq!((m.mem.read32(cs + 20), m.mem.read32(cs + 24)), (1, b));
    }

    /// Spin locks exclude other threads, and the owner may take it again.
    #[test]
    fn spin_locks() {
        let mut m = machine();
        let lock = m.alloc_small(4);
        assert!(m.spin_try_acquire(lock, 0x1000));
        assert!(m.spin_try_acquire(lock, 0x1000));
        assert!(!m.spin_try_acquire(lock, 0x2000));
        m.mem.write32(lock, 0); // KfReleaseSpinLock
        assert!(m.spin_try_acquire(lock, 0x2000));
    }
}
