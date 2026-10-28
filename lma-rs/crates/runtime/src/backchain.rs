//! The guest's stack back chain: who called the function a thread is in (#816, #862).
//!
//! The 360's ABI keeps a back chain: a function's frame stores the caller's stack pointer at
//! `*(sp)`, and the savegpr prologue stores the function's own return address (its incoming LR) at
//! `caller_sp - 8`. So from the stack pointer of a stopped thread, `*(sp)` is the caller's frame and
//! `*(*(sp) - 8)` the call site inside the caller; one step up per frame.
use guest::mem::Memory;

/// Return addresses up the stack from `sp`, nearest call site first, at most `max` of them. Stops
/// at a null link or a null return address (memory nothing wrote reads as zero on the flat
/// mapping; `Memory::is_mapped` is not used: it asks the OS for every resident page per call).
/// A leaf that has not pushed a frame gives the next frame's entries; `lr` is its own return then.
pub fn back_chain(mem: &mut Memory, sp: u32, max: usize) -> Vec<u32> {
    let mut out = Vec::new();
    let mut s = sp;
    while out.len() < max {
        s = mem.read32(s);
        if s == 0 {
            break;
        }
        let ret = mem.read32(s.wrapping_sub(8));
        if ret == 0 {
            break;
        }
        out.push(ret);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chain_lists_the_call_sites_nearest_first() {
        let mut m = Memory::new();
        // three frames: helper (sp 0x7000_0f00) <- caller A (0x7000_0fb0) <- caller B (0x7000_1000) <- 0
        m.write32(0x7000_0f00, 0x7000_0fb0);
        m.write32(0x7000_0fb0 - 8, 0x8233_7628); // the helper's return address: A's call site
        m.write32(0x7000_0fb0, 0x7000_1000);
        m.write32(0x7000_1000 - 8, 0x8238_9a00); // A's return address: B's call site
        m.write32(0x7000_1000, 0);
        assert_eq!(back_chain(&mut m, 0x7000_0f00, 6), vec![0x8233_7628, 0x8238_9a00]);
        assert_eq!(back_chain(&mut m, 0x7000_0f00, 1), vec![0x8233_7628], "max caps the walk");
    }

    #[test]
    fn a_null_link_or_return_ends_the_walk() {
        let mut m = Memory::new();
        m.write32(0x7000_0f00, 0x6000_0000); // a link into memory nothing wrote: its return reads 0
        assert_eq!(back_chain(&mut m, 0x7000_0f00, 6), Vec::<u32>::new());
        assert_eq!(back_chain(&mut m, 0x6100_0000, 6), Vec::<u32>::new(), "an sp nothing wrote");
        m.write32(0x7000_0f00, 0x7000_0fb0);
        m.write32(0x7000_0fb0 - 8, 0x8233_7628);
        m.write32(0x7000_0fb0, 0); // the first frame's caller has no link
        assert_eq!(back_chain(&mut m, 0x7000_0f00, 6), vec![0x8233_7628]);
    }
}
