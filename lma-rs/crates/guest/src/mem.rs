//! Big-endian 32-bit guest address space.
//!
//! On Apple platforms (macOS, iOS) it is one flat 4 GB host reservation (`flat::Map`): a guest
//! access is `base + translate(addr)`, with no page table; the OS zero-fills pages on first touch.
//! Elsewhere it is a table of 64 KB pages allocated on first touch. Both behave byte-for-byte
//! the same (and save states are the same list of non-zero 64 KB pages).

const PAGE_BITS: u32 = 16;
const PAGE: usize = 1 << PAGE_BITS;
const PAGES: usize = 1 << (32 - PAGE_BITS);
/// Fixed-size pages and page table: a guest access needs no bounds or length checks.
#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
type Page = Box<[u8; PAGE]>;

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
fn new_page(mut b: Vec<u8>) -> Page {
    b.resize(PAGE, 0);
    b.into_boxed_slice().try_into().unwrap()
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;

#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
mod flat {
    use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

    /// The whole 4 GB guest space as one anonymous host mapping, zero-filled lazily by the OS.
    /// Lives until the last `Memory`/`MemReader` holding it is gone, so the render thread can
    /// never read freed memory.
    ///
    /// Write watch (#6, what `MEM_WRITE_WATCH` gives the Windows mapping): a host page a query
    /// asked about is made read-only; the next write to it faults, the handler makes it writable
    /// again and then counts the fault, and the next query of that page sees the count changed,
    /// reports the page written and protects it again. Pages nobody asked about are never
    /// protected. Bulk host writes (`host_write`: resolve copies landing) unprotect their range
    /// in one call instead of faulting page by page. A syscall writing into a protected page
    /// would get EFAULT rather than a fault, so no system call may be handed guest memory as a
    /// buffer (vfs reads go through a host Vec, #328).
    pub struct Map {
        pub base: *mut u8,
        /// Per host page: faults taken (bumped after the page is writable again).
        faults: Box<[AtomicU32]>,
        /// Per host page: the fault count the last query saw.
        seen: Box<[AtomicU32]>,
        /// Per host page, one bit: read-only right now.
        protected: Box<[AtomicU64]>,
        /// Per 4 KB page, one bit: written since the last query of it. A fault marks its host
        /// page's four; a bulk host write marks exactly the pages it covers, so two resolves
        /// landing in one 16 KB page (bands of one target, 4 KB apart) don't invalidate each other.
        dirty: Box<[AtomicU64]>,
        /// Linux / Android: the memfd behind guest physical memory and its alias (`alias`); -1 on
        /// Apple platforms, whose alias is a remap of the anonymous mapping itself.
        alias_fd: std::sync::atomic::AtomicI32,
    }
    // SAFETY: the mapping is plain memory owned by this value; the guest thread writes it through
    // `&mut Memory` and the render thread copies through raw pointers, as with the paged store.
    unsafe impl Send for Map {}
    unsafe impl Sync for Map {}

    pub const LEN: usize = 1 << 32;
    /// Apple silicon's page: the unit of protection. 4 KB host pages (Linux / Android) protect it
    /// as four, the mapping aligned to it (asserted in `new`).
    pub const OS_PAGE: usize = 1 << 14;
    const HOST_PAGES: usize = LEN / OS_PAGE;
    const SUB: usize = OS_PAGE / 0x1000;

    fn map_at(addr: *mut u8, fixed: bool) -> *mut u8 {
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE | if fixed { libc::MAP_FIXED } else { 0 };
        // smaller host pages than OS_PAGE (Linux / Android): one OS_PAGE more, trimmed to an
        // OS_PAGE-aligned 4 GB, so the write watch's units are whole host pages
        let extra = if fixed || cfg!(target_vendor = "apple") { 0 } else { OS_PAGE };
        // SAFETY: an anonymous mapping; MAP_FIXED only ever replaces our own mapping.
        let p = unsafe { libc::mmap(addr.cast(), LEN + extra, libc::PROT_READ | libc::PROT_WRITE, flags, -1, 0) };
        assert!(p != libc::MAP_FAILED, "can't reserve 4 GB for guest memory: {}", std::io::Error::last_os_error());
        if extra == 0 {
            return p.cast();
        }
        let (start, end) = (p as usize, p as usize + LEN + extra);
        let aligned = (start + OS_PAGE - 1) & !(OS_PAGE - 1);
        // SAFETY: the unaligned head and the tail past 4 GB of the mapping just made
        unsafe {
            if aligned > start {
                libc::munmap(start as *mut libc::c_void, aligned - start);
            }
            if end > aligned + LEN {
                libc::munmap((aligned + LEN) as *mut libc::c_void, end - aligned - LEN);
            }
        }
        aligned as *mut u8
    }

    /// Guest 0xC0000000..0xE0000000 reads and writes physical memory like 0xA0000000.. (see
    /// `Memory::translate`). Mapping it as an MMU alias of that range (the same pages at both
    /// addresses) makes the guest address the host offset everywhere below 0xE0000000, so the
    /// recompiled code's loads and stores need no translation or page check (#6).
    pub const ALIAS: std::ops::Range<usize> = 0xC000_0000..0xE000_0000;
    const ALIAS_OF: usize = 0xA000_0000;
    const ALIAS_SHIFT: usize = ALIAS.start - ALIAS_OF;

    #[cfg(target_vendor = "apple")]
    extern "C" {
        static mach_task_self_: u32;
        fn mach_vm_remap(target: u32, address: *mut u64, size: u64, mask: u64, flags: i32, src_task: u32, src_address: u64, copy: i32, cur: *mut i32, max: *mut i32, inherit: u32) -> i32;
    }

    /// Map the alias; the memfd behind it, or -1 (Apple: a remap of the mapping itself).
    #[cfg(target_vendor = "apple")]
    fn alias(base: *mut u8) -> libc::c_int {
        const VM_FLAGS_FIXED: i32 = 0;
        const VM_FLAGS_OVERWRITE: i32 = 0x4000;
        const VM_INHERIT_SHARE: u32 = 0;
        let mut at = base as u64 + ALIAS.start as u64;
        let (mut cur, mut max) = (0, 0);
        // SAFETY: remaps part of our own mapping onto another part of it (copy = false: shared)
        let r = unsafe { mach_vm_remap(mach_task_self_, &mut at, (ALIAS.end - ALIAS.start) as u64, 0, VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE, mach_task_self_, base as u64 + ALIAS_OF as u64, 0, &mut cur, &mut max, VM_INHERIT_SHARE) };
        assert!(r == 0 && at == base as u64 + ALIAS.start as u64, "mach_vm_remap for the guest memory alias failed: {r}");
        -1
    }

    /// Linux / Android (#6, phones): private anonymous memory can't be mapped twice, so guest
    /// physical memory (0xA0000000..0xC0000000) is a memfd, mapped there and again at the alias.
    /// Its pages are allocated on first touch, like the rest.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn alias(base: *mut u8) -> libc::c_int {
        let len = ALIAS.end - ALIAS.start;
        // SAFETY: plain syscalls; the two fixed mappings replace part of our own reservation
        unsafe {
            let fd = libc::memfd_create(c"lma-guest-physical".as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0, "memfd_create for guest physical memory: {}", std::io::Error::last_os_error());
            assert_eq!(libc::ftruncate(fd, len as libc::off_t), 0, "ftruncate of guest physical memory: {}", std::io::Error::last_os_error());
            for at in [ALIAS_OF, ALIAS.start] {
                let want = base.add(at).cast::<libc::c_void>();
                let p = libc::mmap(want, len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_FIXED, fd, 0);
                assert!(p == want, "mapping guest physical memory at {at:#x}: {}", std::io::Error::last_os_error());
            }
            fd
        }
    }

    // ---- the fault handler: every live Map, and the handlers that were there before ours
    const MAX_MAPS: usize = 64;
    static MAPS: [AtomicPtr<Map>; MAX_MAPS] = [const { AtomicPtr::new(std::ptr::null_mut()) }; MAX_MAPS];
    static INSTALL: std::sync::Once = std::sync::Once::new();
    static OLD: [std::sync::OnceLock<libc::sigaction>; 2] = [std::sync::OnceLock::new(), std::sync::OnceLock::new()];
    const SIGNALS: [libc::c_int; 2] = [libc::SIGBUS, libc::SIGSEGV];

    fn install_handler() {
        INSTALL.call_once(|| {
            for (i, &sig) in SIGNALS.iter().enumerate() {
                // SAFETY: plain sigaction; the handler only touches atomics and mprotect
                unsafe {
                    let mut new: libc::sigaction = std::mem::zeroed();
                    new.sa_sigaction = on_fault as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as usize;
                    new.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
                    libc::sigemptyset(&mut new.sa_mask);
                    let mut old: libc::sigaction = std::mem::zeroed();
                    assert_eq!(libc::sigaction(sig, &new, &mut old), 0, "sigaction for the guest write watch");
                    let _ = OLD[i].set(old);
                }
            }
        });
    }

    extern "C" fn on_fault(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
        // SAFETY: the kernel hands a valid siginfo
        #[cfg(target_vendor = "apple")]
        let addr = unsafe { (*info).si_addr } as usize;
        // SAFETY: as above (libc's accessor on Linux)
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let addr = unsafe { (*info).si_addr() } as usize;
        for slot in &MAPS {
            let p = slot.load(Ordering::Acquire);
            if p.is_null() {
                continue;
            }
            // SAFETY: a registered map stays alive until `Drop` unregisters it
            let map = unsafe { &*p };
            let base = map.base as usize;
            if (base..base + LEN).contains(&addr) {
                map.fault(addr - base);
                return; // the write is retried
            }
        }
        // not ours: the handler that was there before (Rust's stack-overflow check, or the default)
        let i = SIGNALS.iter().position(|&s| s == sig).unwrap_or(0);
        let Some(old) = OLD[i].get() else { return };
        // SAFETY: calling the previous handler the way the kernel would
        unsafe {
            if old.sa_sigaction == libc::SIG_DFL {
                libc::signal(sig, libc::SIG_DFL); // returning re-faults into the default action
            } else if old.sa_sigaction != libc::SIG_IGN {
                if old.sa_flags & libc::SA_SIGINFO != 0 {
                    let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) = std::mem::transmute(old.sa_sigaction);
                    f(sig, info, ctx);
                } else {
                    let f: extern "C" fn(libc::c_int) = std::mem::transmute(old.sa_sigaction);
                    f(sig);
                }
            }
        }
    }

    /// `n` zero atomics without touching each one (calloc: 540k atomics built one by one took
    /// ~15 ms in a debug build, and guest's native_fallbacks test makes 400k memories).
    fn zeroed<A>(n: usize) -> Box<[A]> {
        // SAFETY: AtomicU32 / AtomicU64 are valid when all their bytes are zero
        unsafe { Box::<[A]>::new_zeroed_slice(n).assume_init() }
    }

    impl Map {
        pub fn new() -> Self {
            // SAFETY: a plain query
            let host_page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            assert!(host_page > 0 && OS_PAGE % host_page == 0, "the write watch needs host pages of at most 16 KB, not {host_page}");
            let base = map_at(std::ptr::null_mut(), false);
            let fd = alias(base);
            install_handler();
            Map {
                base,
                faults: zeroed(HOST_PAGES),
                seen: zeroed(HOST_PAGES),
                protected: zeroed(HOST_PAGES / 64),
                dirty: zeroed(LEN / 0x1000 / 64),
                alias_fd: std::sync::atomic::AtomicI32::new(fd),
            }
        }

        /// Make the handler aware of this map (once it has a fixed address: in its `Arc`).
        pub fn register(self: &std::sync::Arc<Self>) {
            let p = std::sync::Arc::as_ptr(self).cast_mut();
            for slot in &MAPS {
                if slot.compare_exchange(std::ptr::null_mut(), p, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                    return;
                }
            }
            panic!("more than {MAX_MAPS} guest memories alive");
        }

        /// All zeros again (a state load): the same addresses, fresh zero pages (so no page is
        /// protected any more; every protected page counts as written).
        pub fn reset(&self) {
            map_at(self.base, true);
            let old = self.alias_fd.swap(alias(self.base), Ordering::AcqRel);
            if old >= 0 {
                // SAFETY: the previous memfd, no longer mapped
                unsafe { libc::close(old) };
            }
            for (w, word) in self.protected.iter().enumerate() {
                let bits = word.swap(0, Ordering::AcqRel);
                for b in 0..64 {
                    if bits & (1 << b) != 0 {
                        self.faults[w * 64 + b].fetch_add(1, Ordering::Release);
                        self.mark(w * 64 + b);
                    }
                }
            }
        }

        /// For each 64 KB guest page, whether it was ever touched (written, or read and so
        /// mapped). macOS/iOS `mincore` flags compressed and paged-out pages too, so a touched
        /// page never reads as untouched.
        #[cfg(target_vendor = "apple")]
        pub fn touched(&self) -> Vec<bool> {
            // SAFETY: plain syscalls on our own mapping
            let mut v = vec![0 as libc::c_char; HOST_PAGES];
            let r = unsafe { libc::mincore(self.base.cast(), LEN, v.as_mut_ptr()) };
            assert!(r == 0, "mincore: {}", std::io::Error::last_os_error());
            let per = super::PAGE / OS_PAGE;
            v.chunks(per).map(|c| c.iter().any(|&f| f != 0)).collect()
        }

        /// `touched` on Linux / Android, where `mincore` misses swapped-out pages (zram on a
        /// phone) and a save state would lose them: the private part from /proc/self/pagemap
        /// (present or swapped), guest physical memory from its memfd's data ranges.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        pub fn touched(&self) -> Vec<bool> {
            use std::os::unix::fs::FileExt;
            let host_page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let mut units = vec![false; HOST_PAGES];
            let pagemap = std::fs::File::open("/proc/self/pagemap").expect("/proc/self/pagemap (save states need every page ever touched)");
            let (first, pages) = (self.base as usize / host_page, LEN / host_page);
            let mut buf = vec![0u8; 8 << 16];
            let mut i = 0;
            while i < pages {
                let n = (pages - i).min(1 << 16);
                pagemap.read_exact_at(&mut buf[..8 * n], 8 * (first + i) as u64).expect("reading /proc/self/pagemap");
                for k in 0..n {
                    // bit 63 present, bit 62 swapped
                    if u64::from_le_bytes(buf[8 * k..8 * k + 8].try_into().unwrap()) >> 62 != 0 {
                        units[(i + k) * host_page / OS_PAGE] = true;
                    }
                }
                i += n;
            }
            let fd = self.alias_fd.load(Ordering::Acquire);
            let mut off: libc::off_t = 0;
            // SAFETY: plain lseeks on our own memfd
            while let Some(data) = Some(unsafe { libc::lseek(fd, off, libc::SEEK_DATA) }).filter(|&d| d >= 0) {
                let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
                let hole = if hole < 0 { (ALIAS.end - ALIAS.start) as libc::off_t } else { hole };
                for u in (ALIAS_OF + data as usize) / OS_PAGE..(ALIAS_OF + hole as usize).div_ceil(OS_PAGE) {
                    units[u] = true;
                }
                off = hole;
            }
            let per = super::PAGE / OS_PAGE;
            units.chunks(per).map(|c| c.iter().any(|&t| t)).collect()
        }

        // ---- write watch
        /// The physical host page of host offset `off` (the alias folded onto 0xA0000000..).
        fn page_of(off: usize) -> usize {
            (if ALIAS.contains(&off) { off - ALIAS_SHIFT } else { off }) / OS_PAGE
        }
        fn is_protected(&self, hp: usize) -> bool {
            self.protected[hp / 64].load(Ordering::Acquire) & (1 << (hp % 64)) != 0
        }
        /// Set the protection of host pages `first..=last` (and their alias twins, for physical
        /// pages) in one call per run: a resolve landing covers ~1,000 pages every frame, and a
        /// syscall per page cost the render thread more than the hashing it replaced.
        fn protect(&self, first: usize, last: usize, prot: libc::c_int) {
            let (off, len) = (first * OS_PAGE, (last + 1 - first) * OS_PAGE);
            // SAFETY: host pages of our own mapping
            unsafe { libc::mprotect(self.base.add(off).cast(), len, prot) };
            let (lo, hi) = (off.max(ALIAS_OF), (off + len).min(ALIAS.start));
            if lo < hi {
                // SAFETY: their alias (the same pages mapped again at ALIAS)
                unsafe { libc::mprotect(self.base.add(lo + ALIAS_SHIFT).cast(), hi - lo, prot) };
            }
        }
        /// Clear the protected bit of `hp`; whether it was set.
        fn take_bit(&self, hp: usize) -> bool {
            self.protected[hp / 64].fetch_and(!(1 << (hp % 64)), Ordering::AcqRel) & (1 << (hp % 64)) != 0
        }
        /// Set the protected bit of `hp`; whether it was clear.
        fn set_bit(&self, hp: usize) -> bool {
            self.protected[hp / 64].fetch_or(1 << (hp % 64), Ordering::AcqRel) & (1 << (hp % 64)) == 0
        }
        /// Make a page writable again and count the fault, in that order: a query that protects
        /// the page and then reads the count either sees this fault (and protects again) or
        /// protected after it, so no write between the two goes unreported.
        fn unprotect(&self, hp: usize) {
            if self.take_bit(hp) {
                self.protect(hp, hp, libc::PROT_READ | libc::PROT_WRITE);
                self.faults[hp].fetch_add(1, Ordering::Release);
            }
        }
        /// Every 4 KB page of host page `hp` written.
        fn mark(&self, hp: usize) {
            let p = hp * SUB;
            self.dirty[p / 64].fetch_or(((1u64 << SUB) - 1) << (p % 64), Ordering::AcqRel);
        }
        /// A write faulted at host offset `off`. A page that isn't protected (another thread's
        /// handler got there first) just retries.
        fn fault(&self, off: usize) {
            let hp = Self::page_of(off);
            self.unprotect(hp);
            self.mark(hp);
        }
        /// The physical 4 KB page of host offset `off`.
        fn sub_of(off: usize) -> usize {
            (if ALIAS.contains(&off) { off - ALIAS_SHIFT } else { off }) >> 12
        }
        /// A bulk host write of `len` bytes at host offset `off` is about to happen: unprotect its
        /// pages first (one fault per 16 KB would cost a resolve landing ~4,000 signals a frame).
        pub fn host_write(&self, off: usize, len: usize) {
            let (first, last) = (Self::page_of(off), Self::page_of(off + len.max(1) - 1));
            let mut hp = first;
            while hp <= last {
                if !self.take_bit(hp) {
                    hp += 1;
                    continue;
                }
                // a run of protected pages: one mprotect, then the counts (unprotect first, as `unprotect`)
                let start = hp;
                while hp < last && self.take_bit(hp + 1) {
                    hp += 1;
                }
                self.protect(start, hp, libc::PROT_READ | libc::PROT_WRITE);
                for p in start..=hp {
                    self.faults[p].fetch_add(1, Ordering::Release);
                }
                hp += 1;
            }
            // exactly the 4 KB pages written (the fault path can only name a host page)
            let (lo, hi) = (Self::sub_of(off), Self::sub_of(off + len.max(1) - 1));
            for p in lo..=hi {
                self.dirty[p / 64].fetch_or(1 << (p % 64), Ordering::AcqRel);
            }
        }
        /// Translated 4 KB page offsets in `len` bytes at host offset `off` written since the last
        /// query of each (the query resets them and protects the pages again).
        pub fn take_written_range(&self, off: usize, len: usize) -> Vec<usize> {
            let len = len.min(LEN - off);
            let (first, last) = (Self::page_of(off), Self::page_of(off + len.max(1) - 1));
            let mut out = Vec::new();
            // protect the unprotected pages first, a run at a time (bits set before the mprotect:
            // nothing can fault on a page that is still writable)
            let mut hp = first;
            while hp <= last {
                if !self.set_bit(hp) {
                    hp += 1;
                    continue;
                }
                let start = hp;
                while hp < last && self.set_bit(hp + 1) {
                    hp += 1;
                }
                self.protect(start, hp, libc::PROT_READ);
                hp += 1;
            }
            for hp in first..=last {
                loop {
                    if self.set_bit(hp) {
                        self.protect(hp, hp, libc::PROT_READ); // a fault got in since: protect again
                    }
                    let f = self.faults[hp].load(Ordering::Acquire);
                    if f == self.seen[hp].load(Ordering::Relaxed) {
                        break; // protected, and no fault since: the dirty bits are complete
                    }
                    self.seen[hp].store(f, Ordering::Relaxed);
                }
            }
            // the 4 KB pages written in the range, taken (a neighbour's query keeps its own)
            let (lo, hi) = (Self::sub_of(off), Self::sub_of(off + len - 1));
            for p in lo..=hi {
                if self.dirty[p / 64].fetch_and(!(1 << (p % 64)), Ordering::AcqRel) & (1 << (p % 64)) != 0 {
                    out.push(p << 12);
                }
            }
            out
        }
    }

    impl Drop for Map {
        fn drop(&mut self) {
            let me = self as *mut Map;
            for slot in &MAPS {
                let _ = slot.compare_exchange(me, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Relaxed);
            }
            // SAFETY: our own mapping, no references left (we are the last Arc)
            unsafe { libc::munmap(self.base.cast(), LEN) };
            let fd = self.alias_fd.load(Ordering::Acquire);
            if fd >= 0 {
                // SAFETY: our memfd, its mappings gone
                unsafe { libc::close(fd) };
            }
        }
    }
}

/// Windows (#6): the same flat 4 GB space as on Apple platforms. Committed once (demand-zero,
/// so only touched pages take RAM); the OS's write watch tells which pages were written, for
/// save states. A state load zeroes the written pages in place instead of decommitting, so a
/// render thread reading meanwhile never faults.
#[cfg(windows)]
mod flat {
    use std::ffi::c_void;

    pub struct Map {
        pub base: *mut u8,
        /// 64 KB pages whose write-watch bits were taken by `take_written_range` (a range query
        /// resets them): kept here so `touched` still lists every page ever written.
        taken: std::sync::Mutex<Vec<u64>>,
    }
    // SAFETY: as on Apple platforms, plain memory owned by this value
    unsafe impl Send for Map {}
    unsafe impl Sync for Map {}

    pub const LEN: usize = 1 << 32;
    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_RELEASE: u32 = 0x8000;
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    const PAGE_READWRITE: u32 = 0x04;
    const OS_PAGE: usize = 4096;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
        fn VirtualFree(addr: *mut c_void, size: usize, kind: u32) -> i32;
        fn GetWriteWatch(flags: u32, base: *mut c_void, size: usize, addrs: *mut *mut c_void, count: *mut usize, granularity: *mut u32) -> u32;
        fn ResetWriteWatch(base: *mut c_void, size: usize) -> u32;
        fn AddVectoredExceptionHandler(first: u32, handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32) -> *mut c_void;
    }

    /// Commit granularity on first touch: a game touches ~1 GB, so a few hundred faults.
    const CHUNK: usize = 1 << 20;
    const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
    const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

    /// `EXCEPTION_RECORD` (x64 layout).
    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        record: *mut ExceptionRecord,
        address: *mut c_void,
        nparams: u32,
        info: [usize; 15],
    }
    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut c_void,
    }

    /// Base addresses of the live reserved-only maps (0: free slot), for the fault handler.
    static MAPS: [std::sync::atomic::AtomicUsize; 64] = [const { std::sync::atomic::AtomicUsize::new(0) }; 64];

    /// Commit a guest memory chunk on its first access (read or write, from any thread): the
    /// map is only reserved, so untouched memory costs no commit. Other faults pass through.
    unsafe extern "system" fn on_fault(e: *mut ExceptionPointers) -> i32 {
        // SAFETY: the system passes valid exception pointers
        let r = unsafe { &*(*e).record };
        if r.code != EXCEPTION_ACCESS_VIOLATION || r.nparams < 2 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let a = r.info[1];
        for m in &MAPS {
            let b = m.load(std::sync::atomic::Ordering::Acquire);
            if b != 0 && a >= b && a < b + LEN {
                let at = b + (a - b) / CHUNK * CHUNK;
                // at the commit limit, wait for memory to be freed as `new` used to (#315)
                for _ in 0..600 {
                    // SAFETY: a chunk of a reserved map (committing a committed page is a no-op)
                    if !unsafe { VirtualAlloc(at as *mut c_void, CHUNK, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
                        return EXCEPTION_CONTINUE_EXECUTION;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                return EXCEPTION_CONTINUE_SEARCH;
            }
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    impl Map {
        pub fn new() -> Self {
            // Reserved only: committing 4 GB up front counted against the system's commit limit
            // (RAM + page file) though a game touches about a quarter of it, and tests running in
            // parallel each made their own (36 GB on the PC: CI's tests failed). `on_fault`
            // commits a chunk on its first access. The reservation carries the write watch.
            static HANDLER: std::sync::Once = std::sync::Once::new();
            // SAFETY: registers a handler that only acts on faults inside our own maps
            HANDLER.call_once(|| assert!(!unsafe { AddVectoredExceptionHandler(1, on_fault) }.is_null(), "AddVectoredExceptionHandler failed"));
            // SAFETY: a fresh private reservation
            let p = unsafe { VirtualAlloc(std::ptr::null_mut(), LEN, MEM_RESERVE | MEM_WRITE_WATCH, PAGE_READWRITE) };
            assert!(!p.is_null(), "can't reserve 4 GB for guest memory: {}", std::io::Error::last_os_error());
            let registered = MAPS.iter().any(|m| m.compare_exchange(0, p as usize, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Relaxed).is_ok());
            if !registered {
                // more live maps than slots: this one is committed whole, as before
                // SAFETY: our own reservation
                assert!(!unsafe { VirtualAlloc(p, LEN, MEM_COMMIT, PAGE_READWRITE) }.is_null(), "can't commit 4 GB for guest memory: {}", std::io::Error::last_os_error());
            }
            Map { base: p.cast(), taken: std::sync::Mutex::new(vec![0; LEN / super::PAGE / 64]) }
        }
        /// The OS pages in [off, off + len) written since their bits were last reset, as offsets
        /// (`reset`: those bits are cleared, a whole-map query costs ~10 ms, a texture's ~1 us).
        fn written_in(&self, off: usize, len: usize, reset: bool) -> Vec<usize> {
            let mut addrs = vec![std::ptr::null_mut::<c_void>(); len.div_ceil(OS_PAGE) + 1];
            let mut count = addrs.len();
            let mut gran = 0u32;
            // SAFETY: a range of our own allocation; `addrs` holds one entry per OS page in it
            let r = unsafe { GetWriteWatch(if reset { WRITE_WATCH_FLAG_RESET } else { 0 }, self.base.add(off).cast(), len, addrs.as_mut_ptr(), &mut count, &mut gran) };
            assert!(r == 0, "GetWriteWatch: {}", std::io::Error::last_os_error());
            addrs[..count].iter().map(|&a| a as usize - self.base as usize).collect()
        }
        fn written(&self) -> Vec<usize> {
            self.written_in(0, LEN, false)
        }
        /// The OS pages in [off, off + len) written since the last query of each, and resets them
        /// (render-side change detection, #6); `touched` still counts them.
        pub fn take_written_range(&self, off: usize, len: usize) -> Vec<usize> {
            let len = len.min(LEN - off);
            let mut taken = self.taken.lock().unwrap();
            let w = self.written_in(off, len, true);
            for &o in &w {
                let p = o / super::PAGE;
                taken[p / 64] |= 1 << (p % 64);
            }
            w
        }
        /// All zeros again (a state load): the written pages are cleared in place.
        pub fn reset(&self) {
            let mut taken = self.taken.lock().unwrap();
            for off in self.written() {
                // SAFETY: an OS page inside the mapping
                unsafe { std::ptr::write_bytes(self.base.add(off), 0, OS_PAGE) };
            }
            for (i, bits) in taken.iter_mut().enumerate() {
                for b in 0..64 {
                    if *bits & (1 << b) != 0 {
                        // SAFETY: a 64 KB page inside the mapping
                        unsafe { std::ptr::write_bytes(self.base.add((i * 64 + b) * super::PAGE), 0, super::PAGE) };
                    }
                }
                *bits = 0;
            }
            // SAFETY: our own allocation
            unsafe { ResetWriteWatch(self.base.cast(), LEN) };
        }
        /// For each 64 KB guest page, whether it was ever written (untouched pages read as zero).
        pub fn touched(&self) -> Vec<bool> {
            let taken = self.taken.lock().unwrap();
            let mut v: Vec<bool> = (0..LEN / super::PAGE).map(|p| taken[p / 64] & (1 << (p % 64)) != 0).collect();
            for off in self.written() {
                v[off / super::PAGE] = true;
            }
            v
        }
    }

    impl Drop for Map {
        fn drop(&mut self) {
            for m in &MAPS {
                let _ = m.compare_exchange(self.base as usize, 0, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Relaxed);
            }
            // SAFETY: our own allocation, no references left (we are the last Arc)
            unsafe { VirtualFree(self.base.cast(), 0, MEM_RELEASE) };
        }
    }
}

/// Guest addresses below this are host offsets into the flat mapping: on Apple 0xC0000000.. is an
/// MMU alias of 0xA0000000.. (`flat::alias`); on Windows the aliases start at 0xC0000000 (a
/// section view there would lose MEM_WRITE_WATCH, which the texture write watch needs).
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
const LINEAR_END: u32 = 0xE000_0000;
#[cfg(windows)]
const LINEAR_END: u32 = 0xC000_0000;

pub struct Memory {
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    pages: Box<[Option<Page>; PAGES]>,
    /// Every page's bytes for `MemReader`s (null: not allocated, reads as zeros). Set when a
    /// page is allocated; a page's heap block never moves until `restore_pages`.
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    table: Arc<Table>,
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    flat: Arc<flat::Map>,
    /// `flat.base`, kept here so a guest access loads it from `Memory` directly instead of
    /// through the `Arc` (one dependent load fewer per access, #6). The mapping never moves.
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    base: *mut u8,
}

// SAFETY: `base` points into the mapping `flat` keeps alive; it is only used through &mut self
// (or &self for reads), like `flat` itself, which is Send + Sync
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
unsafe impl Send for Memory {}
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
unsafe impl Sync for Memory {}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
struct Table {
    pages: Box<[AtomicPtr<u8>]>,
    /// Reader accesses in progress: freeing pages (`restore_pages`, drop) clears `pages` first,
    /// then waits for these to finish.
    active: AtomicUsize,
}

/// A read-only view of guest memory for another thread (#95: the render thread reads vertex
/// buffers and textures itself, like the real GPU). The guest keeps writing meanwhile, so reads
/// see bytes as they are at that moment, as the console's GPU does; ordering comes from
/// fences. Pages freed meanwhile (`Memory::restore_pages`, drop) read as zeros and take no
/// writes; freeing waits for accesses in progress.
#[derive(Clone)]
pub struct MemReader {
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    table: Arc<Table>,
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    flat: Arc<flat::Map>,
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

/// Read watch (feature `rwatch`, analysis builds): the dispatcher reports loads that touch the
/// word at `RWATCH` (0 = off) with the pc of the next check point, which is in the reading block.
#[cfg(feature = "rwatch")]
pub static RWATCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "rwatch")]
pub static RWATCH_HITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "rwatch")]
#[inline(always)]
fn rwatch(a: u32, n: u32) {
    let w = RWATCH.load(std::sync::atomic::Ordering::Relaxed);
    if w != 0 && a < w.wrapping_add(4) && w < a.wrapping_add(n) {
        RWATCH_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Memory {
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    pub fn new() -> Self {
        let pages = (0..PAGES).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect();
        let mine = vec![None; PAGES].into_boxed_slice().try_into().unwrap();
        Self { pages: mine, table: Arc::new(Table { pages, active: AtomicUsize::new(0) }) }
    }
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    pub fn new() -> Self {
        let flat = Arc::new(flat::Map::new());
        let base = flat.base;
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
        flat.register();
        Self { flat, base }
    }

    /// Physical memory is mapped three times (0xA0000000 64K pages, 0xC0000000 16M pages,
    /// 0xE0000000 4K pages, the last offset by 0x1000 like Xenia); fold the aliases onto 0xA0.
    #[inline]
    pub fn translate(addr: u32) -> u32 {
        if addr >= 0xE000_0000 {
            0xA000_0000 | ((addr - 0xE000_0000 + 0x1000) & 0x1FFF_FFFF)
        } else if addr >= 0xC000_0000 {
            0xA000_0000 | (addr & 0x1FFF_FFFF)
        } else {
            addr
        }
    }

    /// The 64 KB page holding translated address `addr`.
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    fn page(&mut self, addr: u32) -> &mut [u8] {
        let i = (addr >> PAGE_BITS) as usize;
        if self.pages[i].is_none() {
            self.map(i);
        }
        &mut self.pages[i].as_deref_mut().unwrap()[..]
    }
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    #[inline(always)]
    fn page(&mut self, addr: u32) -> &mut [u8] {
        // SAFETY: inside the 4 GB mapping (any u32 page start + 64 KB), borrowed through &mut self
        unsafe { std::slice::from_raw_parts_mut(self.base.add((addr as usize) & !(PAGE - 1)), PAGE) }
    }

    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    #[cold]
    fn map(&mut self, i: usize) {
        let mut b = new_page(Vec::new());
        self.table.pages[i].store(b.as_mut_ptr(), Ordering::Release);
        self.pages[i] = Some(b);
    }

    /// Clear the reader table and wait until no reader can still reach a page.
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    fn detach_readers(&self) {
        self.table.pages.iter().for_each(|p| p.store(std::ptr::null_mut(), Ordering::SeqCst));
        while self.table.active.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
        }
    }

    /// A view another thread can read guest memory through (see `MemReader`).
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    pub fn reader(&self) -> MemReader {
        MemReader { table: self.table.clone() }
    }
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    pub fn reader(&self) -> MemReader {
        MemReader { flat: self.flat.clone() }
    }

    /// `MemReader::write_watch` for the guest thread's own view (deterministic rendering runs
    /// inline on it).
    pub fn write_watch(&self, a: u32, len: u32) -> Option<Vec<u32>> {
        self.reader().write_watch(a, len)
    }

    /// Whether the 64 KB page holding translated address `addr` was ever touched.
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    pub fn is_mapped(&self, addr: u32) -> bool {
        self.pages[(addr >> PAGE_BITS) as usize].is_some()
    }
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    pub fn is_mapped(&self, addr: u32) -> bool {
        self.flat.touched()[(addr >> PAGE_BITS) as usize]
    }

    #[inline]
    pub fn read8(&mut self, a: u32) -> u8 {
        #[cfg(feature = "rwatch")]
        rwatch(a, 1);
        let a = Self::translate(a);
        self.page(a)[(a as usize) & (PAGE - 1)]
    }

    #[inline]
    pub fn write8(&mut self, a: u32, v: u8) {
        let a = Self::translate(a);
        self.page(a)[(a as usize) & (PAGE - 1)] = v;
    }

    /// The bytes [a, a+N) when they lie inside one mapped host page: the common case, inlined
    /// into the recompiled code. Within one translated 64 KB page every alias is linear, so this
    /// agrees with `contiguous`; anything else (page crossing, unmapped page) takes `read`/`write`.
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    #[inline(always)]
    pub(crate) fn fast<const N: usize>(&mut self, a: u32) -> Option<&mut [u8; N]> {
        // below 0xE0000000 the guest address is the host offset (0xC0000000.. is an MMU alias of
        // 0xA0000000.., `flat::alias`), across 64 KB pages too; 0xE0000000.. (physical + 0x1000)
        // and an access running into it take `read`/`write`
        if a > 0xE000_0000 - N as u32 {
            return None;
        }
        // SAFETY: a + N <= 0xE0000000, inside the 4 GB mapping
        Some(unsafe { &mut *self.base.add(a as usize).cast::<[u8; N]>() })
    }
    #[cfg(windows)]
    #[inline(always)]
    pub(crate) fn fast<const N: usize>(&mut self, a: u32) -> Option<&mut [u8; N]> {
        // below 0xC0000000 the guest address is the host offset (no alias there), across 64 KB
        // pages too: one compare (#318's fast path without an alias, which would cost the texture
        // write watch); the physical aliases above take the translating path
        if a <= LINEAR_END - N as u32 {
            // SAFETY: a + N <= 0xC0000000, inside the 4 GB mapping
            return Some(unsafe { &mut *self.base.add(a as usize).cast::<[u8; N]>() });
        }
        self.fast_translated(a)
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    #[inline(always)]
    pub(crate) fn fast<const N: usize>(&mut self, a: u32) -> Option<&mut [u8; N]> {
        self.fast_translated(a)
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    #[inline(always)]
    fn fast_translated<const N: usize>(&mut self, a: u32) -> Option<&mut [u8; N]> {
        let t = Self::translate(a);
        let off = t as usize & (PAGE - 1);
        // within one host page, which also rules out alias segment edges (below 0xE0000000 the low
        // 16 bits are kept; 0xE0000000's +0x1000 keeps its edges on page edges too). An access
        // wrapping past 0xFFFFFFFF stays in 0xA0000000's first page here instead of reaching 0:
        // still guest memory, never host memory, and checking for it costs 2.4%.
        if off + N > PAGE {
            return None;
        }
        self.fast_at::<N>(t, off)
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    #[inline(always)]
    fn fast_at<const N: usize>(&mut self, t: u32, off: usize) -> Option<&mut [u8; N]> {
        let page = self.pages[(t >> PAGE_BITS) as usize].as_deref_mut()?;
        (&mut page[off..off + N]).try_into().ok()
    }
    #[cfg(windows)]
    #[inline(always)]
    fn fast_at<const N: usize>(&mut self, t: u32, _off: usize) -> Option<&mut [u8; N]> {
        // SAFETY: t + N stays inside its 64 KB page (checked above), so inside the mapping
        Some(unsafe { &mut *self.base.add(t as usize).cast::<[u8; N]>() })
    }

    /// Translated start address if `len` bytes at `a` stay inside one host page.
    #[inline]
    fn contiguous(a: u32, len: usize) -> Option<u32> {
        let t = Self::translate(a);
        let end = a.wrapping_add(len as u32 - 1);
        let same_page = (t as usize & (PAGE - 1)) + len <= PAGE;
        (same_page && Self::translate(end) == t.wrapping_add(len as u32 - 1)).then_some(t)
    }

    /// Length of the run starting at guest address `a` (at most `left` bytes) that ends at the
    /// end of its translated host page; bulk copies go page by page.
    #[inline]
    fn run(a: u32, left: usize) -> usize {
        (PAGE - (Self::translate(a) as usize & (PAGE - 1))).min(left)
    }

    /// Bulk read, one memcpy per host page (byte by byte only for a run that is not linear,
    /// e.g. one crossing an alias segment edge), byte-identical to reading each byte.
    pub fn read(&mut self, a: u32, out: &mut [u8]) {
        // below LINEAR_END the guest address is the host offset (see `fast`): one copy (register
        // save/restore, dcbz128, partial vector loads come here, #6)
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        if a as u64 + out.len() as u64 <= LINEAR_END as u64 {
            // SAFETY: [a, a + len) lies inside the 4 GB mapping, below LINEAR_END
            unsafe { std::ptr::copy_nonoverlapping(self.base.add(a as usize), out.as_mut_ptr(), out.len()) };
            return;
        }
        let mut i = 0;
        while i < out.len() {
            let at = a.wrapping_add(i as u32);
            let n = Self::run(at, out.len() - i);
            match Self::contiguous(at, n) {
                Some(t) => {
                    let off = t as usize & (PAGE - 1);
                    out[i..i + n].copy_from_slice(&self.page(t)[off..off + n]);
                }
                None => {
                    for k in i..i + n {
                        out[k] = self.read8(a.wrapping_add(k as u32));
                    }
                }
            }
            i += n;
        }
    }

    /// Bulk write, page by page like `read`.
    pub fn write(&mut self, a: u32, data: &[u8]) {
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        if a as u64 + data.len() as u64 <= LINEAR_END as u64 {
            #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
            if data.len() >= flat::OS_PAGE {
                self.flat.host_write(a as usize, data.len()); // a watched page faults once anyway; a big copy unprotects in one go
            }
            // SAFETY: as in `read`
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.base.add(a as usize), data.len()) };
            return;
        }
        let mut i = 0;
        while i < data.len() {
            let at = a.wrapping_add(i as u32);
            let n = Self::run(at, data.len() - i);
            match Self::contiguous(at, n) {
                Some(t) => {
                    let off = t as usize & (PAGE - 1);
                    self.page(t)[off..off + n].copy_from_slice(&data[i..i + n]);
                }
                None => {
                    for k in i..i + n {
                        self.write8(a.wrapping_add(k as u32), data[k]);
                    }
                }
            }
            i += n;
        }
    }

    #[inline]
    pub fn read16(&mut self, a: u32) -> u16 {
        #[cfg(feature = "rwatch")]
        rwatch(a, 2);
        if let Some(b) = self.fast::<2>(a) {
            return u16::from_be_bytes(*b);
        }
        let mut b = [0; 2];
        self.read_cold(a, &mut b);
        u16::from_be_bytes(b)
    }
    /// N bytes at `a` through the inlined fast path (vector loads/stores, dcbz); anything the fast
    /// path can't take goes through `read`/`write`, so the bytes are the same either way.
    #[inline(always)]
    pub fn read_n<const N: usize>(&mut self, a: u32) -> [u8; N] {
        if let Some(b) = self.fast::<N>(a) {
            return *b;
        }
        let mut b = [0; N];
        self.read_cold(a, &mut b);
        b
    }
    #[inline(always)]
    pub fn write_n<const N: usize>(&mut self, a: u32, v: &[u8; N]) {
        match self.fast::<N>(a) {
            Some(b) => *b = *v,
            None => self.write_cold(a, v),
        }
    }
    /// The single-word accessors' fallback, out of line and cold: the fast path stays short and
    /// LLVM doesn't plan its register use around a call it almost never makes (#6).
    #[cold]
    #[inline(never)]
    fn read_cold(&mut self, a: u32, out: &mut [u8]) {
        self.read(a, out)
    }
    #[cold]
    #[inline(never)]
    fn write_cold(&mut self, a: u32, data: &[u8]) {
        self.write(a, data)
    }
    #[inline]
    pub fn read32(&mut self, a: u32) -> u32 {
        #[cfg(feature = "rwatch")]
        rwatch(a, 4);
        if let Some(b) = self.fast::<4>(a) {
            return u32::from_be_bytes(*b);
        }
        let mut b = [0; 4];
        self.read_cold(a, &mut b);
        u32::from_be_bytes(b)
    }
    #[inline]
    pub fn read64(&mut self, a: u32) -> u64 {
        #[cfg(feature = "rwatch")]
        rwatch(a, 8);
        if let Some(b) = self.fast::<8>(a) {
            return u64::from_be_bytes(*b);
        }
        let mut b = [0; 8];
        self.read_cold(a, &mut b);
        u64::from_be_bytes(b)
    }
    #[inline]
    pub fn write16(&mut self, a: u32, v: u16) {
        match self.fast::<2>(a) {
            Some(b) => *b = v.to_be_bytes(),
            None => self.write_cold(a, &v.to_be_bytes()),
        }
    }
    #[inline]
    pub fn write32(&mut self, a: u32, v: u32) {
        match self.fast::<4>(a) {
            Some(b) => *b = v.to_be_bytes(),
            None => self.write_cold(a, &v.to_be_bytes()),
        }
    }
    #[inline]
    pub fn write64(&mut self, a: u32, v: u64) {
        match self.fast::<8>(a) {
            Some(b) => *b = v.to_be_bytes(),
            None => self.write_cold(a, &v.to_be_bytes()),
        }
    }

    /// stwcx.: store `new` if the word at `a` still holds `old` (values as the guest sees them),
    /// as one host compare-exchange where the word is directly addressable and aligned, so it is
    /// atomic across host threads (#39); elsewhere read, compare and write.
    pub fn cas32(&mut self, a: u32, old: u32, new: u32) -> bool {
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        if a & 3 == 0 {
            if let Some(w) = self.fast::<4>(a) {
                // SAFETY: an aligned word inside the mapping; other host threads only touch it
                // through the same mapping (atomics here, plain accesses as the guest would)
                let at = unsafe { std::sync::atomic::AtomicU32::from_ptr(w.as_mut_ptr().cast()) };
                return at.compare_exchange(old.to_be(), new.to_be(), std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_ok();
            }
        }
        if self.read32(a) != old {
            return false;
        }
        self.write32(a, new);
        true
    }
    /// stdcx.: as `cas32` for a doubleword.
    pub fn cas64(&mut self, a: u32, old: u64, new: u64) -> bool {
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        if a & 7 == 0 {
            if let Some(w) = self.fast::<8>(a) {
                // SAFETY: as in `cas32`
                let at = unsafe { std::sync::atomic::AtomicU64::from_ptr(w.as_mut_ptr().cast()) };
                return at.compare_exchange(old.to_be(), new.to_be(), std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_ok();
            }
        }
        if self.read64(a) != old {
            return false;
        }
        self.write64(a, new);
        true
    }

    /// Another handle on the same guest memory, for a guest thread running on another host
    /// thread (#39): the flat mapping is shared; races between guest threads are the guest's, as
    /// on the Xbox (its atomics go through `cas32`/`cas64`).
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    pub fn share(&self) -> Memory {
        Memory { flat: self.flat.clone(), base: self.base }
    }

    /// Every mapped page that is not all zero: (page index, 64 KB of bytes). For save states.
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    pub fn snapshot_pages(&self) -> Vec<(u32, Vec<u8>)> {
        self.pages
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.as_ref().filter(|b| b.iter().any(|&x| x != 0)).map(|b| (i as u32, b.to_vec())))
            .collect()
    }

    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    pub fn snapshot_pages(&self) -> Vec<(u32, Vec<u8>)> {
        let touched = self.flat.touched();
        (0..PAGES)
            .filter(|&i| touched[i])
            // the alias shows the same pages as 0xA0000000.. (Apple): saved once, there
            .filter(|&i| !cfg!(any(target_vendor = "apple", target_os = "linux", target_os = "android")) || !Self::is_alias(i))
            .filter_map(|i| {
                // SAFETY: a page inside the mapping
                let b = unsafe { std::slice::from_raw_parts(self.base.add(i * PAGE), PAGE) };
                b.iter().any(|&x| x != 0).then(|| (i as u32, b.to_vec()))
            })
            .collect()
    }

    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    fn is_alias(page: usize) -> bool {
        (0xC000_0000 / PAGE..0xE000_0000 / PAGE).contains(&page)
    }

    /// Replace all memory with saved pages (see `snapshot_pages`).
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    pub fn restore_pages(&mut self, pages: Vec<(u32, Vec<u8>)>) {
        // a render thread reading meanwhile sees zeros or the new bytes, never freed memory
        self.flat.reset();
        for (i, b) in pages {
            let n = b.len().min(PAGE);
            self.page(i << PAGE_BITS)[..n].copy_from_slice(&b[..n]);
        }
    }

    /// Replace all memory with saved pages (see `snapshot_pages`).
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    pub fn restore_pages(&mut self, pages: Vec<(u32, Vec<u8>)>) {
        // readers first, so none can reach a page about to be freed
        self.detach_readers();
        self.pages.iter_mut().for_each(|p| *p = None);
        for (i, b) in pages {
            let mut b = new_page(b);
            self.table.pages[i as usize].store(b.as_mut_ptr(), Ordering::Release);
            self.pages[i as usize] = Some(b);
        }
    }

    /// NUL-terminated string at `a` (for logging kernel calls).
    pub fn cstr(&mut self, a: u32, max: usize) -> String {
        let mut s = Vec::new();
        for i in 0..max as u32 {
            let b = self.read8(a + i);
            if b == 0 {
                break;
            }
            s.push(b);
        }
        String::from_utf8_lossy(&s).into_owned()
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
struct Active<'a>(&'a AtomicUsize);

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
impl Drop for Memory {
    fn drop(&mut self) {
        // a render thread may still be reading (#95): it sees no pages from here on
        self.detach_readers();
    }
}

impl MemReader {
    /// Translated addresses of the 4 KB pages in `len` bytes at guest address `a` written since
    /// the last query of each (the query resets them), or None where memory has no write watch
    /// (the flat mappings have one: Windows' MEM_WRITE_WATCH, page protection on Apple). Lets the
    /// render thread skip re-hashing textures that weren't written (#6).
    pub fn write_watch(&self, a: u32, len: u32) -> Option<Vec<u32>> {
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        {
            let t = Memory::translate(a);
            Some(self.flat.take_written_range(t as usize, len as usize).into_iter().map(|o| o as u32).collect())
        }
        #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
        {
            let _ = (a, len);
            None
        }
    }

    /// `out.len()` bytes at guest address `a`, exactly as `Memory::read` returns them (pages
    /// never allocated read as zeros, and stay unallocated).
    pub fn read(&self, a: u32, out: &mut [u8]) {
        #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
        let _g = self.enter();
        let mut i = 0;
        while i < out.len() {
            let at = a.wrapping_add(i as u32);
            let n = Memory::run(at, out.len() - i);
            match Memory::contiguous(at, n) {
                Some(t) => self.copy(t, &mut out[i..i + n]),
                None => {
                    for k in i..i + n {
                        self.copy(Memory::translate(a.wrapping_add(k as u32)), &mut out[k..k + 1]);
                    }
                }
            }
            i += n;
        }
    }

    pub fn read32(&self, a: u32) -> u32 {
        let mut b = [0; 4];
        self.read(a, &mut b);
        u32::from_be_bytes(b)
    }

    /// Count an access in progress (see `Table::active`).
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    fn enter(&self) -> Active<'_> {
        self.table.active.fetch_add(1, Ordering::SeqCst);
        Active(&self.table.active)
    }

    /// The GPU's own writes (resolve copies landing), as `Memory::write` makes them. Returns false
    /// and writes nothing when a page it touches was never allocated (the reader can't allocate;
    /// the guest thread writes those).
    pub fn write(&self, a: u32, data: &[u8]) -> bool {
        #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
        let _g = self.enter();
        #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
        {
            let mut i = 0;
            while i < data.len() {
                let at = a.wrapping_add(i as u32);
                let n = Memory::run(at, data.len() - i);
                if self.table.pages[(Memory::translate(at) >> PAGE_BITS) as usize].load(Ordering::SeqCst).is_null() {
                    return false;
                }
                i += n;
            }
        }
        let mut i = 0;
        while i < data.len() {
            let at = a.wrapping_add(i as u32);
            let n = Memory::run(at, data.len() - i);
            match Memory::contiguous(at, n) {
                Some(t) => self.put(t, &data[i..i + n]),
                None => {
                    for k in i..i + n {
                        self.put(Memory::translate(a.wrapping_add(k as u32)), &data[k..k + 1]);
                    }
                }
            }
            i += n;
        }
        true
    }

    /// Bytes to translated address `t`, all within its (allocated) page.
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    fn put(&self, t: u32, data: &[u8]) {
        assert!(t as usize % PAGE + data.len() <= PAGE, "reader write crosses a page");
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
        self.flat.host_write(t as usize, data.len());
        // SAFETY: inside the mapping, which outlives this reader (Arc); copied through raw
        // pointers since the guest may access the same bytes meanwhile, as on the console
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.flat.base.add(t as usize), data.len()) }
    }
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    fn copy(&self, t: u32, out: &mut [u8]) {
        assert!(t as usize % PAGE + out.len() <= PAGE, "reader read crosses a page");
        // SAFETY: as `put`
        unsafe { std::ptr::copy_nonoverlapping(self.flat.base.add(t as usize), out.as_mut_ptr(), out.len()) }
    }

    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    fn put(&self, t: u32, data: &[u8]) {
        let p = self.table.pages[(t >> PAGE_BITS) as usize].load(Ordering::SeqCst);
        if p.is_null() {
            return; // freed since `write` checked (memory going away)
        }
        assert!(t as usize % PAGE + data.len() <= PAGE, "reader write crosses a page");
        // SAFETY: as `copy`
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), p.add(t as usize & (PAGE - 1)), data.len()) }
    }

    /// Bytes at translated address `t`, all within its page.
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
    fn copy(&self, t: u32, out: &mut [u8]) {
        let p = self.table.pages[(t >> PAGE_BITS) as usize].load(Ordering::SeqCst);
        if p.is_null() {
            out.fill(0);
            return;
        }
        // SAFETY: a page's block lives (and never moves) while this access is counted in
        // `active` and its pointer was non-null (freeing clears the table, then waits for
        // `active`); [off, off + len) stays inside it. The guest may write those bytes
        // meanwhile (as on the console), so they are copied through raw pointers, never borrowed.
        assert!(t as usize % PAGE + out.len() <= PAGE, "reader read crosses a page");
        unsafe { std::ptr::copy_nonoverlapping(p.add(t as usize & (PAGE - 1)), out.as_mut_ptr(), out.len()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_matches_read() {
        let mut m = Memory::new();
        let r = m.reader();
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 + 3) as u8).collect();
        m.write(0x4000_FF00, &data);
        m.write(0xBFFF_FFF0, &data[..32]); // across the end of the 0xA0 segment
        for &(a, n) in &[(0x4000_FF00u32, 200_000usize), (0x4001_0007, 5), (0x5000_0000, 70_000), (0xBFFF_FFF0, 32), (0xDFFF_FFF8, 16)] {
            let (mut x, mut y) = (vec![0; n], vec![1; n]);
            r.read(a, &mut y);
            m.read(a, &mut x);
            assert_eq!(x, y, "{a:#x}");
        }
        m.write32(0x8000_0000, 0x1234_5678);
        assert_eq!(r.read32(0x8000_0000), 0x1234_5678);
        assert!(r.write(0x4000_FFFE, &[1, 2, 3, 4]));
        assert_eq!(m.read32(0x4000_FFFE), 0x0102_0304);
        // the paged store can't allocate from the reader (the guest thread applies those writes);
        // flat memory has every page
        #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
        {
            assert!(!r.write(0x6000_0000, &[1]));
            assert!(!m.is_mapped(0x6000_0000));
        }
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        assert!(r.write(0x6000_0000, &[1]) && m.read8(0x6000_0000) == 1);
        m.restore_pages(vec![(0x8000, vec![9; PAGE])]);
        assert_eq!(r.read32(0x8000_0000), 0x0909_0909);
        assert_eq!(r.read32(0x4000_FF00), 0);
        drop(m); // a render thread outliving guest memory never touches freed memory:
        #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows)))]
        {
            // the paged store's reader then reads zeros and writes nothing
            assert_eq!(r.read32(0x8000_0000), 0);
            assert!(!r.write(0x8000_0000, &[1]));
        }
        // the flat mapping lives until the last reader is gone
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        assert_eq!(r.read32(0x8000_0000), 0x0909_0909);
    }

    #[test]
    fn bulk_copies_match_bytewise() {
        // ranges spanning several pages, unaligned, through the 0xC/0xE aliases and across the
        // 0xDFFF_FFFF / 0xE000_0000 segment edge
        for (start, len) in [(0x8200_fff0u32, 3 * PAGE + 37), (0xA00f_ff00, PAGE + 512), (0xC010_0100, 2 * PAGE), (0xE000_f000, 2 * PAGE + 9), (0xDFFF_ff00, 1024)] {
            let mut m = Memory::new();
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            m.write(start, &data);
            let mut bulk = vec![0u8; len];
            m.read(start, &mut bulk);
            let bytewise: Vec<u8> = (0..len).map(|i| m.read8(start.wrapping_add(i as u32))).collect();
            assert_eq!(bulk, bytewise, "{start:#x}");
            assert_eq!(bulk, data, "{start:#x}");
            // the same bytes written one by one land in the same places
            let mut r = Memory::new();
            for (i, &v) in data.iter().enumerate() {
                r.write8(start.wrapping_add(i as u32), v);
            }
            let mut back = vec![0u8; len];
            r.read(start, &mut back);
            assert_eq!(back, data, "{start:#x}");
        }
    }

    #[test]
    fn fast_and_slow_paths_agree() {
        let mut m = Memory::new();
        // inside a page, across a 64 KB page boundary, and through the 0xE0000000 alias (+0x1000)
        m.write32(0x8200_0010, 0x1122_3344);
        assert_eq!((m.read32(0x8200_0010), m.read16(0x8200_0012), m.read8(0x8200_0013)), (0x1122_3344, 0x3344, 0x44));
        m.write64(0x8200_fffc, 0x0102_0304_0506_0708);
        assert_eq!((m.read64(0x8200_fffc), m.read32(0x8201_0000), m.read8(0x8200_ffff)), (0x0102_0304_0506_0708, 0x0506_0708, 4));
        m.write32(0xE000_0000, 0xdead_beef);
        assert_eq!((m.read32(0xA000_1000), m.read32(0xC000_1000)), (0xdead_beef, 0xdead_beef));
        m.write16(0xA000_fffe, 0xabcd);
        assert_eq!((m.read16(0xA000_fffe), m.read32(0xBFFF_fffc)), (0xabcd, 0));
        // reads of unmapped memory are zero and map the page, as before
        assert_eq!(m.read32(0x4000_0000), 0);
        assert!(m.is_mapped(0x4000_0000));
    }
    /// Any guest address, any size, through every path (fast, slow, alias edges, 4 GB
    /// wraparound, the render thread's reader): stays inside guest memory and agrees with a
    /// byte-by-byte model. No address can reach host memory outside the guest pages.
    #[test]
    fn any_address_is_safe_and_byte_exact() {
        use std::collections::HashMap;
        let mut m = Memory::new();
        let r = m.reader();
        let mut model: HashMap<u32, u8> = HashMap::new();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
        // page edges, alias segment edges and the top of the address space
        let edges = [0u32, 0x8200_0000, 0x9fff_fffc, 0xa000_0000, 0xbfff_fff8, 0xc000_0000, 0xdfff_fffa, 0xe000_0000, 0xffff_effc, 0xffff_fffd];
        for _ in 0..200_000 {
            let x = rnd();
            let a = match x % 4 {
                0 => x as u32 >> 3,                                                  // anywhere
                1 => ((x >> 8) as u32 & !0xffff).wrapping_sub((x >> 40) as u32 % 8), // just below a page edge
                _ => edges[(x >> 32) as usize % edges.len()].wrapping_add((x >> 48) as u32 % 16),
            };
            let n = [1usize, 2, 4, 8, 16, 32, 100][(x >> 20) as usize % 7];
            let a = if a.checked_add(n as u32 - 1).is_none() { a - n as u32 } else { a }; // wraps: see below
            let at = |k: usize| Memory::translate(a.wrapping_add(k as u32));
            if x & (1 << 60) != 0 {
                let data: Vec<u8> = (0..n).map(|_| rnd() as u8).collect();
                match n {
                    1 => m.write8(a, data[0]),
                    4 => m.write32(a, u32::from_be_bytes(data[..4].try_into().unwrap())),
                    8 => m.write64(a, u64::from_be_bytes(data[..8].try_into().unwrap())),
                    16 => m.write_n::<16>(a, &data[..].try_into().unwrap()),
                    _ => m.write(a, &data),
                }
                // later bytes win where an access wraps onto itself
                data.iter().enumerate().for_each(|(k, &v)| { model.insert(at(k), v); });
            } else {
                let want: Vec<u8> = (0..n).map(|k| model.get(&at(k)).copied().unwrap_or(0)).collect();
                let got = match n {
                    1 => vec![m.read8(a)],
                    2 => m.read16(a).to_be_bytes().to_vec(),
                    4 => m.read32(a).to_be_bytes().to_vec(),
                    8 => m.read64(a).to_be_bytes().to_vec(),
                    16 => m.read_n::<16>(a).to_vec(),
                    _ => { let mut b = vec![0; n]; m.read(a, &mut b); b }
                };
                assert_eq!(got, want, "read {n} at {a:#x}");
                let mut b = vec![0; n];
                r.read(a, &mut b);
                assert_eq!(b, want, "reader {n} at {a:#x}");
            }
        }
        // accesses wrapping past 0xFFFFFFFF: no panic, inside guest memory, and they read back
        // what they wrote through every path
        for a in 0xffff_fff0u32..=0xffff_ffff {
            m.write_n::<16>(a, &[0x5a; 16]);
            assert_eq!(m.read_n::<16>(a), [0x5a; 16]);
            m.write64(a, 0x0102_0304_0506_0708);
            assert_eq!(m.read64(a), 0x0102_0304_0506_0708);
            m.write32(a, 0xdead_beef);
            assert_eq!(m.read32(a), 0xdead_beef);
            let mut b = [0; 100];
            m.read(a, &mut b);
            r.read(a, &mut b);
        }
    }
}

#[cfg(all(test, any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
mod watch_tests {
    use super::*;

    /// The write watch (#6): a queried page is reported written exactly once after each write,
    /// through the direct offset, the alias and the reader's host writes alike.
    #[test]
    fn write_watch_reports_each_write_once() {
        let mut m = Memory::new();
        let a = 0xA010_0000u32; // physical, with an alias at 0xC0100000
        m.write32(a, 1);
        assert_eq!(m.write_watch(a, 0x4000).unwrap(), Vec::<u32>::new(), "first query: nothing known yet; it protects");
        assert_eq!(m.write_watch(a, 0x4000).unwrap(), Vec::<u32>::new(), "nothing written since the last query");
        m.write32(a + 8, 2); // faults, is retried
        assert_eq!(m.read32(a + 8), 2);
        assert_eq!(m.write_watch(a, 0x4000).unwrap(), vec![0xA010_0000, 0xA010_1000, 0xA010_2000, 0xA010_3000], "the written host page's 4 KB pages");
        assert_eq!(m.write_watch(a, 0x4000).unwrap(), Vec::<u32>::new());
        m.write32(a + 0x2000_0000, 3); // through the alias
        assert_eq!(m.read32(a), 3);
        assert_eq!(m.write_watch(a, 0x1000).unwrap(), vec![0xA010_0000]);
        m.write_watch(a, 0x8000).unwrap(); // a page is watched from its first query on
        assert!(m.reader().write(a + 0x100, &[7; 0x5000]), "a bulk host write unprotects first");
        assert_eq!(m.read32(a + 0x100), 0x0707_0707);
        assert_eq!(m.write_watch(a, 0x8000).unwrap(), (0xA010_0000..0xA010_6000).step_by(0x1000).collect::<Vec<u32>>(), "exactly the 4 KB pages the bulk write covered");
        // two resolves landing in one 16 KB host page (bands 4 KB apart) don't report each other
        m.write_watch(a + 0x10000, 0x8000).unwrap();
        assert!(m.reader().write(a + 0x10000, &[1; 0x5000]));
        assert!(m.reader().write(a + 0x15000, &[2; 0x3000]));
        assert_eq!(m.write_watch(a + 0x10000, 0x5000).unwrap(), vec![0xA011_0000, 0xA011_1000, 0xA011_2000, 0xA011_3000, 0xA011_4000]);
        assert_eq!(m.write_watch(a + 0x15000, 0x3000).unwrap(), vec![0xA011_5000, 0xA011_6000, 0xA011_7000]);
        assert_eq!(m.write_watch(a + 0x10000, 0x8000).unwrap(), Vec::<u32>::new());
        assert_eq!(m.write_watch(a + 0x8000, 0x1000).unwrap(), Vec::<u32>::new(), "never written, first query");
        // a page nobody queried is never protected: writes there don't fault
        for i in 0..64 {
            m.write32(0x4000_0000 + i * 4, i);
        }
        // reads of a protected page are free
        assert_eq!(m.read32(a + 8), 2);
    }

    /// A state reset drops every protection and reports the pages written.
    #[test]
    fn reset_counts_protected_pages_as_written() {
        let mut m = Memory::new();
        m.write32(0x4000_0000, 1);
        m.write_watch(0x4000_0000, 0x1000).unwrap();
        assert_eq!(m.write_watch(0x4000_0000, 0x1000).unwrap(), Vec::<u32>::new());
        m.restore_pages(Vec::new());
        assert_eq!(m.read32(0x4000_0000), 0);
        assert_eq!(m.write_watch(0x4000_0000, 0x1000).unwrap(), vec![0x4000_0000]);
        m.write32(0x4000_0000, 5); // must not fault into a stale protection
        assert_eq!(m.write_watch(0x4000_0000, 0x1000).unwrap(), vec![0x4000_0000]);
    }

    /// Writes from another thread while the render thread queries: every write is reported by
    /// this query or the next, and the writer is never stuck.
    #[test]
    fn writes_from_other_threads_are_seen() {
        let mut m = Memory::new();
        let a = 0xA020_0000u32;
        m.write_watch(a, 0x4000).unwrap();
        let mut w = m.share();
        let t = std::thread::spawn(move || {
            for i in 0..2000u32 {
                w.write32(a + (i % 0x1000) * 4, i);
            }
        });
        let mut reported = 0;
        for _ in 0..200 {
            reported += m.write_watch(a, 0x4000).unwrap().len();
        }
        t.join().unwrap();
        reported += m.write_watch(a, 0x4000).unwrap().len();
        assert!(reported > 0, "the writer's faults were reported");
        assert_eq!(m.write_watch(a, 0x4000).unwrap(), Vec::<u32>::new());
        assert_eq!(m.read32(a + 1999 % 0x1000 * 4), 1999);
    }
}
