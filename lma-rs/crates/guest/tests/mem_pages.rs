//! Guest memory's page bookkeeping, the same on every backing store (the flat mapping on Apple
//! platforms and Windows, the paged table elsewhere): which pages a save state holds, and what
//! a state load leaves behind.

use guest::mem::Memory;

const PAGE: u32 = 0x1_0000;

#[test]
fn written_pages_are_saved_and_a_load_clears_the_rest() {
    let mut m = Memory::new();
    // fresh memory reads as zero everywhere and holds no pages
    assert_eq!(m.read32(0x4000_0000), 0);
    assert!(m.snapshot_pages().is_empty());

    // three pages written, one of them through the 0xC0000000 alias of physical memory
    m.write32(0x4000_0010, 0x1111_1111);
    m.write32(0x8200_0000, 0x2222_2222);
    m.write32(0xC000_0020, 0x3333_3333); // lands in 0xA0000000's first page
    let pages: Vec<u32> = m.snapshot_pages().into_iter().map(|(i, _)| i).collect();
    assert_eq!(pages, vec![0x4000_0000 / PAGE, 0x8200_0000 / PAGE, 0xA000_0000 / PAGE]);
    assert!(m.is_mapped(0x4000_0000) && m.is_mapped(0x8200_0000) && m.is_mapped(0xA000_0000));

    // a page written and then zeroed again is not saved (only non-zero pages are)
    m.write32(0x5000_0000, 1);
    m.write32(0x5000_0000, 0);
    assert_eq!(m.snapshot_pages().len(), 3);

    // a load replaces everything: the saved page is back, every other byte reads zero
    let saved = m.snapshot_pages();
    m.write32(0x6000_0000, 0x4444_4444);
    m.restore_pages(saved[..1].to_vec());
    assert_eq!(m.read32(0x4000_0010), 0x1111_1111);
    assert_eq!(m.read32(0x8200_0000), 0);
    assert_eq!(m.read32(0xA000_0020), 0);
    assert_eq!(m.read32(0x6000_0000), 0);
    let pages: Vec<u32> = m.snapshot_pages().into_iter().map(|(i, _)| i).collect();
    assert_eq!(pages, vec![0x4000_0000 / PAGE]);

    // the last page's edge: an access ending at 0xFFFFFFFF stays inside guest memory
    m.write32(0xFFFF_FFFC, 0x5555_5555);
    assert_eq!(m.read32(0xFFFF_FFFC), 0x5555_5555);
}

/// The Windows flat map's write watch (#6): a range query returns the pages written since the
/// last query of each and resets them, while save states still list every page ever written.
#[cfg(windows)]
#[test]
fn write_watch_queries_reset_but_saves_keep_the_page() {
    let mut m = Memory::new();
    let r = m.reader();
    m.write32(0xA001_0000, 7);
    m.write32(0xA003_0010, 8);
    let first = r.write_watch(0xA000_0000, 0x4_0000).unwrap();
    assert_eq!(first, vec![0xA001_0000, 0xA003_0000], "both written pages, by translated address");
    // queried again: nothing new (the query reset them)
    assert!(r.write_watch(0xA000_0000, 0x4_0000).unwrap().is_empty());
    // a second query over an overlapping range doesn't see them either...
    assert!(r.write_watch(0xA001_0000, 0x1000).unwrap().is_empty());
    // a new write shows up once, through an alias of physical memory too
    m.write32(0xC001_0004, 9);
    assert_eq!(r.write_watch(0xE000_F000, 0x2000).unwrap(), vec![0xA001_0000]);
    // save states still hold both pages
    let pages: Vec<u32> = m.snapshot_pages().into_iter().map(|(i, _)| i).collect();
    assert_eq!(pages, vec![0xA001_0000 / PAGE, 0xA003_0000 / PAGE]);
    // (Windows creates a never-touched page dirty even when it is only read, as the snapshot's
    // 64 KB reads just did: such pages show up once in the next query, a harmless extra re-hash,
    // never a missed write)
    // a state load clears pages whose bits a query took, too
    m.restore_pages(Vec::new());
    assert_eq!(m.read32(0xA001_0000), 0);
    assert_eq!(m.read32(0xA003_0010), 0);
    assert!(m.snapshot_pages().is_empty());
}

/// A reader on another thread (the render thread) copying from guest memory while state loads
/// replace it: no read faults, and every byte it sees is one the memory held (old, new, or the
/// zeros in between), never foreign. Words may be torn mid-copy. Windows zeroes in place rather
/// than decommitting so that reads can't fault (#245).
#[test]
fn reads_during_state_loads_never_fault() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let mut m = Memory::new();
    for i in 0..64u32 {
        m.write32(0xA010_0000 + i * 0x400, 0x1111_1111);
    }
    let r = m.reader();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let reader = std::thread::spawn(move || {
        let mut seen = 0u64;
        while !s2.load(Ordering::Relaxed) {
            for i in 0..64u32 {
                let v = r.read32(0xA010_0000 + i * 0x400);
                // a word can be torn while the load copies the page in (memcpy isn't atomic per
                // word; CI on macOS caught 0x00111111), but every byte is zero, old or new
                assert!(v.to_be_bytes().iter().all(|b| matches!(b, 0 | 0x11 | 0x22)), "word {i}: {v:#x}");
                seen += 1;
            }
        }
        seen
    });
    for round in 0..200 {
        let v: u8 = if round % 2 == 0 { 0x22 } else { 0x11 };
        let pages = vec![(0xA010_0000 / PAGE, vec![v; PAGE as usize])];
        m.restore_pages(pages);
    }
    stop.store(true, Ordering::Relaxed);
    assert!(reader.join().unwrap() > 0, "the reader ran");
    assert!(matches!(m.read32(0xA010_0000), 0x1111_1111 | 0x2222_2222));
}

/// Apple: guest 0xC0000000..0xE0000000 is an MMU alias of 0xA0000000.. (`flat::alias`), so the
/// recompiled code's fast loads and stores need no translation (#6). The same bytes as through
/// `translate`, across 64 KB pages and the 0xBFFFFFFF/0xC0000000 edge too; 0xE0000000.. still
/// goes through `translate` (physical + 0x1000); a snapshot holds the alias range once.
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
#[test]
fn alias_range_is_the_same_memory() {
    let mut m = Memory::new();
    m.write32(0xC000_1234, 0x1122_3344);
    assert_eq!(m.read32(0xA000_1234), 0x1122_3344);
    m.write32(0xA000_5678, 0x5566_7788);
    assert_eq!(m.read32(0xC000_5678), 0x5566_7788);
    m.write(0xBFFF_FFFE, &[1, 2, 3, 4]);
    assert_eq!(m.read32(0xBFFF_FFFE), 0x0102_0304);
    assert_eq!(m.read16(0xA000_0000), 0x0304);
    m.write32(0x4000_FFFE, 0xAABB_CCDD);
    assert_eq!(m.read32(0x4000_FFFE), 0xAABB_CCDD);
    m.write32(0xE000_0000, 0xCAFE_BABE);
    assert_eq!(m.read32(0xA000_1000), 0xCAFE_BABE);
    // bulk copies take one host copy below 0xE0000000: the same bytes as page by page
    let data: Vec<u8> = (0..300u32).map(|i| (i * 7 + 3) as u8).collect();
    m.write(0xBFFF_FF80, &data);
    let mut back = vec![0u8; 300];
    m.read(0xBFFF_FF80, &mut back);
    assert_eq!(back, data);
    let mut tail = vec![0u8; 172];
    m.read(0xA000_0000, &mut tail);
    assert_eq!(tail, data[128..]);
    m.write(0xDFFF_FFF0, &data[..32]); // runs into 0xE0000000: the page-by-page path
    let mut edge = [0u8; 32];
    m.read(0xDFFF_FFF0, &mut edge);
    assert_eq!(edge, data[..32]);
    assert!(m.snapshot_pages().iter().all(|(i, _)| !(0xC000..0xE000).contains(i)));
}

/// Accesses at the 0xC0000000 edge, where the guest address stops being the host offset on
/// Windows (the aliases of physical memory start there): the fast path's single compare must hand
/// an access running into 0xC0000000 to the translating path, byte for byte, and accesses that
/// cross a 64 KB page below it are linear.
#[test]
fn accesses_at_the_alias_edge_split_byte_by_byte() {
    let mut m = Memory::new();
    // a word straddling 0xC0000000: the high half lands at 0xA0000000 (physical memory)
    m.write32(0xBFFF_FFFE, 0x1122_3344);
    assert_eq!(m.read8(0xBFFF_FFFE), 0x11);
    assert_eq!(m.read8(0xBFFF_FFFF), 0x22);
    assert_eq!(m.read8(0xA000_0000), 0x33);
    assert_eq!(m.read8(0xA000_0001), 0x44);
    assert_eq!(m.read32(0xBFFF_FFFE), 0x1122_3344);
    // a doubleword crossing a 64 KB page below the edge
    m.write64(0x8201_FFFC, 0x0102_0304_0506_0708);
    assert_eq!(m.read32(0x8201_FFFC), 0x0102_0304);
    assert_eq!(m.read32(0x8202_0000), 0x0506_0708);
    assert_eq!(m.read64(0x8201_FFFC), 0x0102_0304_0506_0708);
    // a bulk copy across the edge, both ways
    let data: Vec<u8> = (1..=32).collect();
    m.write(0xBFFF_FFF0, &data);
    let mut out = [0u8; 32];
    m.read(0xBFFF_FFF0, &mut out);
    assert_eq!(out[..], data[..]);
    let mut phys = [0u8; 16];
    m.read(0xA000_0000, &mut phys);
    assert_eq!(phys[..], data[16..]);
}

/// Windows reserves guest memory and commits it on first touch: many live memories at once (more
/// than the commit limit would hold at 4 GB each) all work, reads of untouched memory are zero,
/// and a reader on another thread may be the first to touch a page.
#[cfg(windows)]
#[test]
fn many_memories_commit_only_what_they_touch() {
    let mut ms: Vec<Memory> = (0..24).map(|_| Memory::new()).collect();
    for (i, m) in ms.iter_mut().enumerate() {
        let i = i as u32;
        m.write32(0x4000_0000 + i * 0x10_0000, i);
        m.write32(0x8200_0000 + i * 4, !i);
        m.write32(0xC000_1000 + i * 0x1_0000, i * 3); // through an alias: lands at 0xA0000000+
        assert_eq!(m.read32(0x7000_0000), 0, "untouched memory reads as zero");
    }
    for (i, m) in ms.iter_mut().enumerate() {
        let i = i as u32;
        assert_eq!(m.read32(0x4000_0000 + i * 0x10_0000), i);
        assert_eq!(m.read32(0x8200_0000 + i * 4), !i);
        assert_eq!(m.read32(0xA000_1000 + i * 0x1_0000), i * 3);
    }
    let r = ms[0].reader();
    let t = std::thread::spawn(move || r.read32(0x5000_0000));
    assert_eq!(t.join().unwrap(), 0);
}
