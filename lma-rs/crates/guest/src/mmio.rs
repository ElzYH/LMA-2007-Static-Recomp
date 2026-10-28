//! Device registers where every write is a command, not a value to keep (#199).
//!
//! The XMA audio hardware's kick / lock / clear registers (0x7FEA1940.., 0x7FEA1A40..,
//! 0x7FEA1A80.., one bit per context) act on each store. The game's sound engine writes them with
//! `stwbrx` in loops, one context after another, so polling the memory afterwards would only see
//! the last one. While the runtime has the trap on (LMA_AUDIO=1), each such store is also queued
//! here in order; with it off nothing is recorded, so runs stay bit-exact either way (the queue
//! never feeds back into guest state by itself).

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;

/// Little-endian command registers (kick through clear) whose stores are queued.
pub const XMA_COMMANDS: std::ops::Range<u32> = 0x7FEA_1940..0x7FEA_1AB0;

static ON: AtomicBool = AtomicBool::new(false);
static QUEUE: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

/// Starts or stops queuing XMA register commands.
pub fn set_xma_trap(on: bool) {
    ON.store(on, Relaxed);
}

/// A byte-reversed store of `value` (the register's little-endian value) to `ea`.
#[inline]
pub fn store_le(ea: u32, value: u32) {
    if XMA_COMMANDS.contains(&ea) && ON.load(Relaxed) {
        QUEUE.lock().unwrap().push((ea, value));
    }
}

/// The commands written since the last call, oldest first.
pub fn take_xma_commands() -> Vec<(u32, u32)> {
    std::mem::take(&mut *QUEUE.lock().unwrap())
}
