//! What recompiled game code needs from the runtime: the guest CPU state (with the reference
//! interpreter), big-endian guest memory and the `rec` helpers. Kept in its own crate so the
//! 32 generated `game-blocks` parts depend on this alone; changes elsewhere in `runtime`
//! (machine, GPU, HLE, VFS) don't rebuild them. `runtime` re-exports these modules.

pub mod cpu;
pub mod hooks;
pub mod mem;
pub mod mmio;
pub mod rec;
pub mod vmx;
