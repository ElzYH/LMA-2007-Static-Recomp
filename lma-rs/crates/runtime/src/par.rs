//! Guest threads on host threads (#39, `LMA_THREADS=N`, real time only): Ready guest threads
//! other than the machine's current one run their recompiled code on host workers, each with the
//! thread's own `Cpu` and a shared view of guest memory, until their budget runs out or the next
//! pc needs the machine (a kernel import, a hook, code that isn't recompiled, a stop). Kernel
//! calls, scheduling, the GPU and audio stay on the machine's thread, which takes the `Cpu` back
//! and runs the thread itself from there. The hand-offs through the channels order guest memory
//! between them; within a run, guest threads share memory as on the Xbox (atomics through
//! `Memory::cas32`/`cas64`).

use crate::cpu::{Cpu, Stop};
use crate::mem::Memory;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};

pub(crate) struct Job {
    pub t: usize,
    pub cpu: Cpu,
    pub budget: i64,
    pub rt_base: u64,
}

pub(crate) struct Done {
    pub t: usize,
    pub cpu: Cpu,
    pub used: u64,
    pub stop: Option<Stop>,
    worker: usize,
}

pub(crate) struct Pool {
    jobs: Vec<Sender<Job>>,
    free: Vec<usize>,
    done: Receiver<Done>,
    running: Vec<bool>,
    pub in_flight: usize,
}

impl Pool {
    pub fn new(workers: usize, mem: &Memory) -> Self {
        let (dtx, drx) = channel();
        let jobs = (0..workers)
            .map(|w| {
                let (tx, rx) = channel::<Job>();
                let mut m = mem.share();
                let d = dtx.clone();
                std::thread::Builder::new().name(format!("lma guest worker {w}")).spawn(move || work(w, &mut m, rx, d)).expect("guest worker thread");
                tx
            })
            .collect();
        Pool { jobs, free: (0..workers).collect(), done: drx, running: Vec::new(), in_flight: 0 }
    }

    pub fn has_free(&self) -> bool {
        !self.free.is_empty()
    }

    pub fn is_running(&self, t: usize) -> bool {
        self.running.get(t).copied().unwrap_or(false)
    }

    pub fn send(&mut self, job: Job) {
        let w = self.free.pop().expect("a free worker");
        if self.running.len() <= job.t {
            self.running.resize(job.t + 1, false);
        }
        self.running[job.t] = true;
        self.in_flight += 1;
        self.jobs[w].send(job).expect("guest worker alive");
    }

    /// A finished run, waiting up to `wait` for one (None: don't wait).
    pub fn recv(&mut self, wait: Option<std::time::Duration>) -> Option<Done> {
        let d = match wait {
            None => self.done.try_recv().ok()?,
            Some(t) => match self.done.recv_timeout(t) {
                Ok(d) => d,
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => panic!("guest workers gone"),
            },
        };
        self.free.push(d.worker);
        self.running[d.t] = false;
        self.in_flight -= 1;
        Some(d)
    }
}

/// A worker: runs each job's thread through recompiled code until its budget is spent or the
/// next pc isn't a recompiled block, then hands the `Cpu` back.
fn work(worker: usize, mem: &mut Memory, rx: Receiver<Job>, done: Sender<Done>) {
    while let Ok(Job { t, mut cpu, budget, rt_base }) = rx.recv() {
        // real-time pacing as `Machine::run_slice`: the timebase is the slice's base plus the
        // instructions consumed >> RT_SHIFT
        cpu.rt = true;
        cpu.rt_base = rt_base;
        cpu.slice_len = budget;
        cpu.slice_end = budget as u64;
        let (mut used, mut stop) = (0i64, None);
        while used < budget {
            let pc = cpu.pc;
            let Some(f) = crate::rec::block_at(pc) else { break };
            cpu.timebase = rt_base + ((used as u64) >> crate::rec::RT_SHIFT);
            let start = budget - used;
            let mut left = start;
            let r = f(&mut cpu, mem, pc, &mut left);
            used += (start - left).max(0);
            match r {
                Ok(next) => cpu.pc = next,
                Err(s) => {
                    stop = Some(s);
                    break;
                }
            }
        }
        if done.send(Done { t, cpu, used: used as u64, stop, worker }).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A job whose pc starts no recompiled block comes straight back, untouched, and the pool
    /// tracks the thread as running only until then.
    #[test]
    fn a_job_off_recompiled_code_comes_straight_back() {
        let mem = Memory::new();
        let mut pool = Pool::new(2, &mem);
        let mut cpu = Cpu::default();
        cpu.pc = 0x10; // below the recompiled range: never a block
        cpu.gpr[5] = 42;
        pool.send(Job { t: 3, cpu, budget: 1000, rt_base: 0 });
        assert!(pool.is_running(3) && pool.in_flight == 1);
        let d = pool.recv(Some(std::time::Duration::from_secs(5))).expect("the job comes back");
        assert_eq!((d.t, d.used, d.cpu.pc, d.cpu.gpr[5]), (3, 0, 0x10, 42));
        assert!(d.stop.is_none() && !pool.is_running(3) && pool.in_flight == 0 && pool.has_free());
    }
}
