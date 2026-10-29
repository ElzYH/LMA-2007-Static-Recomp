//! Native replacements for guest functions, by address (#6: the high-level D3D experiment).
//!
//! A hooked address is left out of the recompiled block table, like an import thunk, so calls to
//! it come back to the dispatcher, which runs the hook instead and continues at LR. A hook that
//! returns false declines: the guest function then runs as usual (from the interpreter for its
//! first instruction, recompiled code after that). The D3D hooks are only installed with
//! LMA_HLE_D3D=1, the badge hook only by a mod with a `badges/` folder (`mods::ModSpace::install`),
//! so normal and deterministic runs never see them.

use crate::machine::Machine;
use std::collections::HashMap;

/// Runs instead of the guest function at its address (arguments in r3..r10); true when handled.
pub type Hook = fn(&mut Machine) -> bool;

/// D3D runtime entry points (map and signatures on #6).
pub const D3D_DRAW_VERTICES: u32 = 0x8233_98A0;
pub const D3D_DRAW_INDEXED_VERTICES: u32 = 0x8233_9CA8;
pub const D3D_DRAW_VERTICES_UP: u32 = 0x8233_93D0;
pub const D3D_RESOLVE: u32 = 0x8234_02F8;
pub const D3D_SWAP: u32 = 0x8233_6958;

/// The game's club badge setter, `set_badge(club uid, Badge.mng entry)` (r3, r4). At database load
/// the game gives every club its country's crest (entry 200 + country) and then the 72 clubs of a
/// hard-coded table (0x82712ad8, Championship to League Two 2006/07) the retail badges 251..322;
/// the entry is a 9-bit field of the club record (+0x58 bits 12..20), so entries stay below 512.
pub const BADGE_SET: u32 = 0x820E_39D8;
/// The badge getter, `badge(club uid) -> Badge.mng entry` (r3; -1 for no club), the only reader of
/// the 9-bit field (the five callers load the texture or copy the entry into the club-build data
/// that the setter reads back). Hooked, a mod club's entry comes from `Machine::badge_entries`
/// instead of the field, so a mod may use archive entries above 511.
pub const BADGE_GET: u32 = 0x820E_3970;
/// The club bank balance adder, `add_funds(club uid, amount)` (r3, r4): every credit and debit
/// goes through it (balance capped at 999,999,999). The statement line is a separate call
/// (0x82104cd8: club, category, 0 credit / 1 debit, amount; categories 0 gate receipts, 1 wages,
/// 2 transfer fees, 3 building costs, 4 sponsorship (0x8212ba94: "Chosen sponsor %s"),
/// 5 miscellaneous).
pub const ADD_FUNDS: u32 = 0x820E_4018;
/// Return addresses of the `add_funds` calls that pay a club income from outside the game's
/// transfer market: gate receipts (both halves of a gate, 0x822268b4), sponsorship (0x8212ba94)
/// and the two miscellaneous credits (prize money, 0x82162610 and 0x820ec798). Transfer fees,
/// wages, costs and interest (a share of the balance) are left alone.
pub const INCOME_CALLERS: [u32; 5] = [0x8222_6B1C, 0x8222_6B58, 0x8212_C624, 0x8216_2B98, 0x820E_CA28];
/// The game's player valuation, `value(player index) -> GBP` (r3): a base price from the player's
/// rating (the 22-word ladder at 0x82005508, interpolated; capped at GBP 42m at 0x82120a34) and
/// age, position and country factors, times the stored u16 at record+0x14 as a percentage when it
/// isn't 0, rounded (0x82118d58) and clamped to GBP 10,000..40m (`guest::rec::value_cap`).
pub const VALUATION: u32 = 0x8212_08E0;
/// The stored value's setter, `set(player index, u16)` (r3, r4).
pub const VALUE_SET: u32 = 0x8211_DCB0;
/// Two loops turn the stored value into that percentage: they clear it, price the player with the
/// base alone and store stored / base x 100: the new-game one (0x8220df78, a few squads) and the
/// one over every club's squad (0x8215da48, the percentage capped at 300, so a player was worth at
/// most 3x the game's own price). Return addresses of their setter calls (clear, store, clear,
/// store); with `values_as_stored` all four are skipped.
pub const PREMIUM_SETS: [u32; 4] = [0x8220_E21C, 0x8220_E27C, 0x8215_DAB4, 0x8215_DB24];
/// The game's weekly wage, `wage(player index) -> GBP` (r3): cached in the player's runtime record,
/// word +0x38 bits 0..18 (`>> 13`, so at most 524,287); computed from his rating and value when 0.
/// The same record's word +0x34 holds his contract in its low 3 bits: the seasons left, 1 = ends
/// this season ("11 months"), 2 = "2 years" (22 Arsenal players on the contracts screen, 11 Oct).
pub const WAGE: u32 = 0x8212_3DC8;
/// The contract length setter, `set(player index, seasons)` (r3, r4; clamped to 0..5 into +0x34's
/// low 3 bits). The career start draws every player's contract through it after the wage cache is
/// filled, so the real length goes in here.
pub const CONTRACT_SET: u32 = 0x8211_C658;
/// The D3D fence-wait watchdog, `watchdog(wait state*) -> still waiting` (r3; #312, Ghidra 11 Oct):
/// `wait_for_fence(device, fence)` at 0x82335380 loops `while watchdog(&s) && *(ring + 0x3c) < fence`
/// with no sleep, and the watchdog only times the wait out after 5000 ticks; `s[0]` is the device
/// block, `device + 0x2a10` its ring, `ring + 0x3c` the GPU's read pointer. On the 360 that spin
/// lasts microseconds; here it burns a core at the title screen (100% CPU, menus too). With
/// LMA_FENCE_NAP=1 the hook lets the watchdog run but, when the read pointer has not moved since
/// `FENCE_NAP_AFTER` polls of the same thread, puts the thread to sleep for `FENCE_NAP_TICKS` on
/// every other poll.
pub const FENCE_WATCHDOG: u32 = 0x8233_9080;
/// The nap between two polls of an unmoved fence: 1 ms of the 50 MHz timebase. The GPU moving the
/// fence ends it early (wait mode); quiet inbox, game thread: 26-29% of a core at 0.25 ms, 21-25%
/// at 1 ms, night match fps unchanged (#312).
pub const FENCE_NAP_TICKS: u64 = 50_000;
/// Unmoved polls before the first nap. A poll is ~115 guest instructions (~50 ns here), so 2,000
/// of them are ~0.1 ms of spinning: the fences of a running match land within that and never nap
/// (one A/B round of the first version, napping from the second poll, cut the day match's 1% lows
/// 57 -> 33 fps), while the title screen's and the menus' waits last a frame or more and still
/// sleep through nearly all of it.
pub const FENCE_NAP_AFTER: u32 = 2_000;
/// LMA_FENCE_NAP=wait (real time only; deterministic runs spin as without the hook, so their
/// instruction counts and fingerprints stay): the thread parks on the second unmoved poll and every
/// other one after (`Wait::GpuMoved`), woken by the fence or read pointer moving, so a fence that
/// lands at the next command-processor run costs no oversleep. The polls in between let the
/// caller's own exit test run.
pub const FENCE_WAIT_AFTER: u32 = 2;

/// The fence hook's mode from LMA_FENCE_NAP: `1` the timed nap, `2` observe only, `wait` the native
/// wait (3), `0` no hook. Unset is the native wait in real time (mac-perf's day-match rounds on #810:
/// ~59 fps with 20-35% less CPU, the best 1% lows) and no hook in deterministic runs, so their
/// instruction counts and fingerprints stay as they were.
pub fn fence_nap_mode(env: Option<&str>, deterministic: bool) -> Option<u8> {
    match env {
        Some("0") => None,
        Some("1") => Some(1),
        Some("wait") => Some(3),
        Some(_) => Some(2),
        None if deterministic => None,
        None => Some(3),
    }
}
/// The players' runtime records (0x12c bytes each; the database record first, stored value at +0x14).
const PLAYER_TABLE: u32 = 0x8274_06C8;
/// The club's starting funds, `funds(club uid) -> GBP` (r3): tier word x 10 x the club's group
/// multiplier (0x820033ac), Chelsea x200, Celtic and Rangers x80; the career start stores it capped at
/// 999,999,999 (0x820eb8c8) and team select shows it.
pub const FUNDS: u32 = 0x820E_AA10;
/// `[club uid] -> club record index` (u16 each), the table the setter itself looks the club up in.
const CLUB_UID_MAP: u32 = 0x8273_D6C8;

/// Every address any hook may take: the one list shared with the recompiler, which keeps V2
/// direct calls off them (`guest::hooks`, #165). New hooks are added there.
pub use guest::hooks::ADDRESSES;

/// The hooks to install for this run.
pub fn installed() -> Vec<(u32, Hook)> {
    let mut out: Vec<(u32, Hook)> = Vec::new();
    let deterministic = std::env::var("LMA_DETERMINISTIC").is_ok_and(|v| v != "0");
    if fence_nap_mode(std::env::var("LMA_FENCE_NAP").ok().as_deref(), deterministic).is_some() {
        out.push((FENCE_WATCHDOG, fence_watchdog as Hook));
    }
    // `count` only counts the calls (the game's function still runs): the work-based A/B's baseline;
    // `hook` answers them as hooks (each call unwinds to the dispatcher); `1` puts the native
    // functions in the block table instead (`native_blocks`), so they are called like recompiled code
    match std::env::var("LMA_NATIVE_OVERALL").as_deref() {
        Ok("hook") => out.extend([(PLAYER_OVERALL, player_overall as Hook), (PLAYER_ROLE, player_role_hook as Hook)]),
        Ok("count") => out.extend([(PLAYER_OVERALL, observe as Hook), (PLAYER_ROLE, observe as Hook)]),
        _ => {}
    }
    if std::env::var("LMA_HLE_D3D").as_deref() != Ok("1") {
        return out;
    }
    debug_assert!([D3D_DRAW_VERTICES, D3D_DRAW_INDEXED_VERTICES, D3D_DRAW_VERTICES_UP, D3D_RESOLVE, D3D_SWAP].iter().all(|a| ADDRESSES.contains(a)));
    // first stage: observe only (count calls per frame), every function still runs as before
    out.extend([D3D_DRAW_VERTICES, D3D_DRAW_INDEXED_VERTICES, D3D_DRAW_VERTICES_UP, D3D_RESOLVE, D3D_SWAP].iter().map(|&a| (a, observe as Hook)));
    out
}

/// Whether a fence poll should nap: the read pointer `rd` against the thread's previous poll
/// `(last_rd, repeats)`; returns the new state and the decision. The first `FENCE_NAP_AFTER`
/// unmoved polls run on (a match's fences land within them); from then the thread sleeps on every
/// other poll.
/// Every other, not every: a napping thread resumes at the hooked function and polls again at
/// once, so a nap on each unmoved poll would never let the watchdog or its caller run. The
/// caller's own exit test (the fence number, not the read pointer) must get its turn: at the
/// title screen the GPU's queue runs dry with the fence long complete, and the read pointer never
/// moves again (the live stall of the first test build, 11 Oct).
pub fn fence_nap(last: (u32, u32), rd: u32) -> ((u32, u32), bool) {
    fence_nap_after(last, rd, FENCE_NAP_AFTER)
}

/// `fence_nap` with the threshold `after` (LMA_FENCE_NAP_AFTER).
pub fn fence_nap_after(last: (u32, u32), rd: u32, after: u32) -> ((u32, u32), bool) {
    if rd != last.0 {
        return ((rd, 0), false);
    }
    let repeats = last.1.saturating_add(1);
    ((rd, repeats), repeats >= after && repeats % 2 == 0)
}

/// The nap's threshold and length: `FENCE_NAP_AFTER` / `FENCE_NAP_TICKS`, or LMA_FENCE_NAP_AFTER /
/// LMA_FENCE_NAP_TICKS (#6: the match's ring-space wait `0x82335fc8` spins in the watchdog's caller
/// until the command processor, which runs between guest slices, frees ring space; a few polls and
/// a ~zero nap end the slice at once instead of spinning it out).
fn fence_nap_config() -> (u32, u64) {
    static CONFIG: std::sync::OnceLock<(u32, u64)> = std::sync::OnceLock::new();
    *CONFIG.get_or_init(|| {
        let after = std::env::var("LMA_FENCE_NAP_AFTER").ok().and_then(|v| v.parse().ok()).unwrap_or(FENCE_NAP_AFTER);
        let ticks = std::env::var("LMA_FENCE_NAP_TICKS").ok().and_then(|v| v.parse().ok()).unwrap_or(FENCE_NAP_TICKS);
        (after, ticks)
    })
}

/// `FENCE_WATCHDOG`: nap while the GPU read pointer stands still (see the constant); the game's
/// watchdog then runs as usual, so its 5000-tick timeout and bookkeeping are untouched.
pub fn fence_watchdog(m: &mut Machine) -> bool {
    let s = m.cpu.gpr[3] as u32;
    let device = m.mem.read32(s);
    let ring = if device != 0 { m.mem.read32(device + 0x2a10) } else { 0 };
    if ring == 0 {
        return false;
    }
    // No command-processor pump here: `service_gpu` delivers GPU interrupts by switching threads,
    // which is only sound at a slice boundary; called from a hook (mid-slice) it stalled the
    // title's first fence for good. The pump runs when this thread's slice ends or it naps.
    let rd = m.mem.read32(ring + 0x3c);
    let cur = m.cur;
    let last = m.fence_polls.get(&cur).copied().unwrap_or((rd.wrapping_add(1), 0));
    // LMA_FENCE_NAP=2 (debug): the hook observes and logs but never naps
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    let mode = *MODE.get_or_init(|| fence_nap_mode(std::env::var("LMA_FENCE_NAP").ok().as_deref(), false).unwrap_or(2));
    let wait = mode == 3 && !m.deterministic;
    let (after, ticks) = fence_nap_config();
    let (next, nap) = fence_nap_after(last, rd, if wait { FENCE_WAIT_AFTER } else { after });
    m.fence_polls.insert(cur, next);
    if nap && (mode == 1 || wait) {
        m.nap_until = Some(m.now() + ticks);
        if wait {
            // the native wait: parked until the GPU writes its fence or moves its read pointer
            // (the command processor runs as soon as this slice ends), the nap only a bound
            m.nap_watch = Some(([ring, ring + 0x3c], [m.mem.read32(ring), rd]));
        }
    }
    // LMA_FENCE_NAP_LOG=1 (debug): every poll's decision
    static LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *LOG.get_or_init(|| std::env::var_os("LMA_FENCE_NAP_LOG").is_some()) && (next.1 == 0 || next.1 % 5000 == 0) {
        let mmio_w = m.mem.read32(crate::gpu::GPU_MMIO_BASE + crate::gpu::CP_RB_WPTR as u32 * 4);
        eprintln!("fence t{cur} clock={} rd={rd:x} wptr={mmio_w:x} fence_done={:x} fence_next={:x} repeats={} nap={nap} lr={:x} {}", m.clock, m.mem.read32(ring), m.mem.read32(device + 0x2a1c), next.1, m.cpu.lr, m.gpu.fence_debug());
    }
    false
}

/// Badge setter (`BADGE_SET`): a club with a badge in the mod gets that entry instead of what the
/// game chose (`Machine::badge_entries`, by club record index); then the game's setter runs.
pub fn badge_set(m: &mut Machine) -> bool {
    let uid = m.cpu.gpr[3] as u32;
    let map = m.mem.read32(CLUB_UID_MAP);
    if uid < 0x2056 && map != 0 {
        let rec = m.mem.read16(map + uid * 2);
        if let Some(&entry) = m.badge_entries.get(&rec) {
            m.cpu.gpr[4] = entry as u64;
        }
    }
    false
}

/// Badge getter (`BADGE_GET`): a club with a badge in the mod answers with that entry (the record's
/// 9-bit field can't hold entries above 511); any other club goes to the game's getter.
pub fn badge_get(m: &mut Machine) -> bool {
    let uid = m.cpu.gpr[3] as u32;
    let map = m.mem.read32(CLUB_UID_MAP);
    if uid < 0x2056 && map != 0 {
        let rec = m.mem.read16(map + uid * 2);
        if let Some(&entry) = m.badge_entries.get(&rec) {
            m.cpu.gpr[3] = entry as u64;
            // answered lookups, for lma-cli run's hook report
            *m.hook_calls.entry(BADGE_GET).or_default() += 1;
            return true;
        }
    }
    false
}

/// Club income (`ADD_FUNDS`) from an `INCOME_CALLERS` site times `Machine::income_scale` (the maintainer,
/// 11 Oct: prize money, gate receipts and the other earnings on the modern scale of the funds);
/// then the game's adder runs with the scaled amount.
pub fn add_funds(m: &mut Machine) -> bool {
    let amount = m.cpu.gpr[4] as u32 as i32;
    let lr = m.cpu.lr as u32;
    if amount > 0 && INCOME_CALLERS.contains(&lr) && !real_sponsorship(m, SPONSORSHIP_CREDIT) {
        m.cpu.gpr[4] = scaled_income(amount, income_factor(m, GATE_CALLERS.contains(&lr))) as u32 as u64;
    }
    false
}

/// The bank statement line, `record(club, category, 0 credit / 1 debit, amount)` (r3..r6). The
/// balance the game shows and plans with follows these lines (PC check, 11 Oct: with only
/// `ADD_FUNDS` scaled, Ipswich's balance word still equalled the opening balance plus the unscaled
/// lines), so the same income is scaled here too.
pub const STATEMENT: u32 = 0x8210_4CD8;
/// Return addresses of the statement calls for the `INCOME_CALLERS` incomes: the gate (four lines,
/// 0x822268b4), sponsorship (0x8212ba94) and the two miscellaneous credits (0x82162610, 0x820ec798).
pub const INCOME_STATEMENTS: [u32; 7] = [0x8222_6A64, 0x8222_6AC0, 0x8222_6B10, 0x8222_6B44, 0x8212_C614, 0x8216_2BAC, 0x820E_CA1C];

/// The gate's own sites among `INCOME_CALLERS` (both halves of a gate) and `INCOME_STATEMENTS` (its
/// four lines): `Machine::gate_scale` applies there when set.
pub const GATE_CALLERS: [u32; 2] = [0x8222_6B1C, 0x8222_6B58];
pub const GATE_STATEMENTS: [u32; 4] = [0x8222_6A64, 0x8222_6AC0, 0x8222_6B10, 0x8222_6B44];

/// The factor for an income site: the gate's own (`gate_scale`) at a gate site when the mod sets one,
/// else `income_scale`.
fn income_factor(m: &Machine, gate: bool) -> f64 {
    match (gate, m.gate_scale) {
        (true, Some(g)) => g,
        _ => m.income_scale,
    }
}

/// A statement credit (`STATEMENT`) from an `INCOME_STATEMENTS` site times `Machine::income_scale`.
pub fn statement(m: &mut Machine) -> bool {
    let amount = m.cpu.gpr[6] as u32 as i32;
    let lr = m.cpu.lr as u32;
    if m.cpu.gpr[5] as u32 == 0 && amount > 0 && INCOME_STATEMENTS.contains(&lr) && !real_sponsorship(m, SPONSORSHIP_STATEMENT) {
        m.cpu.gpr[6] = scaled_income(amount, income_factor(m, GATE_STATEMENTS.contains(&lr))) as u32 as u64;
    }
    // LMA_STATEMENT_LOG=<file> (analysis): every statement line as it is recorded, "club uid,
    // category, 0 credit / 1 debit, GBP, caller", to compare a season's income and bills by category
    // with a modern club's accounts
    static LOG: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    if let Some(f) = LOG.get_or_init(|| std::env::var_os("LMA_STATEMENT_LOG").and_then(|p| std::fs::File::create(p).ok()).map(std::sync::Mutex::new)) {
        use std::io::Write;
        let g = &m.cpu.gpr;
        let _ = writeln!(f.lock().unwrap(), "{},{},{},{},{:#x}", g[3] as u32, g[4] as u32, g[5] as u32, g[6] as u32 as i32, m.cpu.lr as u32);
    }
    false
}

/// A club's weekly wage budget setter, `set(club uid, GBP a week)` (the club record's +0xb0), called
/// by the weekly recalculation (0x8210b8e4, after its clamp to 4,800..the wage budget cap). The
/// contract offer helper (0x821f6f00) lets a club offer up to budget - squad bill + the player's
/// current wage, so with real wages the computed budget left ~GBP 100k of room (the maintainer, 11 Oct).
pub const WAGE_BUDGET_SET: u32 = 0x820E_6BE0;

/// The wage budget's getter, `get(club uid) -> GBP a week` (the club record's +0xb0): the offer
/// helper's room (0x8210b900: budget - squad bill) reads it. The setter only runs in the weekly
/// recalculation, so a new career kept the database's 2006 budget until the first week ticked and the
/// board still refused offers over ~GBP 100k (the maintainer, 12 Oct: Arsenal and Man United).
pub const WAGE_BUDGET_GET: u32 = 0x820E_6B80;

/// The club table: `[CLUB_TABLE] + record * CLUB_STRIDE` is a club's runtime record.
const CLUB_TABLE: u32 = 0x8273_D6C4;
const CLUB_STRIDE: u32 = 0x3d8;

/// `WAGE_BUDGET_GET` with a mod's wage_budgets.csv: the stored budget, at least the club's floor, from
/// the first day of a career; a club the file doesn't list goes to the game's getter.
pub fn wage_budget_get(m: &mut Machine) -> bool {
    let uid = m.cpu.gpr[3] as u32;
    let map = m.mem.read32(CLUB_UID_MAP);
    let table = m.mem.read32(CLUB_TABLE);
    if uid >= 0x2056 || map == 0 || table == 0 {
        return false;
    }
    let rec = m.mem.read16(map + uid * 2);
    let Some(&floor) = m.wage_budgets.get(&rec) else { return false };
    if rec >= 0x708 {
        return false;
    }
    let stored = m.mem.read32(table + rec as u32 * CLUB_STRIDE + 0xb0);
    m.cpu.gpr[3] = stored.max(floor) as u64;
    true
}

/// `WAGE_BUDGET_SET` with a mod's wage_budgets.csv: the budget is at least the club's figure (its real
/// squad bill plus headroom), more when the game computes more.
pub fn wage_budget_set(m: &mut Machine) -> bool {
    let uid = m.cpu.gpr[3] as u32;
    let map = m.mem.read32(CLUB_UID_MAP);
    if uid >= 0x2056 || map == 0 {
        return false;
    }
    let rec = m.mem.read16(map + uid * 2);
    if let Some(&floor) = m.wage_budgets.get(&rec) {
        if (m.cpu.gpr[4] as u32) < floor {
            m.cpu.gpr[4] = floor as u64;
        }
    }
    false
}

/// The weekly sponsorship credit (shirt and hoardings in one): its `ADD_FUNDS` and `STATEMENT`
/// return addresses.
const SPONSORSHIP_CREDIT: u32 = 0x8212_C624;
const SPONSORSHIP_STATEMENT: u32 = 0x8212_C614;

/// With sponsors.csv the shirt deal is already real money, so the weekly sponsorship credit is not
/// scaled again (the hoardings get `income_scale` when they're set instead, `sponsor_hoarding_set`).
fn real_sponsorship(m: &Machine, site: u32) -> bool {
    m.cpu.lr as u32 == site && !m.sponsors.is_empty()
}

/// `amount` times `scale`, rounded, kept within the balance cap (999,999,999).
pub fn scaled_income(amount: i32, scale: f64) -> i32 {
    (amount as f64 * scale).round().clamp(0.0, 999_999_999.0) as i32
}

/// Player valuation (`VALUATION`) with `Machine::values_as_stored`: a player with a stored value is
/// worth exactly that (u16 x `guest::rec::value_unit()`, within the game's 10,000..cap), so the
/// imported Transfermarkt values are what the game shows, asks and pays (the maintainer, 11 Oct); a player
/// without one (a regen, a youth player) is priced by the game as before.
pub fn valuation(m: &mut Machine) -> bool {
    if !m.values_as_stored {
        return false;
    }
    match stored_valuation(&mut m.mem, m.cpu.gpr[3] as u32 as i32, &m.value_base_levels) {
        Some(v) => {
            m.cpu.gpr[3] = v;
            true
        }
        None => false,
    }
}

/// `valuation`'s answer for player `idx`, or None for a player the game prices itself (no stored
/// value, or no player table yet).
pub fn stored_valuation(mem: &mut guest::mem::Memory, idx: i32, base_levels: &[f32]) -> Option<u64> {
    let table = mem.read32(PLAYER_TABLE);
    if !(0..0x8000).contains(&idx) || table == 0 {
        return None;
    }
    let rec = table + idx as u32 * 0x12c;
    let raw = mem.read16(rec + 0x14) as i64;
    if raw == 0 {
        return None;
    }
    // the value moves with the player: his ratings now against the mod's database (training, form
    // dips, age decline and growth all go through the ratings; the maintainer, 11 Oct)
    let base = base_levels.get(idx as usize).copied().unwrap_or(0.0);
    let mut bytes = [0u8; 40];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = mem.read8(rec + i as u32);
    }
    let level = rating_level(&bytes);
    let ratings = if base > 0.0 { form_factor(level, base) } else { 1.0 };
    // match form: the season's average match rating and appearances (the Mac's Ghidra read, #31)
    let season = (mem.read32(rec + 0x28) >> 12) & 0x7f;
    let apps = (mem.read32(rec + 0x30) >> 2) & 0x7f;
    let factor = (ratings * match_form(season, apps, level)).clamp(0.25, 4.0);
    // the stored unit (GBP 5,000) quantises cheap players (668 at exactly GBP 80k, 11 Oct): a fixed
    // per-player offset within the unit's rounding step makes them distinct again
    let unit = guest::rec::value_unit();
    let stored = stored_value(raw, unit, guest::rec::value_cap()) as f64 + dequantise(idx as u32, unit);
    let v = (stored.max(10_000.0) * factor).round() as i64;
    Some(v.clamp(10_000, guest::rec::value_cap()) as u64)
}

/// The 13 rating fields the game's overall weighs (`f8211f4c8`), found once.
fn overall_fields() -> &'static [lma_data::db::Field] {
    static FIELDS: std::sync::OnceLock<Vec<lma_data::db::Field>> = std::sync::OnceLock::new();
    FIELDS.get_or_init(|| {
        const OVERALL: [&str; 13] = ["tackling", "marking", "passing", "technique", "shot_power", "shot_accuracy", "heading", "pace", "creativity", "runs", "dribbling", "positioning", "intelligence"];
        lma_data::db::RATINGS.iter().filter(|f| OVERALL.contains(&f.name)).copied().collect()
    })
}

/// A player record's level (the 40-byte database layout, which the runtime record starts with):
/// the mean of the 13 ratings the game's overall weighs (`f8211f4c8`).
pub fn rating_level(rec: &[u8]) -> f32 {
    let fields = overall_fields();
    let sum: u32 = fields.iter().map(|f| lma_data::bits::get(rec, f.pos, f.width) + f.add).sum();
    sum as f32 / fields.len().max(1) as f32
}

/// The same level from the mod's database (the baseline).
pub fn database_level(db: &lma_data::Database, pid: usize) -> f32 {
    const OVERALL: [&str; 13] = ["tackling", "marking", "passing", "technique", "shot_power", "shot_accuracy", "heading", "pace", "creativity", "runs", "dribbling", "positioning", "intelligence"];
    let fields = lma_data::db::RATINGS.iter().filter(|f| OVERALL.contains(&f.name));
    let (sum, n) = fields.fold((0u32, 0u32), |(s, n), f| (s + db.field(pid, *f), n + 1));
    sum as f32 / n.max(1) as f32
}

/// The value's factor for match form: 3% per point of season rating above what players of his
/// level average (62.7 + 0.072 x level: 13,000 players after pre-season, PC 11 Oct; the rating is
/// mostly relative to the opposition), in full from 10 appearances.
pub fn match_form(season: u32, apps: u32, level: f32) -> f64 {
    if season == 0 || apps == 0 {
        return 1.0;
    }
    let expected = 62.7 + 0.072 * level as f64;
    1.03f64.powf((season as f64 - expected) * apps.min(10) as f64 / 10.0)
}

/// The value's factor for a level change: 12% a rating point (a 5-point rise doubles it nearly,
/// a 5-point fall about halves it), within 0.25..4.
pub fn form_factor(now: f32, base: f32) -> f64 {
    1.12f64.powf((now - base) as f64).clamp(0.25, 4.0)
}

/// A player's fixed offset within half a stored unit either side (a hash of his index), in GBP 100s.
pub fn dequantise(idx: u32, unit: i64) -> f64 {
    let h = idx.wrapping_mul(2_654_435_761) >> 16;
    let half = unit as f64 / 2.0;
    let off = (h % 1000) as f64 / 999.0 * 2.0 * half - half;
    ((off / 100.0).trunc() * 100.0).clamp(-half, half)
}

/// A stored u16 in GBP: `raw` x `unit`, within the game's GBP 10,000 floor and `cap`.
pub fn stored_value(raw: i64, unit: i64, cap: i64) -> u32 {
    (raw * unit).clamp(10_000, cap) as u32
}

/// The stored value's setter (`VALUE_SET`) with `Machine::values_as_stored`: the conversion loops'
/// calls (`PREMIUM_SETS`) are skipped, so the stored value stays a value; every other call runs.
pub fn value_set(m: &mut Machine) -> bool {
    m.values_as_stored && PREMIUM_SETS.contains(&(m.cpu.lr as u32))
}

/// Weekly wage (`WAGE`) for a player in the mod's wages.csv (`Machine::wages`), the first time the
/// game asks in a career: his real wage into the cache, so every reader agrees (the maintainer, 11 Oct), then
/// `WAGE_APPLIED` set. His contract goes in through `contract_set`. The career start fills the cache with the
/// game's own wage before anyone asks (PC, 11 Oct), so "cache empty" can't tell; the mark lives in
/// the record, survives saves, and leaves a contract renewed in the game alone.
pub fn wage(m: &mut Machine) -> bool {
    let Some(w) = mod_wage(&mut m.mem, m.cpu.gpr[3] as u32 as i32, &m.wages) else { return false };
    m.cpu.gpr[3] = w as u64;
    true
}

/// `wage`'s work for player `idx`: his wages.csv wage written into the record and returned, or None
/// when the game's getter answers (no row, already applied, out of range).
fn mod_wage(mem: &mut guest::mem::Memory, idx: i32, wages: &HashMap<u32, (u32, Option<u32>)>) -> Option<u32> {
    let &(w, _) = (idx >= 0).then(|| wages.get(&(idx as u32))).flatten()?;
    let table = mem.read32(PLAYER_TABLE);
    if table == 0 || idx >= 0x8000 {
        return None;
    }
    let rec = table + idx as u32 * 0x12c;
    let mut word = mem.read32(rec + 0x38);
    // builds 24-25 marked records with bits 5 and 6 of this word, which count the player's goals
    // this season (the Mac's pre/post-match diff, 11 Oct): take them out again (no one scores 96)
    if word & OLD_MARKS == OLD_MARKS {
        word &= !OLD_MARKS;
        mem.write32(rec + 0x38, word);
        let mark = mem.read32(rec + MARKS);
        mem.write32(rec + MARKS, mark | WAGE_APPLIED | CONTRACT_APPLIED);
    }
    let mark = mem.read32(rec + MARKS);
    if mark & WAGE_APPLIED != 0 {
        return None;
    }
    mem.write32(rec + MARKS, mark | WAGE_APPLIED);
    let w = w.min(WAGE_MAX);
    mem.write32(rec + 0x38, (w << 13) | (word & 0x1fff));
    Some(w)
}

/// The game's wage rounding, `round(wage) -> wage` (0x82118e88): to steps of 10 below GBP 2,000 up to
/// 10,000 below 250,000 (table at 0x8200560c: step, threshold pairs; 100,000 above), then clamped to
/// 150..250,000. Every wage offer and demand goes through it.
pub const WAGE_ROUND: u32 = 0x8211_8E88;
/// The AI's wage offer for a player it buys, `offer(player, club) -> wage` (0x82187738): his
/// expected wage from his rating (0x82120d38, the 2006 scale), rounded and capped at 250,000;
/// 0x82189898 then offers 90-114% of it.
pub const AI_OFFER: u32 = 0x8218_7738;

/// Mod `ai_wage_offers`: `WAGE_ROUND` clamps to `WAGE_MAX` instead of 250,000 and `AI_OFFER` is at
/// least the player's current wage + 10% (native blocks, `Machine::install_blocks`).
pub static AI_WAGE_OFFERS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `WAGE_ROUND`'s answer with the upper clamp `cap` (the game's is 250,000). Above the table the
/// game rounds to 100,000, which its clamp hides; with a higher cap the table's last step (10,000)
/// goes on, so a floor of wage + 10% stays about 10% (a 331k player: 364,100 -> 360,000, not 400,000).
pub fn round_wage(mem: &mut guest::mem::Memory, wage: u32, cap: u32) -> u32 {
    let above = if cap > 250_000 { mem.read32(0x8200_560C + 8 * 5) } else { 100_000 };
    let step = (0..6).find(|&k| wage < mem.read32(0x8200_5610 + 8 * k)).map_or(above, |k| mem.read32(0x8200_560C + 8 * k));
    let r = ((step >> 1).wrapping_add(wage) / step).wrapping_mul(step);
    let r = (r / 10) * 10 & !1;
    if r > cap {
        cap
    } else {
        r.max(150)
    }
}

fn wage_round_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let wage = c.gpr[3] as u32;
    // LMA_WAGE_ROUND=check: the game's own rounding runs on a copy of the call and is compared
    // with ours at the game's cap (250,000), so the step table is read as the game reads it
    static CHECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if let (true, Some(game)) = (*CHECK.get_or_init(|| std::env::var("LMA_WAGE_ROUND").as_deref() == Ok("check")), WAGE_ROUND_GAME.get()) {
        let mut g = c.clone();
        if game(&mut g, m, pc, b)? == c.lr as u32 & !3 {
            let (theirs, ours) = (g.gpr[3] as u32, round_wage(m, wage, 250_000));
            static N: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];
            let n = N[(theirs != ours) as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if theirs != ours && n < 50 {
                eprintln!("LMA_WAGE_ROUND MISMATCH: {wage} -> game {theirs}, native {ours}");
            }
            if matches!(n + 1, 1 | 10 | 100) || (n + 1) % 1000 == 0 || (theirs != ours && n < 50) {
                eprintln!("LMA_WAGE_ROUND: {} agreed, {} mismatched", N[0].load(std::sync::atomic::Ordering::Relaxed), N[1].load(std::sync::atomic::Ordering::Relaxed));
            }
        }
    }
    c.gpr[3] = round_wage(m, wage, WAGE_MAX) as u64;
    *b -= 20;
    Ok(c.lr as u32 & !3)
}

/// The game's rounding, for `LMA_WAGE_ROUND=check`.
static WAGE_ROUND_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The native `WAGE_ROUND` for `ai_wage_offers` (the game's function is replaced: it only computes;
/// `game` is kept for `LMA_WAGE_ROUND=check`).
pub fn wage_round_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = WAGE_ROUND_GAME.set(game);
    wage_round_block
}

static AI_OFFER_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The wage from a player's rating, `rating_wage(r3, player r4) -> wage` (0x82120d38): a 10-step
/// table by rating (2..480, immediates) x (club tier x 15 + rating), by contract kind and nation,
/// rounded (`WAGE_ROUND`), then capped by league status and kind (`0x820055a8`, 60k-80k a week at
/// the top) and rounded again. A new player's wage (`WAGE`), his demands and `AI_OFFER` start here.
pub const RATING_WAGE: u32 = 0x8212_0D38;

/// Mod `rating_wage_scale` x 1000 (1000 = the game's own, no block).
pub static RATING_WAGE_SCALE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);

static RATING_WAGE_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The `RATING_WAGE` block for `rating_wage_scale`: the game's wage x the scale, rounded with
/// `round_wage` (capped at `WAGE_MAX` with `ai_wage_offers`, else at the game's 250,000).
pub fn rating_wage_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = RATING_WAGE_GAME.set(game);
    rating_wage_block
}

/// `wage` x `scale` / 1000, rounded as the game rounds a wage and capped at `cap`.
pub fn scale_rating_wage(mem: &mut guest::mem::Memory, wage: u32, scale: u32, cap: u32) -> u32 {
    round_wage(mem, (wage as u64 * scale as u64 / 1000).min(cap as u64) as u32, cap)
}

fn rating_wage_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let lr = c.lr as u32 & !3;
    let Some(game) = RATING_WAGE_GAME.get() else { unreachable!("rating_wage_block is only installed through rating_wage_block_for") };
    let next = call_to_the_end(c, m, pc, b, *game)?;
    if next != lr {
        return Ok(next); // something inside unwound: the game's answer stands
    }
    let scale = RATING_WAGE_SCALE.load(std::sync::atomic::Ordering::Relaxed);
    let cap = if AI_WAGE_OFFERS.load(std::sync::atomic::Ordering::Relaxed) { WAGE_MAX } else { 250_000 };
    c.gpr[3] = scale_rating_wage(m, c.gpr[3] as u32, scale, cap) as u64;
    Ok(lr)
}

/// The `AI_OFFER` block for `ai_wage_offers`: the game's offer, raised to the player's wage now
/// (the wage getter, so wages.csv's) + 10%, rounded with `round_wage`.
pub fn ai_offer_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = AI_OFFER_GAME.set(game);
    ai_offer_block
}

/// Run `game` from `pc` to its return without the slice running out on the way: the slice is
/// lengthened for the call (budget, `slice_len` and `slice_end` alike, so the guest's time base
/// reads as it would have) and its instructions stay charged to it. With the slice's own budget a
/// long call was cut short and resumed by the dispatcher, past the block (92% of personal-terms
/// calls in a live modded day, #663).
pub(crate) fn call_to_the_end(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64, game: guest::rec::FuncFn) -> Result<u32, guest::cpu::Stop> {
    const MORE: i64 = 1 << 40;
    (c.slice_len, c.slice_end, *b) = (c.slice_len + MORE, c.slice_end.wrapping_add(MORE as u64), *b + MORE);
    let r = game(c, m, pc, b);
    (c.slice_len, c.slice_end, *b) = (c.slice_len - MORE, c.slice_end.wrapping_sub(MORE as u64), *b - MORE);
    r
}

fn ai_offer_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let lr = c.lr as u32 & !3;
    let player = c.gpr[3] as u32;
    let call = c.clone();
    let Some(game) = AI_OFFER_GAME.get() else { unreachable!("ai_offer_block is only installed through ai_offer_block_for") };
    let next = call_to_the_end(c, m, pc, b, *game)?;
    if next != lr {
        return Ok(next); // something inside unwound (a hook or a thunk): the game's answer stands
    }
    // the player's wage now, through the game's getter (wages.csv's block in the mods)
    let mut w = call;
    w.gpr[3] = player as u64;
    let wage = match guest::rec::block_at(WAGE) {
        Some(getter) if call_to_the_end(&mut w, m, WAGE, b, getter)? == lr => w.gpr[3] as u32,
        _ => 0,
    };
    let floor = round_wage(m, (wage as u64 * 11 / 10).min(WAGE_MAX as u64) as u32, WAGE_MAX);
    let game_offer = c.gpr[3] as u32;
    c.gpr[3] = game_offer.max(floor) as u64;
    // LMA_AI_OFFER_LOG=<file> (#704): `player,wage,game_offer,offer` per AI wage offer, to count
    // the offers that reach players by wage band (the maintainer: hardly any above 100k a week)
    static LOG: std::sync::OnceLock<Option<std::sync::Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    if let Some(f) = LOG.get_or_init(|| std::env::var_os("LMA_AI_OFFER_LOG").and_then(|p| std::fs::File::create(p).ok()).map(std::sync::Mutex::new)) {
        use std::io::Write;
        let _ = writeln!(f.lock().unwrap(), "{player},{wage},{game_offer},{}", c.gpr[3]);
    }
    Ok(lr)
}

static WAGES: std::sync::RwLock<Option<HashMap<u32, (u32, Option<u32>)>>> = std::sync::RwLock::new(None);
static WAGE_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The native `WAGE` block for wages.csv (`native_mod_blocks`), falling back to `game` (the
/// recompiled getter) for every call `wage` would leave to it. As a hook, the getter unwound every
/// recompiled caller to the dispatcher (the transfer search's filter and personal terms call it per
/// player), which also hid their answers from the native blocks in front of them (#653).
pub fn wage_block_for(game: guest::rec::FuncFn, wages: &HashMap<u32, (u32, Option<u32>)>) -> guest::rec::FuncFn {
    let _ = WAGE_GAME.set(game);
    *WAGES.write().unwrap() = Some(wages.clone());
    wage_block
}

fn wage_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let w = WAGES.read().unwrap().as_ref().and_then(|wages| mod_wage(m, c.gpr[3] as u32 as i32, wages));
    match (w, WAGE_GAME.get()) {
        (Some(w), _) => {
            c.gpr[3] = w as u64;
            *b -= 1;
            Ok(c.lr as u32 & !3)
        }
        (None, Some(game)) => game(c, m, pc, b),
        (None, None) => unreachable!("wage_block is only installed through wage_block_for"),
    }
}

/// Contract length (`CONTRACT_SET`) for a player with `years` in the mod's wages.csv, the first time
/// it is set in a career (`CONTRACT_APPLIED` in +0x38): his real seasons left (the game's 5 at most);
/// later calls (a renewal in the game) run as they are.
pub fn contract_set(m: &mut Machine) -> bool {
    let idx = m.cpu.gpr[3] as u32 as i32;
    let Some(&(_, Some(years))) = (idx >= 0).then(|| m.wages.get(&(idx as u32))).flatten() else { return false };
    let table = m.mem.read32(PLAYER_TABLE);
    if table == 0 || idx >= 0x8000 {
        return false;
    }
    let rec = table + idx as u32 * 0x12c;
    let mark = m.mem.read32(rec + MARKS);
    if mark & CONTRACT_APPLIED != 0 {
        return false;
    }
    m.mem.write32(rec + MARKS, mark | CONTRACT_APPLIED);
    m.cpu.gpr[4] = years.clamp(1, 5) as u64;
    false
}

/// The cache's 19 bits.
pub const WAGE_MAX: u32 = (1 << 19) - 1;
/// The record word holding our marks: +0x54..+0x60 were 0 in all 27,290 records of three career
/// states (day 1 and after matches, 11 Oct), so the game leaves them alone and they go into its saves.
pub const MARKS: u32 = 0x5c;
/// This player's wages.csv wage is in.
pub const WAGE_APPLIED: u32 = 1 << 31;
/// This player's wages.csv contract is in.
pub const CONTRACT_APPLIED: u32 = 1 << 30;
/// The marks builds 24-25 left in +0x38 (bits 5 and 6), which turned out to be the season's goals.
pub const OLD_MARKS: u32 = 0x60;

/// Club funds (`FUNDS`) for a club in the mod's funds.csv (`Machine::funds`): exactly its target
/// (the maintainer, 11 Oct: every club's FC 27 budget as close as possible, lower leagues and worldwide too);
/// the 16 tiers x group multipliers came within 8% on average, this is exact.
pub fn funds(m: &mut Machine) -> bool {
    let uid = m.cpu.gpr[3] as u32;
    let map = m.mem.read32(CLUB_UID_MAP);
    if uid >= 0x2056 || map == 0 {
        return false;
    }
    let rec = m.mem.read16(map + uid * 2);
    let Some(&f) = m.funds.get(&rec) else { return false };
    m.cpu.gpr[3] = f.min(999_999_999) as u64;
    true
}

/// Sponsorship deals (Finance > Sponsors, 11 Oct; mod `sponsor_unit`). The human manager's deals live
/// in the manager block (`MANAGER_BLOCKS` + manager x `MANAGER_STRIDE`; the current manager at
/// `MANAGER_CUR` when the index argument is -2): 20 hoarding slots, one u32 bitfield each at
/// `(slot + 0x44e4) * 8` with the amount per week / 10 in bits 12..22 (11 bits, GBP 20,470 a week at
/// most), and the shirt deal at +0x3cd0 with the amount / 10 in bits 18..31 (14 bits, GBP 163,830).
/// Real 2025/26 deals are far above both (Arsenal's shirt ~GBP 960k a week), so the mod's unit
/// (`Machine::sponsor_unit`, vanilla 10) replaces the game's 10 in all four accessors.
pub const SPONSOR_HOARDING_GET: u32 = 0x8210_75A0;
pub const SPONSOR_HOARDING_SET: u32 = 0x8210_7608;
pub const SPONSOR_SHIRT_GET: u32 = 0x8210_71C8;
pub const SPONSOR_SHIRT_SET: u32 = 0x8210_7200;
/// The weekly hoarding income (the Sponsors screen's total; sums the slots inline, x10) and the whole
/// sponsorship income (shirt x10 + hoardings; the weekly credit), both with the game's own x10.
pub const SPONSOR_HOARDING_TOTAL: u32 = 0x8210_7DB8;
pub const SPONSOR_TOTAL: u32 = 0x8210_7E60;
/// Hoardings: up to 0xd8 of them, each an i16 slot index at block + 0x227c0 + 2 x i (-1 = none).
const HOARDINGS: u32 = 0xd8;
const HOARDING_SLOT_MAP: u32 = 0x2_27C0;
const MANAGER_BLOCKS: u32 = 0x8274_050C;
const MANAGER_CUR: u32 = 0x8270_34F8;
const MANAGER_STRIDE: u32 = 0x3_C700;
const HOARDING_SLOTS: u32 = 0x14;

/// The manager block the game's `FUN_820f7df8(index)` returns: index 0 or 1, else the current manager.
fn manager_block(m: &mut Machine, index: i32) -> u32 {
    let mut i = index;
    if !(0..=1).contains(&i) {
        i = m.mem.read32(MANAGER_CUR) as i32;
        if !(0..=1).contains(&i) {
            i = 0;
        }
    }
    m.mem.read32(MANAGER_BLOCKS).wrapping_add(i as u32 * MANAGER_STRIDE)
}

/// `SPONSOR_HOARDING_GET(manager, slot) -> GBP a week`.
pub fn sponsor_hoarding_get(m: &mut Machine) -> bool {
    let slot = m.cpu.gpr[4] as u32;
    if slot >= HOARDING_SLOTS {
        return false;
    }
    let idx = m.cpu.gpr[3] as i32;
    let a = manager_block(m, idx) + (slot + 0x44e4) * 8;
    let w = m.mem.read32(a);
    m.cpu.gpr[3] = (((w >> 12) & 0x7ff) * m.sponsor_unit) as u64;
    true
}

/// `SPONSOR_HOARDING_SET(manager, slot, GBP a week)`: the amount in units, capped at the field.
pub fn sponsor_hoarding_set(m: &mut Machine) -> bool {
    let slot = m.cpu.gpr[4] as u32;
    if slot >= HOARDING_SLOTS {
        return false;
    }
    // with sponsors.csv the weekly credit isn't scaled (`real_sponsorship`): the hoardings take the
    // mod's income scale here, so they stay on the scale of the other income
    if !m.sponsors.is_empty() {
        m.cpu.gpr[5] = scaled_income(m.cpu.gpr[5] as u32 as i32, m.income_scale) as u32 as u64;
    }
    let units = ((m.cpu.gpr[5] as i32).max(0) as u32 / m.sponsor_unit).min(0x7ff);
    let idx = m.cpu.gpr[3] as i32;
    let a = manager_block(m, idx) + (slot + 0x44e4) * 8;
    let w = m.mem.read32(a);
    m.mem.write32(a, (w & 0xFF80_0FFF) | (units << 12));
    true
}

/// `SPONSOR_SHIRT_GET(manager) -> GBP a week`.
pub fn sponsor_shirt_get(m: &mut Machine) -> bool {
    let idx = m.cpu.gpr[3] as i32;
    let a = manager_block(m, idx) + 0x3cd0;
    let w = m.mem.read32(a);
    m.cpu.gpr[3] = ((w >> 18) * m.sponsor_unit) as u64;
    true
}

/// `SPONSOR_SHIRT_SET(manager, GBP a week)`: the amount in units, capped at the field.
pub fn sponsor_shirt_set(m: &mut Machine) -> bool {
    let idx = m.cpu.gpr[3] as i32;
    let block = manager_block(m, idx);
    // a mod's sponsors.csv: the managed club's real shirt deal (per season / 52), whatever the
    // generator offered (the manager block's +0x564 is the club's uid; records are by index)
    let uid = m.mem.read16(block + 0x564) as u32;
    let map = m.mem.read32(CLUB_UID_MAP);
    let rec = (uid < 0x2056 && map != 0).then(|| m.mem.read16(map + uid * 2));
    if let Some(&(shirt, _)) = rec.and_then(|c| m.sponsors.get(&c)) {
        m.cpu.gpr[4] = (shirt / 52) as u64;
    }
    let units = ((m.cpu.gpr[4] as i32).max(0) as u32 / m.sponsor_unit).min(0x3fff);
    let a = block + 0x3cd0;
    let w = m.mem.read32(a);
    m.mem.write32(a, (w & 0x0003_FFFF) | (units << 18));
    true
}

/// `SPONSOR_HOARDING_TOTAL(manager) -> GBP a week`: the slots' amounts summed over the hoardings that
/// point at them (the game's 0x82107db8 with the mod's unit; nothing when the manager has no deals,
/// block + 0x3cce == 0).
pub fn sponsor_hoarding_total(m: &mut Machine) -> bool {
    let idx = m.cpu.gpr[3] as i32;
    let block = manager_block(m, idx);
    m.cpu.gpr[3] = hoarding_total(m, block) as u64;
    true
}

fn hoarding_total(m: &mut Machine, block: u32) -> u32 {
    if m.mem.read8(block + 0x3cce) == 0 {
        return 0;
    }
    let mut total = 0u32;
    for i in 0..HOARDINGS {
        let slot = m.mem.read16(block + HOARDING_SLOT_MAP + i * 2) as i16;
        if (0..HOARDING_SLOTS as i16).contains(&slot) {
            let w = m.mem.read32(block + (slot as u32 + 0x44e4) * 8);
            total = total.saturating_add(((w >> 12) & 0x7ff) * m.sponsor_unit);
        }
    }
    total
}

/// `SPONSOR_TOTAL(manager) -> GBP a week`: the shirt deal plus the hoardings (the game's 0x82107e60).
pub fn sponsor_total(m: &mut Machine) -> bool {
    let idx = m.cpu.gpr[3] as i32;
    let block = manager_block(m, idx);
    let shirt = (m.mem.read32(block + 0x3cd0) >> 18) * m.sponsor_unit;
    let total = shirt.saturating_add(hoarding_total(m, block));
    m.cpu.gpr[3] = total as u64;
    true
}

/// A player's overall, `player_overall(player id) -> 1..99` (r3): the day update's top function
/// (#39: 18 M of the day segment's guest instructions, and most calls of the 0x820dad10 getter).
/// The player's role (`player_role`) picks a row of 13 weights; the overall is the weighted mean
/// of his 13 ratings, truncated. Answered natively by default (`LMA_NATIVE_OVERALL=0`: the game's): the same r3, but not
/// the same instruction count, volatile registers or stack bytes, so not on by default.
pub const PLAYER_OVERALL: u32 = 0x8211_F4C8;
/// `[[PLAYER_COUNT] + 4]`: how many player records `PLAYER_TABLE` has (0x820dad10; 0 while unset).
const PLAYER_COUNT: u32 = 0x8273_D6AC;
/// The overall's weights: 10 roles x 14 bytes (13 used), in the image (a mod may patch them).
const OVERALL_WEIGHTS: u32 = 0x8200_5468;
/// The 13 ratings as the overall reads them, in weight order: (record word, left rotation), 7 bits.
const OVERALL_RATINGS: [(u32, u32); 13] = [(4, 7), (4, 14), (4, 21), (4, 28), (28, 14), (8, 7), (8, 14), (8, 28), (16, 21), (16, 28), (16, 14), (20, 23), (12, 14)];

/// The record the game's field getters read for `p`: 0 (so they read low memory, as the game does)
/// for a negative id or one past the table.
fn player_record(mem: &mut guest::mem::Memory, p: u32) -> u32 {
    let at = mem.read32(PLAYER_COUNT);
    let count = if at == 0 { 0 } else { mem.read32(at + 4) };
    if (p as i32) < 0 || (p as i32) >= count as i32 {
        return 0;
    }
    mem.read32(PLAYER_TABLE).wrapping_add(p.wrapping_mul(300))
}

/// A record field: `rotl(word, rot) & mask`, as the getters' rlwinm.
fn field(mem: &mut guest::mem::Memory, rec: u32, word: u32, rot: u32, mask: u32) -> i32 {
    (mem.read32(rec.wrapping_add(word)).rotate_left(rot) & mask) as i32
}

/// The game's player role, `player_role(player id) -> 0..9` (r3; 6% of the day segment's host time
/// besides its calls from the overall). Natively by default, as `PLAYER_OVERALL`.
pub const PLAYER_ROLE: u32 = 0x8211_BE88;

/// The game's player role (`PLAYER_ROLE`), the overall's weight row: 0 keeper; 1-2 defender (by a
/// flag); 3-6 midfielder (flags, then two sums of three ratings); 7-9 forward (two rating means).
pub fn player_role(mem: &mut guest::mem::Memory, p: u32) -> u32 {
    let rec = player_record(mem, p);
    let f = |mem: &mut guest::mem::Memory, w, r, k| field(mem, rec, w, r, k);
    match f(mem, 4, 30, 3) {
        0 => 0,
        1 => {
            if f(mem, 24, 0, 1) == 1 {
                2
            } else {
                1
            }
        }
        2 => {
            if f(mem, 24, 31, 1) == 1 || f(mem, 24, 30, 1) == 1 {
                match f(mem, 24, 0, 1) {
                    0 => return 6,
                    1 => return 4,
                    _ => {}
                }
            }
            let s1 = f(mem, 4, 28, 0x7f) + f(mem, 4, 21, 0x7f) + f(mem, 16, 21, 0x7f);
            let s2 = f(mem, 4, 7, 0x7f) + f(mem, 4, 14, 0x7f) + f(mem, 20, 23, 0x7f);
            if s2 - s1 > 4 {
                3
            } else if s1 - s2 > 25 {
                5
            } else {
                4
            }
        }
        _ => {
            let a = (f(mem, 8, 28, 0x7f) + f(mem, 8, 7, 0x7f) + f(mem, 28, 14, 0x7f) + f(mem, 16, 21, 0x7f)) / 4;
            let b = (f(mem, 8, 14, 0x7f) + f(mem, 20, 23, 0x7f)) / 2;
            if b - a > 20 {
                9
            } else if a - b > 30 {
                7
            } else {
                8
            }
        }
    }
}

/// The game's overall for `p`, or None where it would trap (all weights 0).
pub fn player_overall_of(mem: &mut guest::mem::Memory, p: u32) -> Option<u32> {
    let role = player_role(mem, p);
    let rec = player_record(mem, p);
    let (mut sum, mut weights) = (0i32, 0i32);
    for (k, &(w, r)) in OVERALL_RATINGS.iter().enumerate() {
        let weight = mem.read8(OVERALL_WEIGHTS + role * 14 + k as u32) as i32;
        sum = sum.wrapping_add(field(mem, rec, w, r, 0x7f).wrapping_mul(weight));
        weights = weights.wrapping_add(weight);
    }
    (weights != 0).then(|| sum.wrapping_div(weights) as u32)
}

/// Overalls answered by `native_blocks` (lma-cli run reports them).
pub static NATIVE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Native functions for the block table (`Machine::install_blocks`), in place of the recompiled
/// ones: the overall and the role, unless LMA_NATIVE_OVERALL is 0 (or hook / count). `rec::call` runs them like any
/// recompiled function (no unwind to the dispatcher, as a hook takes); they return to lr.
pub fn native_blocks() -> Vec<(u32, guest::rec::FuncFn)> {
    // on by default (the maintainer, 12 Oct); `0` keeps the game's own functions, `hook`/`count` use hooks
    if matches!(std::env::var("LMA_NATIVE_OVERALL").as_deref(), Ok("0") | Ok("hook") | Ok("count")) {
        return Vec::new();
    }
    vec![(PLAYER_OVERALL, overall_block as guest::rec::FuncFn), (PLAYER_ROLE, role_block as guest::rec::FuncFn)]
}

/// The mods' valuation (`values_as_stored`) and club lookup (squad_overflow.csv) as native blocks
/// with the game's function behind them; LMA_NATIVE_MOD=hook keeps them hooks. A hook unwinds to
/// the dispatcher on every call, and the day's processing prices players and looks up their clubs
/// millions of times (#39).
pub fn native_mod_blocks() -> bool {
    std::env::var("LMA_NATIVE_MOD").as_deref() != Ok("hook")
}

/// `values_as_stored`'s base levels for the native valuation block (`Machine::install_blocks`).
pub static VALUE_LEVELS: std::sync::RwLock<Vec<f32>> = std::sync::RwLock::new(Vec::new());
/// The game's own valuation, for the players the native block leaves to it.
static VALUATION_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The native valuation block, falling back to `game` (the recompiled function) for a player
/// without a stored value.
pub fn valuation_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = VALUATION_GAME.set(game);
    valuation_block
}

fn valuation_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let v = stored_valuation(m, c.gpr[3] as u32 as i32, &VALUE_LEVELS.read().unwrap());
    match (v, VALUATION_GAME.get()) {
        (Some(v), _) => {
            c.gpr[3] = v;
            *b -= 1;
            Ok(c.lr as u32 & !3)
        }
        (None, Some(game)) => game(c, m, pc, b),
        (None, None) => unreachable!("valuation_block is only installed through valuation_block_for"),
    }
}

fn overall_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, _pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    NATIVE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // all-zero weights would trap in the game (divw by 0); the recompiled divw gives 0
    c.gpr[3] = player_overall_of(m, c.gpr[3] as u32).unwrap_or(0) as u64;
    *b -= 1;
    Ok(c.lr as u32 & !3)
}

fn role_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, _pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    c.gpr[3] = player_role(m, c.gpr[3] as u32) as u64;
    *b -= 1;
    Ok(c.lr as u32 & !3)
}

/// `PLAYER_ROLE` natively (LMA_NATIVE_OVERALL=hook).
pub fn player_role_hook(m: &mut Machine) -> bool {
    *m.hook_calls.entry(PLAYER_ROLE).or_default() += 1;
    m.cpu.gpr[3] = player_role(&mut m.mem, m.cpu.gpr[3] as u32) as u64;
    true
}

/// `PLAYER_OVERALL` natively (LMA_NATIVE_OVERALL=hook).
pub fn player_overall(m: &mut Machine) -> bool {
    *m.hook_calls.entry(PLAYER_OVERALL).or_default() += 1;
    let Some(v) = player_overall_of(&mut m.mem, m.cpu.gpr[3] as u32) else { return false };
    m.cpu.gpr[3] = v as i32 as i64 as u64;
    true
}

/// The game's "whose player is this" lookup, `player_club(player id) -> club uid` (r3), 198 call
/// sites. It trusts a per-player cache of club uids (`[0x827406cc]`, u16 per player) and only on a
/// miss walks every club's 32-id squad array, so a club can't own more than 32 players (#538).
pub const PLAYER_CLUB: u32 = 0x8211_8FC0;

/// The club uid of squad_overflow.csv player `pid` (`overflow`: player id -> club record), or None
/// for anyone else and before the uid table exists. A club's uid is found once and kept in `uids`.
pub fn overflow_club(mem: &mut guest::mem::Memory, pid: u32, overflow: &HashMap<u16, u16>, uids: &mut HashMap<u16, u16>) -> Option<u32> {
    if pid > 0xffff {
        return None;
    }
    let &rec = overflow.get(&(pid as u16))?;
    if let Some(&u) = uids.get(&rec) {
        return Some(u as u32);
    }
    let map = mem.read32(CLUB_UID_MAP);
    if map == 0 {
        return None;
    }
    // the uid table maps uid -> record; the record's uid is the first entry that points at it,
    // found once per club (the lookup runs often in the day's processing)
    let u = (1..0x2056u32).find(|&u| mem.read16(map + u * 2) == rec)?;
    uids.insert(rec, u as u16);
    Some(u)
}

/// squad_overflow.csv for the native `PLAYER_CLUB` block (`Machine::install_blocks`): a bit per
/// player id, so the ~99% of calls for other players reach the game's function without a lock,
/// then the map and the uid cache.
static OVERFLOW_BITS: [std::sync::atomic::AtomicU64; 1024] = [const { std::sync::atomic::AtomicU64::new(0) }; 1024];
static OVERFLOW: std::sync::Mutex<Option<(HashMap<u16, u16>, HashMap<u16, u16>)>> = std::sync::Mutex::new(None);
static PLAYER_CLUB_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The native `PLAYER_CLUB` block for `overflow`, falling back to `game` (the recompiled lookup) for
/// every other player.
pub fn player_club_block_for(game: guest::rec::FuncFn, overflow: &HashMap<u16, u16>) -> guest::rec::FuncFn {
    let _ = PLAYER_CLUB_GAME.set(game);
    for w in &OVERFLOW_BITS {
        w.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    for &pid in overflow.keys() {
        OVERFLOW_BITS[pid as usize >> 6].fetch_or(1 << (pid & 63), std::sync::atomic::Ordering::Relaxed);
    }
    *OVERFLOW.lock().unwrap() = Some((overflow.clone(), HashMap::new()));
    player_club_block
}

fn player_club_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let pid = c.gpr[3] as u32;
    let flagged = pid <= 0xffff && OVERFLOW_BITS[pid as usize >> 6].load(std::sync::atomic::Ordering::Relaxed) & (1 << (pid & 63)) != 0;
    let uid = if flagged {
        let mut g = OVERFLOW.lock().unwrap();
        g.as_mut().and_then(|(overflow, uids)| overflow_club(m, pid, overflow, uids))
    } else {
        None
    };
    match (uid, PLAYER_CLUB_GAME.get()) {
        (Some(u), _) => {
            c.gpr[3] = u as u64;
            *b -= 1;
            Ok(c.lr as u32 & !3)
        }
        (None, Some(game)) => game(c, m, pc, b),
        (None, None) => unreachable!("player_club_block is only installed through player_club_block_for"),
    }
}

/// `PLAYER_CLUB` for a player in the mod's squad_overflow.csv (`Machine::squad_overflow`): his
/// club, though he isn't in its 32-id array (the squad the game picks from and shows), so a club can
/// keep a modern squad beyond 32 (#538). Wages come from wages.csv as for every player.
pub fn player_club(m: &mut Machine) -> bool {
    let Some(uid) = overflow_club(&mut m.mem, m.cpu.gpr[3] as u32, &m.squad_overflow, &mut m.squad_overflow_uids) else { return false };
    m.cpu.gpr[3] = uid as u64;
    // answered lookups, for lma-cli run's hook report
    *m.hook_calls.entry(PLAYER_CLUB).or_default() += 1;
    true
}

/// The "who goes to Europe" step for one league (#607): the season reset (`0x8213a918`), the
/// qualification conditions (`0x821398e0`, e.g. the Qualification Summary screen) and the screens'
/// per-nation lists (`0x821399c8`) build a nation's places record with `0x82138b50`, then this function (r3 = the nation, 0..7 in LMA's
/// country order: England, Scotland, Italy, Spain, Germany, France, Netherlands, Portugal; r4 = the
/// record) turns the final table into entrants: the top `+4` clubs to the Champions League group
/// stage, the next `+0` to its qualifying round, the next `+8` to the UEFA Cup, then the cup
/// winners (`+0x14..+0x24`) and the table's top 10 (`+0x28`). The counts are hard-coded per nation
/// in `0x82138b50` (England 2 + 2 + 1, as in 2006).
pub const EUROPE_PLACES: u32 = 0x8213_8E68;

/// `europe_places` of the mod (`ModSpace::europe_places`): per nation, [group stage, qualifying
/// round, UEFA Cup] places.
pub static EUROPE_PLACES_TABLE: std::sync::RwLock<Vec<[u32; 3]>> = std::sync::RwLock::new(Vec::new());

/// The nations whose places have been logged this session (a bit each).
static EUROPE_PLACES_LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// `EUROPE_PLACES`: writes the mod's counts into the nation's record and declines, so the game's
/// function hands out the places from them.
pub fn europe_places(m: &mut Machine) -> bool {
    let (nation, record) = (m.cpu.gpr[3] as u32, m.cpu.gpr[4] as u32);
    if let Some(&[group, qualifying, uefa]) = EUROPE_PLACES_TABLE.read().unwrap().get(nation as usize) {
        let was = [m.mem.read32(record + 4), m.mem.read32(record), m.mem.read32(record + 8)];
        m.mem.write32(record + 4, group);
        m.mem.write32(record, qualifying);
        m.mem.write32(record + 8, uefa);
        *m.hook_calls.entry(EUROPE_PLACES).or_default() += 1;
        // the screens ask often: each league's first time only, on stderr where a play session's log keeps it
        if EUROPE_PLACES_LOGGED.fetch_or(1 << (nation & 31), std::sync::atomic::Ordering::Relaxed) & (1 << (nation & 31)) == 0 {
            let top: Vec<String> = (0..6).map(|k| format!("{:#x}", m.mem.read32(record + 0x28 + 4 * k))).collect();
            eprintln!("europe places, nation {nation} (top 6 uids {}): {was:?} -> {:?}", top.join(" "), [group, qualifying, uefa]);
        }
    }
    false
}

/// The AI transfer search's player filter, `filter(_, entry, criteria) -> match` (#39): `0x82179e48`
/// walks the player list (16-byte entries: player u16, club u16, ...) and asks it about each player.
/// It computes the player's valuation first (`0x821208e0`, the vanilla day's biggest cost), then
/// tests the criteria: among them the searching club (`criteria + 16`, its own players never match)
/// and a position (`criteria + 24`, 4 = any; the player's first or second position must equal it,
/// record word +4 bits 2-3 and 0-1).
pub const SEARCH_FILTER: u32 = 0x8217_94C8;

/// LMA_SEARCH_PREFILTER=0 keeps the game's filter alone; `count` installs the block but never
/// answers (the A/B baseline: it counts the calls, and LMA_SEARCH_LOG=1 prints the counts).
pub fn search_prefilter_on() -> bool {
    std::env::var("LMA_SEARCH_PREFILTER").as_deref() != Ok("0")
}

/// The game's "yes" answers in check mode (LMA_SEARCH_LOG prints them).
pub static SEARCH_YES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// [calls, answered natively]
pub static SEARCH_CALLS: [std::sync::atomic::AtomicU64; 2] = [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

static SEARCH_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The native pre-filter, the game's filter (`game`, recompiled) behind it.
pub fn search_filter_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = SEARCH_GAME.set(game);
    search_filter_block
}

/// Whether the filter can answer "no" without the game: a player of the searching club, a flagged
/// player (record +0x28 bit 0), a player at a flagged club (club word +4 bit 1), at a club of an
/// unwanted country or whose tier (word 0 >> 28) is outside the criteria's range, one whose ability
/// is below the minimum, or one whose two positions both differ from the one searched for. The game
/// returns 1 only when all of these pass, so these answers equal its own; the calls they skip (the
/// valuation and the getters before the tests) only read, apart from caches (the club lookup's) they
/// would fill with the same values. The club is the entry's own u16 (+2), as the game reads it (`lhz`, `extsh`), not the
/// player-club lookup, so overflow players (squad_overflow.csv) are judged by the same club.
pub fn search_rejects(mem: &mut guest::mem::Memory, entry: u32, criteria: u32) -> bool {
    let (player, club) = (mem.read16(entry) as i16 as i32, mem.read16(entry + 2) as i16 as i32);
    if club == mem.read32(criteria + 16) as i32 {
        return true;
    }
    let at = mem.read32(PLAYER_COUNT);
    let count = if at == 0 { 0 } else { mem.read32(at + 4) as i32 };
    // the game's getters read a record at address 0 for an id out of range
    let record = if player < 0 || player >= count { 0 } else { mem.read32(PLAYER_TABLE).wrapping_add(player as u32 * 300) };
    // a flagged player (record +0x28 bit 0, 0x8211e8f0) never matches
    if mem.read32(record.wrapping_add(0x28)) & 1 != 0 {
        return true;
    }
    if club >= 1 {
        // the club's runtime record by uid, as 0x820e3e68 and 0x820e95c0 find it (address 0 when
        // the uid is out of range or unmapped)
        let club_rec = club_record(mem, club as u32);
        // its country must be wanted (criteria + (country + 10) x 4 non-zero)
        let country = club_country(mem, club as u32);
        if mem.read32(criteria.wrapping_add((country + 10) * 4)) == 0 {
            return true;
        }
        // a flagged club (word +4 bit 1) never matches
        if (mem.read32(club_rec.wrapping_add(4)) >> 1) & 1 != 0 {
            return true;
        }
        // its tier (word 0 >> 28) must lie in [criteria + 12, criteria + 8]
        let tier = (mem.read32(club_rec) >> 28) as i32;
        if tier > mem.read32(criteria + 8) as i32 || tier < mem.read32(criteria + 12) as i32 {
            return true;
        }
    }
    let w = mem.read32(record.wrapping_add(4));
    // the ability (0x8211f878) must reach criteria + 0: the overall, cut for a player past his
    // position's peak age (u16 table at 0x8200556c); its overall is the native block's when that is
    // on, so then this is the game's own number
    static NATIVE_OVERALL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *NATIVE_OVERALL.get_or_init(|| !matches!(std::env::var("LMA_NATIVE_OVERALL").as_deref(), Ok("0") | Ok("hook") | Ok("count"))) {
        let ability = if player == -1 {
            0
        } else {
            let overall = player_overall_of(mem, player as u32).unwrap_or(0) as i32;
            let age = ((mem.read32(record.wrapping_add(0x14)) >> 4) & 0x1f) as i32 + 16;
            let past_peak = age - mem.read16(PEAK_AGES + ((w >> 1) & 6)) as i32;
            match past_peak {
                ..26 => overall,
                26..30 => overall * 95 / 100,
                30..33 => overall * 88 / 100,
                _ => overall * 50 / 100,
            }
        };
        if ability < mem.read32(criteria) as i32 {
            return true;
        }
    }
    let pos = mem.read32(criteria + 24) as i32;
    if pos == 4 {
        return false;
    }
    ((w >> 2) & 3) as i32 != pos && (w & 3) as i32 != pos
}

/// The stadium records (0x20 bytes each), read by 0x8212e168 for a club's country.
const STADIUM_RECORDS: u32 = 0x8274_1278;

/// A club's runtime record by uid, as 0x820e3e68 and 0x820e95c0 find it (0 when the uid is out of
/// range or unmapped).
fn club_record(mem: &mut guest::mem::Memory, uid: u32) -> u32 {
    let map = mem.read32(CLUB_UID_MAP);
    let index = if uid.wrapping_sub(1) < 0x2056 { mem.read16(map.wrapping_add(uid * 2)) as i16 } else { -1 };
    if (0..0x708).contains(&index) { mem.read32(CLUB_RECORDS).wrapping_add(index as u32 * 0x3d8) } else { 0 }
}

/// A club's country: its stadium's, as 0x820e4798 -> 0x8212e168 find it (0x33 for an unmapped club).
fn club_country(mem: &mut guest::mem::Memory, uid: u32) -> u32 {
    let rec = club_record(mem, uid);
    if rec == 0 {
        return 0x33;
    }
    let stadium = mem.read32(rec.wrapping_add(4)) >> 21;
    let at = if stadium < 2000 { mem.read32(STADIUM_RECORDS).wrapping_add(stadium * 0x20 + 0xc) } else { 0xc };
    (mem.read32(at) >> 15) & 0x3f
}

/// Peak age per position (u16 each, indexed by the record's first position x 2), read by 0x8211f878.
const PEAK_AGES: u32 = 0x8200_556C;

/// The clubs' runtime records (0x3d8 bytes each), indexed through `CLUB_UID_MAP`.
const CLUB_RECORDS: u32 = 0x8273_D6C4;

fn search_filter_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    use std::sync::atomic::Ordering::Relaxed;
    static MODE: std::sync::OnceLock<(bool, bool)> = std::sync::OnceLock::new();
    let (count_only, log) = *MODE.get_or_init(|| (matches!(std::env::var("LMA_SEARCH_PREFILTER").as_deref(), Ok("count") | Ok("check")), std::env::var_os("LMA_SEARCH_LOG").is_some()));
    let n = SEARCH_CALLS[0].fetch_add(1, Relaxed) + 1;
    if log && n % (1 << 14) == 0 {
        eprintln!("search filter: {n} calls, {} answered natively, {} yes, guest time {}", SEARCH_CALLS[1].load(Relaxed), SEARCH_YES.load(Relaxed), guest::rec::timebase_at(c, *b));
    }
    if !count_only && search_rejects(m, c.gpr[4] as u32, c.gpr[5] as u32) {
        SEARCH_CALLS[1].fetch_add(1, Relaxed);
        c.gpr[3] = 0;
        *b -= 1;
        return Ok(c.lr as u32 & !3);
    }
    let Some(game) = SEARCH_GAME.get() else { unreachable!("search_filter_block is only installed through search_filter_block_for") };
    // LMA_SEARCH_PREFILTER=check: the game decides every call, and a "yes" the pre-filter would have
    // answered "no" is reported (none may appear)
    static CHECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *CHECK.get_or_init(|| std::env::var("LMA_SEARCH_PREFILTER").as_deref() == Ok("check")) {
        let (entry, criteria, lr) = (c.gpr[4] as u32, c.gpr[5] as u32, c.lr as u32 & !3);
        let would = search_rejects(m, entry, criteria);
        let next = game(c, m, pc, b)?;
        if next == lr && !would && c.gpr[3] as u32 != 0 {
            SEARCH_YES.fetch_add(1, Relaxed);
        }
        if next == lr && would {
            if c.gpr[3] as u32 != 0 {
                eprintln!("search filter MISMATCH: entry {entry:#x} (player {}, club {}) matched, the pre-filter would have said no", m.read16(entry) as i16, m.read16(entry + 2) as i16);
            }
            SEARCH_CALLS[1].fetch_add(1, Relaxed);
        }
        return Ok(next);
    }
    game(c, m, pc, b)
}

/// The match's slot lookup by player id, `lookup(obj, id) -> slot or 0` (#530): it scans the 2 x 16
/// player slots (`obj + 3008 + team x 19408 + i x 0x400`, word 0 the id). The in-match substitution
/// (0x8226c130) looks up the player coming on with it before swapping the two slots' images
/// (0x8226bf30).
pub const SLOT_LOOKUP: u32 = 0x8225_EDF0;
/// The match builder, `build(G + 32, obj = G + 20640, ...)`: clears the match object and makes slots
/// 0-15 from the pre-match squad entries 0-15 (`G + 5936 + team x 7296 + k x 228`).
pub const MATCH_BUILD: u32 = 0x8225_BF10;
/// The lookup that may swap a reserve in: 0x8226c130's of the player coming on (the substitution
/// itself; the queue entry's first id). The player going off is always in a slot (0x8226c1e0), so
/// only the incoming one can miss the scan; with 0x8226c1e0 here a queued reserve came on as an
/// empty slot and the player going off was written to address 0 (#530).
const SUB_LOOKUP_RETS: [u32; 1] = [0x8226_C1C0];
/// The lookups that find a queued reserve where the subs menu queued him, his virtual slot
/// (`slot_get`), never swapping anyone in: the menu's undo of a pending substitution (0x82190130,
/// both players) and the cancel it calls (0x8226b9b8, both players). Swapping him in there made
/// the undo find nothing to cancel (#530, the maintainer 28 Oct: "can't undo the sub before confirming it").
const UNDO_LOOKUP_RETS: [u32; 4] = [0x8219_01DC, 0x8219_01EC, 0x8226_BA00, 0x8226_BA28];
/// A slot's state (+996) while its player is queued to come on (the subs menu's pending substitution).
const STATE_QUEUED: u32 = 6;
const SLOTS_AT: u32 = 3008;
const TEAM_STRIDE: u32 = 19408;
const SLOT_SIZE: usize = 0x400;
/// The pre-match squad entries from the match object: G + 5936 - (G + 20640).
const ENTRIES_FROM_OBJ: u32 = 20640 - 5936;
const ENTRY_STRIDE: u32 = 228;
const ENTRY_TEAM_STRIDE: u32 = 7296;
/// A slot's state word (+996): 0 before kick-off; in the match 1 on the pitch, 2 on the bench, 4 for
/// the player a substitution took off (0x8226bf30), 3 for the one it brought on.
const SLOT_STATE: usize = 996;
const STATE_BENCH: u32 = 2;

/// A bench player who hasn't played: 0 before kick-off, 2 in the match.
fn unused_state(state: u32) -> bool {
    state == 0 || state == STATE_BENCH
}
const SLOT_ON_PITCH: usize = 1012;

/// Mod `bench` (5..9) minus 5: the reserves after the matchday 16 (pre-match entries 16..) that can
/// come on as substitutes (#530). 0 = vanilla.
pub static BENCH_EXTRA: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Slot images of reserves not in a slot, by (team, id): a bench player swapped out to make room
/// for one of them. Cleared when a match is built.
static RESERVES: std::sync::Mutex<Option<HashMap<(u32, u32), Vec<u8>>>> = std::sync::Mutex::new(None);

fn slot_at(obj: u32, team: u32, i: u32) -> u32 {
    obj + SLOTS_AT + team * TEAM_STRIDE + i * SLOT_SIZE as u32
}

/// A team's pre-match entries 16..32 hold the rest of its squad, with gaps (-1); the named reserves
/// are the first `extra` (at most 4) players there.
fn named_reserves(mem: &mut guest::mem::Memory, obj: u32, team: u32, extra: u32) -> Vec<(u32, u32)> {
    let entries = obj.wrapping_sub(ENTRIES_FROM_OBJ) + team * ENTRY_TEAM_STRIDE;
    (16..ENTRY_TEAM_STRIDE / ENTRY_STRIDE)
        .map(|k| (mem.read32(entries + k * ENTRY_STRIDE), entries + k * ENTRY_STRIDE))
        .filter(|&(id, _)| id != u32::MAX)
        .take(extra.min(4) as usize)
        .collect()
}

/// The players a team can still bring on from past its 16 slots, as (id, pre-match entry or 0 for a
/// substitute displaced earlier): the named reserves not in a slot, then the displaced substitutes
/// (by id). The team screen's rows 16.. list them in this order.
fn reserve_list(mem: &mut guest::mem::Memory, obj: u32, team: u32, extra: u32) -> Vec<(u32, u32)> {
    let in_slot: Vec<u32> = (0..2).flat_map(|t| (0..16).map(move |i| slot_at(obj, t, i))).map(|s| mem.read32(s)).collect();
    let mut list: Vec<(u32, u32)> = named_reserves(mem, obj, team, extra).into_iter().filter(|(id, _)| !in_slot.contains(id)).collect();
    let mut displaced: Vec<u32> = RESERVES.lock().unwrap().as_ref().map_or(Vec::new(), |r| r.keys().filter(|&&(t, _)| t == team).map(|&(_, id)| id).collect());
    displaced.sort_unstable();
    for id in displaced {
        if !in_slot.contains(&id) && !list.iter().any(|&(l, _)| l == id) {
            list.push((id, 0));
        }
    }
    list.truncate(4);
    list
}

/// The game's lookup (the slot whose word 0 is `id`, else 0), and for the substitution's incoming
/// player (`swap_in`) one of the `extra` named reserves (`named_reserves`): its image goes into
/// a bench slot, preferring one whose player already went off, else slot 15.
pub fn slot_lookup(mem: &mut guest::mem::Memory, obj: u32, id: u32, swap_in: bool, extra: u32) -> u32 {
    for team in 0..2 {
        for i in 0..16 {
            let s = slot_at(obj, team, i);
            if mem.read32(s) == id {
                return s;
            }
        }
    }
    if !swap_in || extra == 0 || id == u32::MAX {
        return 0;
    }
    // a substitute swapped out earlier for a reserve
    let stored = RESERVES.lock().unwrap().as_ref().and_then(|r| (0..2).find(|&team| r.contains_key(&(team, id))));
    if let Some(team) = stored {
        return bring_in(mem, obj, team, id, None);
    }
    for team in 0..2 {
        if let Some((_, entry)) = named_reserves(mem, obj, team, extra).into_iter().find(|&(r, _)| r == id) {
            return bring_in(mem, obj, team, id, Some(entry));
        }
    }
    0
}

/// The slot a reserve's image starts from: the team's last bench slot holding a player. An empty
/// one (id -1, no player injured or named) holds zeros where a player's slot has his team (+0x230,
/// the substitution's counter of substitutions made: a reserve made from one came on uncounted, the
/// count written at guest 0x4ba8, and the dugout listed the substitutions before his only, #530),
/// floats of 1.0 and the bench state; slot 15 when the whole bench is empty.
fn bench_template(mem: &mut guest::mem::Memory, obj: u32, team: u32) -> u32 {
    (11..16).rev().map(|i| slot_at(obj, team, i)).find(|&s| mem.read32(s) != u32::MAX).unwrap_or_else(|| slot_at(obj, team, 15))
}

fn read_slot(mem: &mut guest::mem::Memory, s: u32) -> Vec<u8> {
    let mut v = vec![0u8; SLOT_SIZE];
    mem.read(s, &mut v);
    v
}

fn word(v: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(v[at..at + 4].try_into().unwrap())
}

/// Put reserve `id` of `team` into a bench slot and return the slot (0 when every bench slot holds a
/// player who already played): its stored image, else one made from its pre-match `entry`. The slot
/// is an unused substitute's, the last one first; a player who went off keeps his slot, which holds
/// his match (the ratings list 0x8224cb30 and the end of the match read it).
fn bring_in(mem: &mut guest::mem::Memory, obj: u32, team: u32, id: u32, entry: Option<u32>) -> u32 {
    let bench: Vec<u32> = (11..16).map(|i| slot_at(obj, team, i)).collect();
    let Some(target) = bench.iter().rev().copied().find(|&s| unused_state(mem.read32(s + SLOT_STATE as u32))) else { return 0 };
    let mut reserves = RESERVES.lock().unwrap();
    let reserves = reserves.get_or_insert_with(HashMap::new);
    let mut image = match (reserves.remove(&(team, id)), entry) {
        (Some(v), _) => v,
        (None, None) => return 0,
        (None, Some(entry)) => {
            // as the builder makes a bench slot: a bench slot's image with the entry's id and its 220
            // bytes of player data twice (+8 and +228)
            let template = bench_template(mem, obj, team);
            let mut v = read_slot(mem, template);
            let mut data = [0u8; 220];
            mem.read(entry + 8, &mut data);
            v[0..4].copy_from_slice(&id.to_be_bytes());
            v[8..228].copy_from_slice(&data);
            v[228..448].copy_from_slice(&data);
            // a substitute's state (0 before kick-off, else 2) and off the pitch
            let state = if mem.read32(slot_at(obj, team, 0) + SLOT_STATE as u32) == 0 { 0 } else { STATE_BENCH };
            v[SLOT_STATE..SLOT_STATE + 4].copy_from_slice(&state.to_be_bytes());
            v[SLOT_ON_PITCH..SLOT_ON_PITCH + 4].fill(0);
            v
        }
    };
    let old = read_slot(mem, target);
    // the slot keeps +28, as the swap 0x8226bf30 leaves it, and its index (+4; an empty one holds 0)
    let index = bench.iter().position(|&s| s == target).map_or(0, |k| 11 + k as u32);
    image[4..8].copy_from_slice(&index.to_be_bytes());
    image[28..32].copy_from_slice(&old[28..32]);
    // the unused substitute it displaces is kept, so he can still come on later (an empty slot: none)
    if word(&old, 0) != u32::MAX {
        reserves.insert((team, word(&old, 0)), old);
    }
    mem.write(target, &image);
    target
}

/// A reserve's virtual slot: the one he is queued in (the menu has swapped his id off the reserve
/// rows by then), else his row's (`slot_get`), or 0.
pub fn reserve_slot(mem: &mut guest::mem::Memory, obj: u32, id: u32, extra: u32, virt: u32) -> u32 {
    if extra == 0 || virt == 0 || id == u32::MAX {
        return 0;
    }
    for team in 0..2 {
        if !reserve_list(mem, obj, team, extra).iter().any(|&(r, _)| r == id) {
            continue;
        }
        let queued = (0..VIRTUAL_PER_TEAM)
            .map(|k| virt + (team * VIRTUAL_PER_TEAM + k) * SLOT_SIZE as u32)
            .find(|&v| mem.read32(v) == id && mem.read32(v + SLOT_STATE as u32) == STATE_QUEUED);
        if let Some(v) = queued {
            return v;
        }
        if let Some(i) = (16..16 + VIRTUAL_ROWS).find(|&i| row_player(mem, obj, team, i, extra).is_some_and(|(r, _)| r == id)) {
            return slot_get(mem, obj, team, i, extra, virt);
        }
    }
    0
}

/// The subs menu's id for each of its rows, i16 by the row's slot index (-1 none), at `*0x8274cc08`:
/// the menu finds its selection and cursor rows again by these ids (0x821903A0), and the X queues
/// the player of the cursor row's slot (0x82190ff8 -> 0x8226b920).
const MENU_ROW_IDS: u32 = 0x8274_CC08;

/// Who virtual row `i` (16..) holds, as (id, pre-match entry or 0 for a displaced substitute): on the
/// team the screen shows, the reserve with the menu's own id for the row (none if that player can't
/// come on), so the screen, the menu's selection and the queue name the same player (#530: every
/// queued substitution brought on the last reserve row's player, whatever row the cursor was on);
/// else `reserve_list`'s order.
fn row_player(mem: &mut guest::mem::Memory, obj: u32, team: u32, i: u32, extra: u32) -> Option<(u32, u32)> {
    let ids = mem.read32(MENU_ROW_IDS);
    if ids != 0 && mem.read32(TEAM_SCREEN_TEAM) == team {
        let id = mem.read16(ids + 2 * i) as i16;
        return (id >= 0).then(|| reserve_list(mem, obj, team, extra).into_iter().find(|&(r, _)| r == id as u32)).flatten();
    }
    reserve_list(mem, obj, team, extra).get((i - 16) as usize).copied()
}

/// The rows past 16 a team screen needs for `team`'s reserves: up to the menu's last row holding one
/// (its table also lists injured players, who can't come on, so a reserve can sit past row 16 +
/// `extra`: 276 at row 20 in the 24 Oct match), else one per reserve.
/// A queued reserve's row counts too (the menu has swapped his id off it).
fn reserve_rows(mem: &mut guest::mem::Memory, obj: u32, team: u32, extra: u32) -> u32 {
    let list = reserve_list(mem, obj, team, extra);
    let ids = mem.read32(MENU_ROW_IDS);
    if ids != 0 && mem.read32(TEAM_SCREEN_TEAM) == team {
        let mut last = (16..16 + VIRTUAL_ROWS).rev().find(|&i| {
            let id = mem.read16(ids + 2 * i) as i16;
            id >= 0 && list.iter().any(|&(r, _)| r == id as u32)
        });
        let virt = VIRTUAL_SLOTS.load(std::sync::atomic::Ordering::Relaxed);
        for k in 0..VIRTUAL_PER_TEAM {
            let v = virt + (team * VIRTUAL_PER_TEAM + k) * SLOT_SIZE as u32;
            let row = mem.read32(v + 4);
            if virt != 0 && mem.read32(v + SLOT_STATE as u32) == STATE_QUEUED && list.iter().any(|&(r, _)| r == mem.read32(v)) && (16..16 + VIRTUAL_ROWS).contains(&row) {
                last = last.max(Some(row));
            }
        }
        if let Some(i) = last {
            return i - 15;
        }
    }
    list.len() as u32
}

fn slot_lookup_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, _pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let lr = c.lr as u32 & !3;
    let extra = BENCH_EXTRA.load(std::sync::atomic::Ordering::Relaxed);
    let (obj, id) = (c.gpr[3] as u32, c.gpr[4] as u32);
    let mut s = slot_lookup(m, obj, id, SUB_LOOKUP_RETS.contains(&lr), extra);
    if s == 0 && UNDO_LOOKUP_RETS.contains(&lr) {
        s = reserve_slot(m, obj, id, extra, VIRTUAL_SLOTS.load(std::sync::atomic::Ordering::Relaxed));
    }
    c.gpr[3] = s as u64;
    // about what the game's scan costs on average (3 instructions per slot, ~16 slots compared)
    *b -= 40;
    Ok(lr)
}

/// The match's slot getter, `slot(obj, team, i) -> slot or 0` (0x8225edc0): `obj + team x 19408 +
/// i x 0x400 + 3008` for team 0-1 and i 0-15, else 0. The team screen's rows read their slots through
/// it (0x8218f750, the row table 0x8218feb8), and so do ~100 other callers that never ask past 15.
pub const SLOT_GET: u32 = 0x8225_EDC0;
/// Guest memory for the reserves' virtual slots, 2 teams x 4 x 0x400 (`Machine::install_blocks`
/// allocates it when `bench` > 5); 0 = none.
pub static VIRTUAL_SLOTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// A team screen's rows past 16 (16..16 + this): room for the reserves behind injured players'
/// rows. Each team has 4 virtual slots (`install_blocks`' 8 KB, part of every save state's small
/// allocations since #661), one per reserve on a row, whatever the row.
pub const VIRTUAL_ROWS: u32 = 16;
const VIRTUAL_PER_TEAM: u32 = 4;

/// The getter's answer with reserves (#530, the UI side's engine half): i = 16..16+extra of a team
/// is a virtual slot at `virt` holding that reserve's image, the pre-match entry's (as `bring_in`
/// makes one: id, the 220 bytes of player data twice, state 0, not on the pitch, index i), so a
/// screen whose row count grows past 16 shows him like a substitute; 0 for an empty entry (-1) or
/// a reserve already in a real slot (brought on: his real slot is his row). i 0-15 is the game's
/// own answer.
pub fn slot_get(mem: &mut guest::mem::Memory, obj: u32, team: u32, i: u32, extra: u32, virt: u32) -> u32 {
    if team > 1 {
        return 0;
    }
    if i < 16 {
        return slot_at(obj, team, i);
    }
    if virt == 0 || i >= 16 + VIRTUAL_ROWS {
        return 0;
    }
    let list = reserve_list(mem, obj, team, extra);
    let slots: Vec<u32> = (0..VIRTUAL_PER_TEAM).map(|k| virt + (team * VIRTUAL_PER_TEAM + k) * SLOT_SIZE as u32).collect();
    // a reserve the subs menu queued keeps his slot as the menu left it (state 6 and its flags),
    // so the menu can see the pending substitution and undo it (#530), until he comes on: the menu
    // swaps the two players' ids in its own row table while the substitution waits, so the row's
    // id is then the player going off; his slot remembers the row (+4)
    let held = |mem: &mut guest::mem::Memory, v: u32| mem.read32(v + SLOT_STATE as u32) == STATE_QUEUED && list.iter().any(|&(r, _)| r == mem.read32(v));
    if let Some(&v) = slots.iter().find(|&&v| held(mem, v) && mem.read32(v + 4) == i) {
        return v;
    }
    let Some((id, entry)) = row_player(mem, obj, team, i, extra) else { return 0 };
    // his own slot, else one whose player is no reserve any more, else one no queued reserve holds
    // (there are at most 4 reserves)
    let own = slots.iter().copied().find(|&v| mem.read32(v) == id);
    let stale = || slots.iter().copied().find(|&v| !list.iter().any(|&(r, _)| r == mem.read32(v)));
    let Some(v) = own.or_else(stale).or_else(|| slots.iter().copied().find(|&v| !held(mem, v))) else { return 0 };
    if held(mem, v) {
        return v;
    }
    let stored = RESERVES.lock().unwrap().as_ref().and_then(|r| r.get(&(team, id)).cloned());
    // a substitute's state: 2 once the match is on (the goalkeeper's slot has 1), else 0
    let state = if mem.read32(slot_at(obj, team, 0) + SLOT_STATE as u32) == 0 { 0 } else { STATE_BENCH };
    let mut image = stored.unwrap_or_else(|| {
        let template = bench_template(mem, obj, team);
        let mut v = read_slot(mem, template);
        let mut data = [0u8; 220];
        mem.read(entry + 8, &mut data);
        v[0..4].copy_from_slice(&id.to_be_bytes());
        v[8..228].copy_from_slice(&data);
        v[228..448].copy_from_slice(&data);
        v[SLOT_STATE..SLOT_STATE + 4].copy_from_slice(&state.to_be_bytes());
        v[SLOT_ON_PITCH..SLOT_ON_PITCH + 4].fill(0);
        v
    });
    image[4..8].copy_from_slice(&i.to_be_bytes());
    mem.write(v, &image);
    v
}

fn slot_get_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, _pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let lr = c.lr as u32 & !3;
    let (extra, virt) = (BENCH_EXTRA.load(std::sync::atomic::Ordering::Relaxed), VIRTUAL_SLOTS.load(std::sync::atomic::Ordering::Relaxed));
    c.gpr[3] = slot_get(m, c.gpr[3] as u32, c.gpr[4] as u32, c.gpr[5] as u32, extra, virt) as u64;
    *b -= 12;
    Ok(lr)
}

/// The native slot getter (the game's own is replaced: it only computes an address).
pub fn slot_get_block_for() -> guest::rec::FuncFn {
    slot_get_block
}

/// The team screen's setup (`0x8218f068`, per side): in the match modes (2, 3, 4) it sets each
/// side's row count to 16 (`li r25,0x10` at `0x8218f134`), else 32 (48 in mode 7).
pub const TEAM_SCREEN_SETUP: u32 = 0x8218_F068;
/// Side 0's row count; side 1's is 0x20 on.
pub const TEAM_SCREEN_ROWS: u32 = 0x8274_CB18;
/// The team the screen shows (0 or 1), the slot getter's `team` in `0x8218f750`.
const TEAM_SCREEN_TEAM: u32 = 0x8274_CAF8;
/// The match globals' pointer (`0x822135c0`); the match object is at +0x50a0.
const MATCH_GLOBALS: u32 = 0x8275_5900;
const OBJ_FROM_GLOBALS: u32 = 0x50A0;

/// Mod `bench` (#530): a match-mode team screen shows a row for every reserve the shown team can
/// still bring on (`reserve_list`, at most `extra`), so they can be picked. Rows past 10 map to
/// themselves (`0x823261e0`), the slot getter answers them (`slot_get`) and the screen already
/// scrolls past 16 (mode 7 has 48 rows).
pub fn team_screen_rows(mem: &mut guest::mem::Memory, extra: u32) {
    let globals = mem.read32(MATCH_GLOBALS);
    let team = mem.read32(TEAM_SCREEN_TEAM);
    if globals == 0 || team > 1 {
        return;
    }
    let shown = reserve_rows(mem, globals + OBJ_FROM_GLOBALS, team, extra);
    for side in 0..2 {
        let a = TEAM_SCREEN_ROWS + side * 0x20;
        if mem.read32(a) == 16 {
            mem.write32(a, 16 + shown);
        }
    }
}

static TEAM_SCREEN_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The team screen's setup with the bench's extra rows after it, the game's setup first.
pub fn team_screen_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = TEAM_SCREEN_GAME.set(game);
    team_screen_block
}

fn team_screen_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let lr = c.lr as u32 & !3;
    let Some(game) = TEAM_SCREEN_GAME.get() else { unreachable!("team_screen_block is only installed through team_screen_block_for") };
    let next = call_to_the_end(c, m, pc, b, *game)?;
    if next == lr {
        team_screen_rows(m, BENCH_EXTRA.load(std::sync::atomic::Ordering::Relaxed));
    }
    Ok(next)
}

/// The team screen's row -> player (`0x8218f750`) and row -> lineup status (`0x82190bc8`, which
/// calls the first). In the match modes the line drawer (`0x821d0008`) asks both by the line on screen
/// (r30) rather than the scrolled row (r22 = scroll + line), so a screen scrolled past 16 showed the
/// first 16 names again; these call sites get the scrolled row.
pub const TEAM_ROW_PLAYER: u32 = 0x8218_F750;
pub const TEAM_ROW_STATUS: u32 = 0x8219_0BC8;
/// Return addresses of the drawer's calls by line: 0x8218f750 at 0x821d00f4, 0x82190bc8 at
/// 0x821d0188 and 0x821d0254.
const TEAM_LINE_CALLS: [u32; 3] = [0x821D_00F8, 0x821D_018C, 0x821D_0258];

static TEAM_ROW_PLAYER_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();
static TEAM_ROW_STATUS_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

pub fn team_row_player_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = TEAM_ROW_PLAYER_GAME.set(game);
    team_row_player_block
}

pub fn team_row_status_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = TEAM_ROW_STATUS_GAME.set(game);
    team_row_status_block
}

fn scrolled_row(c: &mut guest::cpu::Cpu) {
    if TEAM_LINE_CALLS.contains(&(c.lr as u32 & !3)) {
        c.gpr[3] = c.gpr[22] & 0xffff_ffff;
    }
}

fn team_row_player_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let Some(game) = TEAM_ROW_PLAYER_GAME.get() else { unreachable!("installed through team_row_player_block_for") };
    scrolled_row(c);
    game(c, m, pc, b)
}

fn team_row_status_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let Some(game) = TEAM_ROW_STATUS_GAME.get() else { unreachable!("installed through team_row_status_block_for") };
    scrolled_row(c);
    game(c, m, pc, b)
}

static MATCH_BUILD_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The native slot lookup (the game's own is replaced: it only scans).
pub fn slot_lookup_block_for() -> guest::rec::FuncFn {
    slot_lookup_block
}

/// The match builder with the reserves' stored images cleared first, the game's builder behind it.
pub fn match_build_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = MATCH_BUILD_GAME.set(game);
    match_build_block
}

fn match_build_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    *RESERVES.lock().unwrap() = None;
    // no substitution queued in the last match carries over into this one's rows
    let virt = VIRTUAL_SLOTS.load(std::sync::atomic::Ordering::Relaxed);
    if virt != 0 {
        m.write(virt, &[0u8; 2 * VIRTUAL_PER_TEAM as usize * SLOT_SIZE]);
    }
    let Some(game) = MATCH_BUILD_GAME.get() else { unreachable!("match_build_block is only installed through match_build_block_for") };
    game(c, m, pc, b)
}

/// The European Cup's draws (#703), `draw(competition << 32 | round, date)`: 0x821362b0 calls it
/// on each of the diary's schedule entries for competition 0x37 and it switches on the round (2 the
/// qualifying draw, 42 the 8 groups of 4 by 0x82134480, 9 the last 16, 0x1c/0x20/0x25 the rest).
pub const EUROPEAN_CUP_DRAW: u32 = 0x8213_94A8;
const LEAGUE_ROUND: u32 = 42;
/// The straight entrants (16 uids, 0x8213abc8's copy of 0x8213a918's set) and the qualifying round's
/// winners (16, collected by 0x8213a718 after round 3).
const DIRECT_ENTRANTS: u32 = 0x8274_1520;
const QUALIFIERS: u32 = 0x8274_14E0;
/// The diary: [pointer, size] (the loader 0x822e5d48 sizes it from the file).
const DIARY: u32 = 0x8274_0400;

/// The league phase is on (the 2024+ European Cup; set with the diary's league-phase shape).
pub static LEAGUE_PHASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Each league's European places in all (group stage + qualifying + UEFA Cup) and its table's top
/// 10 uids, read just before the draw (`read_league_tables`): the clubs below the places refill the
/// UEFA Cup slots the league phase's extras leave (`uefa_extras`).
pub static LEAGUE_TABLES: std::sync::RwLock<[(u32, [u32; 10]); 8]> = std::sync::RwLock::new([(0, [0; 10]); 8]);

/// The game's places record for a league, `places(nation, 0, record)` (0x82138b50): the hard-coded
/// counts at `+0`, `+4`, `+8` and the table's top 10 at `+0x28` (see `EUROPE_PLACES`).
const PLACES_RECORD: u32 = 0x8213_8B50;
/// The return address `read_league_tables` gives the game's function: no code there, so `call`
/// stops at it when the function returns.
const CALL_RETURN: u32 = 4;
/// The instructions the 8 places records may take (one is ~1,000-10,000).
const PLACES_GRANT: i64 = 2_000_000;

/// `LEAGUE_TABLES` from the game's places records of the 8 leagues, built in `record` (0x50 bytes)
/// by calling 0x82138b50; the mod's `europe_places` counts replace the game's, as `europe_places`
/// does. A league whose call doesn't come back keeps no table, nor do the ones after it (their
/// extras' slots stay empty).
fn read_league_tables(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, b: &mut i64, record: u32) -> Result<(), guest::cpu::Stop> {
    let (gpr, lr) = (c.gpr, c.lr);
    // a scheduling slice (20,000) is too short for the 8 records: the calls run on a grant that is
    // charged back after them, as the hooks' own work is
    *b += PLACES_GRANT;
    let mut tables = [(0, [0; 10]); 8];
    for (nation, table) in tables.iter_mut().enumerate() {
        m.write(record, &[0; 0x50]);
        c.gpr[3] = nation as u64;
        c.gpr[4] = 0;
        c.gpr[5] = record as u64;
        c.lr = CALL_RETURN as u64;
        let back = guest::rec::call(c, m, PLACES_RECORD, b);
        if !matches!(back, Ok(CALL_RETURN)) {
            eprintln!("league phase: no table for league {nation} and on");
            back?;
            break;
        }
        let places = EUROPE_PLACES_TABLE.read().unwrap().get(nation).map(|p| p.iter().sum()).unwrap_or_else(|| [0, 4, 8].iter().map(|&o| m.read32(record + o)).sum());
        *table = (places, std::array::from_fn(|k| m.read32(record + 0x28 + 4 * k as u32)));
    }
    *b -= PLACES_GRANT;
    (c.gpr, c.lr) = (gpr, lr);
    *LEAGUE_TABLES.write().unwrap() = tables;
    Ok(())
}

/// The 4 clubs the league phase takes beyond the game's 32 (uids), filled before its draw.
pub static LEAGUE_EXTRAS: std::sync::RwLock<Vec<u32>> = std::sync::RwLock::new(Vec::new());

/// The game's straight entrants and qualifiers, without empty slots (a career whose last season
/// has no European Cup winner leaves the holder's slot at 0).
fn entrant_sets(mem: &mut guest::mem::Memory) -> (Vec<u32>, Vec<u32>) {
    let mut set = |at: u32| -> Vec<u32> { (0..16).map(|k| mem.read32(at + 4 * k)).filter(|&u| u != 0 && u != u32::MAX).collect() };
    (set(DIRECT_ENTRANTS), set(QUALIFIERS))
}

/// The league phase's 36 entrants in pot order: the straight entrants (holders first, as the
/// game's set has them), the extras, then the qualifiers; None unless all 36 are there, distinct.
pub fn league_entrants(mem: &mut guest::mem::Memory, extras: &[u32]) -> Option<[u32; 36]> {
    let (mut all, qualifiers) = entrant_sets(mem);
    all.extend_from_slice(extras);
    all.extend(qualifiers);
    let mut seen = all.clone();
    seen.sort_unstable();
    seen.dedup();
    if all.len() != 36 || seen.len() != 36 || all.iter().any(|&u| u == 0 || u == u32::MAX) {
        return None;
    }
    all.try_into().ok()
}

/// The league phase's draw (`lma_data::league::draw`, seeded with `seed`) written into the game's
/// diary, whose round-42 days `lma_data::diary::league_phase` shaped (8 of 18 blanks).
pub fn league_phase_draw(mem: &mut guest::mem::Memory, seed: u64) -> Result<(), String> {
    let mut extras = LEAGUE_EXTRAS.read().unwrap().clone();
    let mut taken = Vec::new();
    if extras.is_empty() {
        // 4, and one more for each empty straight place (no holder)
        let (direct, qualifiers) = entrant_sets(mem);
        taken = uefa_extras(mem, 36usize.saturating_sub(direct.len() + qualifiers.len()));
        extras = taken.iter().map(|&(_, u, _)| u).collect();
    }
    let clubs = league_entrants(mem, &extras).ok_or("not 36 distinct entrants")?;
    let nations: [u16; 36] = std::array::from_fn(|k| club_country(mem, clubs[k]) as u16);
    let days = lma_data::league::draw(&nations, seed).ok_or("no draw keeps the nation rules")?;
    let days: Vec<Vec<(u16, u16)>> = days.iter().map(|d| d.iter().map(|&(h, a)| (clubs[h] as u16, clubs[a] as u16)).collect()).collect();
    let (at, size) = (mem.read32(DIARY), mem.read32(DIARY + 4) as usize);
    if at == 0 || size == 0 {
        return Err("no diary".into());
    }
    let mut diary = vec![0u8; size];
    mem.read(at, &mut diary);
    lma_data::diary::fill_league_phase(&mut diary, guest::rec::base_year() as i32, &days).map_err(|e| e.to_string())?;
    mem.write(at, &diary);
    // the extras play in the league phase, not the UEFA Cup (its first round is drawn the next day):
    // their leagues' next clubs take their places, so the UEFA Cup keeps its 80 and its groups 40
    for (slot, uid, next) in taken {
        mem.write32(slot, next);
        eprintln!("league phase: {uid:#x} from the UEFA Cup, {next:#x} in its place");
    }
    Ok(())
}

/// The UEFA Cup's first-round entrants (64 uids, 0x8213a918's third set, copied by 0x8213abc8); the
/// European Cup's round-42 draw comes the day before that round's draw in every disc season.
const UEFA_FIRST_ROUND: u32 = 0x8274_13E0;

/// The `n` extras (4, or more when a straight place is empty) for the league phase when the mod
/// names none: the UEFA Cup's first-round set is in nation order with each nation's clubs by place,
/// so the first club of each of its first `n` countries is that country's best-placed club below the European Cup's places. Returns (slot, uid,
/// the club that takes the slot once the draw is in: the next in the uid's league table below its
/// European places and in no European set, from `LEAGUE_TABLES`; 0, an empty slot, when none).
fn uefa_extras(mem: &mut guest::mem::Memory, n: usize) -> Vec<(u32, u32, u32)> {
    let mut out: Vec<(u32, u32, u32)> = Vec::new();
    let mut countries = Vec::new();
    for k in 0..64 {
        let slot = UEFA_FIRST_ROUND + 4 * k;
        let uid = mem.read32(slot);
        if uid == 0 || uid == u32::MAX {
            continue;
        }
        let country = club_country(mem, uid);
        if !countries.contains(&country) {
            countries.push(country);
            out.push((slot, uid, 0));
            if out.len() == n {
                break;
            }
        }
    }
    // every European set of the season (0x8213abc8's): the UEFA Cup's group, first round and
    // qualifier-loser sets, the thirds, and the European Cup's
    let mut european: Vec<u32> = (0..(EUROPEAN_SETS_END - EUROPEAN_SETS) / 4).map(|k| mem.read32(EUROPEAN_SETS + 4 * k)).collect();
    let tables = *LEAGUE_TABLES.read().unwrap();
    for (_, uid, next) in out.iter_mut() {
        let Some(&(places, top)) = tables.iter().find(|(_, top)| top.contains(uid)) else { continue };
        let below = top.iter().position(|u| u == uid).unwrap_or(0).max(places as usize);
        if let Some(&club) = top[below..].iter().find(|&&u| u != 0 && u != u32::MAX && !european.contains(&u)) {
            *next = club;
            european.push(club);
        }
    }
    out
}

/// The season's European sets, `0x827412e0..0x827415e0` (cleared and filled by 0x8213abc8).
const EUROPEAN_SETS: u32 = 0x8274_12E0;
const EUROPEAN_SETS_END: u32 = 0x8274_15E0;

/// The play-off's first-leg round (a two-legged pair, `lma_data::diary::PLAYOFF`) and the last 16's.
const PLAYOFF_ROUND: u32 = lma_data::diary::PLAYOFF[0] as u32;
const LAST_16_ROUND: u32 = 9;
/// The game's knockout writer, `write(competition, round, list, shuffle)` (0x82134350): a list of
/// clubs (word 0 the mode, 1 = consecutive (home, away) pairs; words 1.. the clubs; +0x404 the
/// count), its first leg and then the second with home and away swapped.
const KNOCKOUT_WRITER: u32 = 0x8213_4350;
/// Guest memory for that list (0x408 bytes), allocated with the hook.
pub static LEAGUE_LIST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// What the group stage leaves: the 8 thirds for the UEFA Cup's last 32 (0x82138780 reads them),
/// the 8 winners (0x827414e0) and the 8 runners-up (0x82741500) for the last 16.
const UEFA_THIRDS: u32 = 0x8274_1380;
const RUNNERS_UP: u32 = 0x8274_1500;

/// The league phase's clubs and table from the game's diary: the 36 uids in seeding order (as
/// drawn: the order they first appear), and the table's order (`lma_data::league::table`), with
/// how many of the 144 games are played. Recomputed from the diary each time, so a save state
/// needs nothing else.
pub fn league_order(mem: &mut guest::mem::Memory) -> Result<(Vec<u32>, usize), String> {
    let (clubs, rows, played) = league_rows(mem)?;
    Ok((rows.iter().map(|r| clubs[r.club]).collect(), played))
}

/// The league phase's clubs (by first appearance in the diary), their table and the games played.
fn league_rows(mem: &mut guest::mem::Memory) -> Result<(Vec<u32>, Vec<lma_data::league::Row>, usize), String> {
    let (at, size) = (mem.read32(DIARY), mem.read32(DIARY + 4) as usize);
    if at == 0 || size == 0 {
        return Err("no diary".into());
    }
    let mut diary = vec![0u8; size];
    mem.read(at, &mut diary);
    let games = lma_data::diary::league_phase_results(&diary, guest::rec::base_year() as i32).map_err(|e| e.to_string())?;
    let mut clubs: Vec<u32> = Vec::with_capacity(36);
    for &(h, a, _) in &games {
        for u in [h as u32, a as u32] {
            if !clubs.contains(&u) {
                clubs.push(u);
            }
        }
    }
    if clubs.len() != lma_data::league::CLUBS {
        return Err(format!("{} clubs in the league phase", clubs.len()));
    }
    let index = |u: u16| clubs.iter().position(|&c| c == u as u32).unwrap();
    let scores: Vec<lma_data::league::Score> = games.iter().filter_map(|&(h, a, s)| s.map(|(hg, ag)| (index(h), index(a), hg, ag))).collect();
    let rows = lma_data::league::table(&scores);
    Ok((clubs, rows, scores.len()))
}

/// The cup screens' standings store (0x820df550's, at [0x8273d6ac]): 6 matchdays x 8 groups x 4
/// places of 8 bytes for the European Cup's round 42, UEFA's groups right behind it.
const STANDINGS: u32 = 0x8273_D6AC;
const STANDINGS_FIRST: u32 = 0x7243;
const STANDINGS_MATCHDAYS: usize = 6;

/// One standings entry as 0x82135930 packs it: uid, won, drawn, lost, scored, conceded, points.
fn standings_entry(uid: u32, r: &lma_data::league::Row) -> u64 {
    let b = |v: u16| v.min(255) as u64;
    (uid as u64 & 0xffff) << 48 | b(r.won as u16) << 40 | b(r.drawn as u16) << 32 | b(r.lost as u16) << 24 | b(r.scored) << 16 | b(r.conceded) << 8 | b(r.points())
}

/// The table into the screens' store: places 1-32 as the 8 groups of 4 the screens show (A = 1st-4th,
/// ..., H = 29th-32nd), on the matchday just played and every one after it, as the game does. The
/// store has 6 matchdays: days 7 and 8 go into the 6th.
fn league_standings(mem: &mut guest::mem::Memory, clubs: &[u32], rows: &[lma_data::league::Row], played: usize) {
    let base = mem.read32(STANDINGS);
    if base == 0 || played == 0 {
        return;
    }
    let day = (played.div_ceil(lma_data::league::CLUBS / 2) - 1).min(STANDINGS_MATCHDAYS - 1);
    for md in day..STANDINGS_MATCHDAYS {
        for (place, r) in rows.iter().take(32).enumerate() {
            let at = (md as u32 * 8 + place as u32 / 4) * 4 + place as u32 % 4 + STANDINGS_FIRST;
            mem.write64(base + 8 * at, standings_entry(clubs[r.club], r));
        }
    }
}

/// The final table's order, once all 144 games are played.
fn final_order(mem: &mut guest::mem::Memory) -> Result<Vec<u32>, String> {
    let (order, played) = league_order(mem)?;
    if played != lma_data::league::CLUBS * lma_data::league::DAYS / 2 {
        return Err(format!("{played} of 144 league games played"));
    }
    Ok(order)
}

/// The play-off: 9th-16th against 17th-24th (`lma_data::league::playoff`), as (first-leg home, away).
fn playoff_ties(mem: &mut guest::mem::Memory, seed: u64) -> Result<Vec<(u32, u32)>, String> {
    let order = final_order(mem)?;
    let idx: Vec<usize> = (0..order.len()).collect();
    Ok(lma_data::league::playoff(&idx, seed).iter().map(|t| (order[t.first], order[t.second])).collect())
}

/// The last 16: the top 8 against the play-off's winners (in 0x827414e0, collected by the game's
/// round-done code), by bracket (`lma_data::league::last_16`).
fn last_16_ties(mem: &mut guest::mem::Memory, seed: u64) -> Result<Vec<(u32, u32)>, String> {
    let order = final_order(mem)?;
    let idx: Vec<usize> = (0..order.len()).collect();
    // the brackets come from the places alone, whatever the play-off's draw within them was
    let ties = lma_data::league::playoff(&idx, 0);
    let won: Vec<u32> = (0..16).map(|k| mem.read32(QUALIFIERS + 4 * k)).filter(|&u| u != 0).collect();
    let mut winners = Vec::with_capacity(8);
    for t in &ties {
        let (a, b) = (order[t.first], order[t.second]);
        match (won.contains(&a), won.contains(&b)) {
            (true, false) => winners.push(t.first),
            (false, true) => winners.push(t.second),
            _ => return Err(format!("the play-off tie {a:#x} v {b:#x} has no single winner in 0x827414e0")),
        }
    }
    Ok(lma_data::league::last_16(&idx, &ties, &winners, seed).iter().map(|t| (order[t.first], order[t.second])).collect())
}

/// The ties as the knockout writer's list: mode 1, the clubs in pairs, the count.
fn write_tie_list(mem: &mut guest::mem::Memory, list: u32, ties: &[(u32, u32)]) {
    mem.write32(list, 1);
    for (k, &(h, a)) in ties.iter().enumerate() {
        mem.write32(list + 4 + 8 * k as u32, h);
        mem.write32(list + 8 + 8 * k as u32, a);
    }
    mem.write32(list + 0x404, 2 * ties.len() as u32);
}

/// The group stage's end (0x8213a2d0, after each round-42 matchday from 0x8213a718): the game builds
/// 8 group tables. With the league phase there are no groups: after the last matchday the UEFA Cup
/// gets the 25th-32nd (its last 32 still takes 8 from the European Cup), the winners' lists are
/// cleared for the play-off's winners and the straight entrants' list as the game clears it.
pub const GROUP_END: u32 = 0x8213_A2D0;

pub fn league_phase_end(mem: &mut guest::mem::Memory) -> Result<bool, String> {
    let (clubs, rows, played) = league_rows(mem)?;
    league_standings(mem, &clubs, &rows, played);
    let order: Vec<u32> = rows.iter().map(|r| clubs[r.club]).collect();
    if played < lma_data::league::CLUBS * lma_data::league::DAYS / 2 {
        return Ok(false);
    }
    for k in 0..8 {
        mem.write32(UEFA_THIRDS + 4 * k, order[24 + k as usize]);
    }
    for k in 0..16 {
        mem.write32(QUALIFIERS + 4 * k, 0); // with RUNNERS_UP, the 16 words the play-off's winners fill
        mem.write32(DIRECT_ENTRANTS + 4 * k, 0);
    }
    debug_assert_eq!(RUNNERS_UP, QUALIFIERS + 0x20);
    Ok(true)
}

/// The standings store's reader, 0x820df370(matchday, group, place, comp, round).
pub const STANDINGS_READ: u32 = 0x820D_F370;

static STANDINGS_READ_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The store's reader with the league phase's matchdays 7 and 8 read from the 6th, where
/// `league_standings` keeps them (the game's reader answers an empty entry past the 6th).
pub fn standings_read_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = STANDINGS_READ_GAME.set(game);
    standings_read_block
}

fn standings_read_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    if c.gpr[6] as u32 == 0x37 && c.gpr[7] as u32 == LEAGUE_ROUND && (c.gpr[3] as u32 as usize) >= STANDINGS_MATCHDAYS {
        c.gpr[3] = STANDINGS_MATCHDAYS as u64 - 1;
    }
    let Some(game) = STANDINGS_READ_GAME.get() else { unreachable!("standings_read_block is only installed through standings_read_block_for") };
    game(c, m, pc, b)
}

static GROUP_END_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The group stage's end with the league phase's own, the game's behind it when the diary has no
/// league phase.
pub fn group_end_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = GROUP_END_GAME.set(game);
    group_end_block
}

fn group_end_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    if LEAGUE_PHASE.load(std::sync::atomic::Ordering::Relaxed) {
        match league_phase_end(m) {
            Ok(_) => {
                *b -= 5_000;
                return Ok(c.lr as u32 & !3);
            }
            Err(e) => eprintln!("league phase end: {e}; the game's group tables instead"),
        }
    }
    let Some(game) = GROUP_END_GAME.get() else { unreachable!("group_end_block is only installed through group_end_block_for") };
    game(c, m, pc, b)
}

static EUROPEAN_CUP_DRAW_GAME: std::sync::OnceLock<guest::rec::FuncFn> = std::sync::OnceLock::new();

/// The European Cup's draws with the league phase's round 42 native, the game's draws behind it.
pub fn european_cup_draw_block_for(game: guest::rec::FuncFn) -> guest::rec::FuncFn {
    let _ = EUROPEAN_CUP_DRAW_GAME.set(game);
    european_cup_draw_block
}

fn european_cup_draw_block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let round = c.gpr[3] as u32;
    if LEAGUE_PHASE.load(std::sync::atomic::Ordering::Relaxed) {
        // seeded from the guest clock: the same in a deterministic run, another league each career
        let seed = c.timebase ^ 0x37;
        if round == LEAGUE_ROUND {
            let list = LEAGUE_LIST.load(std::sync::atomic::Ordering::Relaxed);
            if list != 0 {
                read_league_tables(c, m, b, list)?;
            }
            match league_phase_draw(m, seed) {
                Ok(()) => {
                    *b -= 20_000;
                    return Ok(c.lr as u32 & !3);
                }
                Err(e) => eprintln!("league phase: {e}; the game's group draw instead"),
            }
        }
        if round == PLAYOFF_ROUND || round == LAST_16_ROUND {
            let list = LEAGUE_LIST.load(std::sync::atomic::Ordering::Relaxed);
            let ties = if round == PLAYOFF_ROUND { playoff_ties(m, seed) } else { last_16_ties(m, seed) };
            match ties {
                Ok(ties) if list != 0 => {
                    // the game's knockout writer, as the draw's own tail: the pairs as given (mode 1,
                    // no shuffle), both legs
                    write_tie_list(m, list, &ties);
                    c.gpr[3] = 0x37;
                    c.gpr[4] = round as u64;
                    c.gpr[5] = list as u64;
                    c.gpr[6] = 0;
                    *b -= 2_000;
                    return Ok(KNOCKOUT_WRITER);
                }
                Ok(_) => eprintln!("league phase: no list memory for round {round}"),
                Err(e) => eprintln!("league phase: round {round}: {e}"),
            }
        }
    }
    let Some(game) = EUROPEAN_CUP_DRAW_GAME.get() else { unreachable!("european_cup_draw_block is only installed through european_cup_draw_block_for") };
    game(c, m, pc, b)
}

/// Counts the call (`Machine::hook_calls`) and declines.
fn observe(m: &mut Machine) -> bool {
    let pc = m.cpu.pc;
    *m.hook_calls.entry(pc).or_default() += 1;
    false
}

/// The mod hooks on a real Machine: what they read and write in the player record (11 Oct).
#[cfg(test)]
mod mod_hook_tests {
    use super::*;

    /// The tests that fill `RESERVES` (a global) run one at a time: the test threads raced on it
    /// (21 Oct, CI: the lookup test found no reserve while the getter test had just cleared it).
    static RESERVES_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The sponsorship accessors with a unit of GBP 100: a real shirt deal and a hoarding deal go in
    /// and come back, the fields are capped, and manager -2 means the current manager.
    /// wage_budgets.csv raises a club's computed wage budget to its floor, never lowers it, and
    /// leaves a club the file doesn't list alone.
    #[test]
    fn wage_budget_has_the_clubs_floor() {
        let mut m = machine();
        const MAP: u32 = 0x4400_0000;
        m.mem.write32(CLUB_UID_MAP, MAP);
        m.mem.write16(MAP + 0x123 * 2, 0); // uid 0x123 -> record 0
        m.mem.write16(MAP + 0x77 * 2, 5);
        m.wage_budgets.insert(0, 4_125_000);
        m.cpu.gpr[3] = 0x123;
        m.cpu.gpr[4] = 3_400_000;
        assert!(!wage_budget_set(&mut m), "the setter still stores it");
        assert_eq!(m.cpu.gpr[4], 4_125_000, "raised to the floor");
        m.cpu.gpr[4] = 5_000_000;
        wage_budget_set(&mut m);
        assert_eq!(m.cpu.gpr[4], 5_000_000, "a higher computed budget stays");
        m.cpu.gpr[3] = 0x77;
        m.cpu.gpr[4] = 100_000;
        wage_budget_set(&mut m);
        assert_eq!(m.cpu.gpr[4], 100_000, "a club the file doesn't list keeps the game's budget");
    }

    /// The getter answers the floor from the first day (the setter only runs weekly), keeps a higher
    /// stored budget, and leaves an unlisted club or a missing club table to the game.
    #[test]
    fn wage_budget_getter_has_the_clubs_floor() {
        let mut m = machine();
        const MAP: u32 = 0x4400_0000;
        const CLUBS: u32 = 0x4500_0000;
        m.mem.write32(CLUB_UID_MAP, MAP);
        m.mem.write16(MAP + 0x123 * 2, 2); // uid 0x123 -> record 2
        m.mem.write16(MAP + 0x77 * 2, 5);
        m.wage_budgets.insert(2, 4_125_000);
        m.cpu.gpr[3] = 0x123;
        assert!(!wage_budget_get(&mut m), "no club table yet: the game's getter");
        m.mem.write32(CLUB_TABLE, CLUBS);
        m.mem.write32(CLUBS + 2 * CLUB_STRIDE + 0xb0, 350_000); // the database's 2006 budget
        assert!(wage_budget_get(&mut m));
        assert_eq!(m.cpu.gpr[3], 4_125_000, "the floor, before any weekly recalculation");
        m.mem.write32(CLUBS + 2 * CLUB_STRIDE + 0xb0, 6_000_000);
        m.cpu.gpr[3] = 0x123;
        assert!(wage_budget_get(&mut m));
        assert_eq!(m.cpu.gpr[3], 6_000_000, "a higher stored budget stays");
        m.cpu.gpr[3] = 0x77;
        assert!(!wage_budget_get(&mut m), "a club the file doesn't list: the game's getter");
    }

    /// With sponsors.csv the managed club's shirt deal is its real one per week, whatever the game
    /// offered; a club the file doesn't list keeps the game's amount.
    #[test]
    fn sponsors_csv_sets_the_managed_clubs_shirt_deal() {
        let mut m = machine();
        m.sponsor_unit = 100;
        let blocks = 0x4200_0000u32;
        m.mem.write32(MANAGER_BLOCKS, blocks);
        const MAP: u32 = 0x4300_0000;
        m.mem.write32(CLUB_UID_MAP, MAP);
        m.mem.write16(MAP + 0x123 * 2, 0); // uid 0x123 -> record 0 (Arsenal)
        m.mem.write16(MAP + 0x77 * 2, 5); // uid 0x77 -> record 5
        m.mem.write16(blocks + 0x564, 0x123); // manager 0 manages Arsenal
        m.sponsors.insert(0, (50_000_000, 20_000_000));
        m.cpu.gpr[3] = 0;
        m.cpu.gpr[4] = 36_400;
        assert!(sponsor_shirt_set(&mut m));
        assert_eq!(m.mem.read32(blocks + 0x3cd0) >> 18, 9_615, "GBP 50m a season = 961,538 a week = 9,615 units of 100");
        m.mem.write16(blocks + 0x564, 0x77);
        m.cpu.gpr[3] = 0;
        m.cpu.gpr[4] = 36_400;
        sponsor_shirt_set(&mut m);
        assert_eq!(m.mem.read32(blocks + 0x3cd0) >> 18, 364, "a club the file doesn't list keeps the game's amount");
        // the weekly sponsorship credit isn't scaled again; a hoarding deal takes the income scale
        m.income_scale = 4.0;
        assert!(INCOME_CALLERS.contains(&SPONSORSHIP_CREDIT) && INCOME_STATEMENTS.contains(&SPONSORSHIP_STATEMENT), "the credit's real sites");
        m.cpu.lr = SPONSORSHIP_CREDIT as u64;
        m.cpu.gpr[4] = 1_000_000;
        add_funds(&mut m);
        assert_eq!(m.cpu.gpr[4], 1_000_000, "real sponsorship: no x4");
        m.cpu.gpr[3] = 0;
        m.cpu.gpr[4] = 0; // slot 0
        m.cpu.gpr[5] = 10_250;
        sponsor_hoarding_set(&mut m);
        assert_eq!((m.mem.read32(blocks + 0x44e4 * 8) >> 12) & 0x7ff, 410, "GBP 10,250 x 4 = 41,000 a week = 410 units of 100");
    }

    #[test]
    fn sponsor_deals_in_the_mod_unit() {
        let mut m = machine();
        m.sponsor_unit = 100;
        let blocks = 0x4100_0000u32;
        m.mem.write32(MANAGER_BLOCKS, blocks);
        m.mem.write32(MANAGER_CUR, 1);
        // shirt: manager 0, GBP 960,000 a week (Arsenal)
        m.cpu.gpr[3] = 0;
        m.cpu.gpr[4] = 960_000;
        assert!(sponsor_shirt_set(&mut m));
        assert_eq!(m.mem.read32(blocks + 0x3cd0) >> 18, 9600, "960,000 / 100 in bits 18..");
        m.cpu.gpr[3] = 0;
        assert!(sponsor_shirt_get(&mut m));
        assert_eq!(m.cpu.gpr[3], 960_000);
        // the field is capped, the low bits (years, sponsor) kept
        m.mem.write32(blocks + 0x3cd0, 0x0002_0000 | 3 << 7);
        m.cpu.gpr[3] = 0; // the getter left its result in r3
        m.cpu.gpr[4] = 5_000_000;
        assert!(sponsor_shirt_set(&mut m));
        assert_eq!(m.mem.read32(blocks + 0x3cd0), 0x3fff << 18 | 0x0002_0000 | 3 << 7);
        // hoarding slot 3 of the current manager (index -2 -> MANAGER_CUR = 1)
        m.cpu.gpr[3] = (-2i64) as u64;
        m.cpu.gpr[4] = 3;
        m.cpu.gpr[5] = 150_000;
        assert!(sponsor_hoarding_set(&mut m));
        let a = blocks + MANAGER_STRIDE + (3 + 0x44e4) * 8;
        assert_eq!((m.mem.read32(a) >> 12) & 0x7ff, 1500);
        m.cpu.gpr[3] = (-2i64) as u64;
        m.cpu.gpr[4] = 3;
        assert!(sponsor_hoarding_get(&mut m));
        assert_eq!(m.cpu.gpr[3], 150_000);
        m.cpu.gpr[4] = 20;
        assert!(!sponsor_hoarding_get(&mut m), "slot out of range: the game's own code runs");
        // totals: manager 1 (current) has deals (block + 0x3cce != 0), hoardings 0 and 1 point at slot 3
        let b1 = blocks + MANAGER_STRIDE;
        m.mem.write8(b1 + 0x3cce, 1);
        m.mem.write16(b1 + HOARDING_SLOT_MAP, 3);
        m.mem.write16(b1 + HOARDING_SLOT_MAP + 2, 3);
        for i in 2..HOARDINGS {
            m.mem.write16(b1 + HOARDING_SLOT_MAP + i * 2, 0xffff);
        }
        m.cpu.gpr[3] = (-2i64) as u64;
        assert!(sponsor_hoarding_total(&mut m));
        assert_eq!(m.cpu.gpr[3], 300_000, "two hoardings on the 150,000 deal");
        m.cpu.gpr[3] = 1;
        m.cpu.gpr[4] = 250_000;
        assert!(sponsor_shirt_set(&mut m));
        m.cpu.gpr[3] = 1;
        assert!(sponsor_total(&mut m));
        assert_eq!(m.cpu.gpr[3], 550_000, "shirt 250,000 + hoardings 300,000");
        m.mem.write8(b1 + 0x3cce, 0);
        m.cpu.gpr[3] = 1;
        assert!(sponsor_hoarding_total(&mut m) && m.cpu.gpr[3] == 0, "no deals yet: nothing");
    }

    const TABLE: u32 = 0x4000_0000;

    fn machine() -> Machine {
        let x = xex::Xex { module_flags: 0, entry_point: 0x8200_0000, image_base: 0x8200_0000, image: vec![0; 0x1000], libraries: vec![], used_devkit_key: false, resources: vec![] };
        let dir = std::env::temp_dir();
        let mut m = Machine::new(&x, &dir, &dir);
        m.mem.write32(PLAYER_TABLE, TABLE);
        m
    }

    fn rec(idx: u32) -> u32 {
        TABLE + idx * 0x12c
    }

    #[test]
    fn the_league_phase_blocks_are_never_called_directly() {
        // a same-part direct call skips the block table, so a block swap there would never run
        for a in [EUROPEAN_CUP_DRAW, GROUP_END, STANDINGS_READ] {
            assert!(ADDRESSES.contains(&a), "{a:#x} is not in guest::hooks::ADDRESSES");
        }
    }

    /// The league phase's round-42 draw on a machine: 16 straight entrants, 16 qualifiers and 4
    /// extras of 9 countries, a diary of 8 league days of 18 blanks; every day gets 18 games of the
    /// 36, each club once, never two of a country. Played out: the end gives the UEFA Cup the
    /// 25th-32nd, the play-off is 17th-24th v 9th-16th, the last 16 the top 8 v its winners. A diary
    /// without the shape is refused untouched.
    #[test]
    fn the_league_phase_runs_from_the_draw_to_the_last_16() {
        let mut m = machine();
        const MAP: u32 = 0x4100_0000;
        const RECS: u32 = 0x4110_0000;
        const STADIUMS: u32 = 0x4120_0000;
        const DIARY_AT: u32 = 0x4130_0000;
        m.mem.write32(CLUB_UID_MAP, MAP);
        m.mem.write32(CLUB_RECORDS, RECS);
        m.mem.write32(STADIUM_RECORDS, STADIUMS);
        // club uid 101 + k: record k, stadium k, country k % 9
        let uids: Vec<u32> = (0..36).map(|k| 101 + k).collect();
        for (k, &u) in uids.iter().enumerate() {
            m.mem.write16(MAP + u * 2, k as u16);
            m.mem.write32(RECS + k as u32 * 0x3d8 + 4, (k as u32) << 21);
            m.mem.write32(STADIUMS + k as u32 * 0x20 + 0xc, ((k as u32 % 9) + 1) << 15);
        }
        for k in 0..16 {
            m.mem.write32(DIRECT_ENTRANTS + 4 * k, uids[k as usize]);
            m.mem.write32(QUALIFIERS + 4 * k, uids[20 + k as usize]);
        }
        // the 4 extras come from the UEFA Cup's first round: its set in country order with each
        // country's clubs by place, so the first of each of its first 4 countries (clubs 16-19);
        // 137 and 138 are second clubs of clubs 16's and 17's countries and stay
        LEAGUE_EXTRAS.write().unwrap().clear();
        for (k, like) in [(36u32, 16u32), (37, 17)] {
            m.mem.write16(MAP + (101 + k) * 2, k as u16);
            m.mem.write32(RECS + k * 0x3d8 + 4, like << 21);
        }
        let uefa_set = [uids[16], 137, uids[17], uids[18], 138, uids[19]];
        for (k, &u) in uefa_set.iter().enumerate() {
            m.mem.write32(UEFA_FIRST_ROUND + 4 * k as u32, u);
        }
        // club 16's league: 5 places, club 16 5th, 137 (in the UEFA Cup) 6th, 140 7th: 140 takes
        // club 16's slot; club 17's league has no club below it, so its slot stays empty
        *LEAGUE_TABLES.write().unwrap() = [(0, [0; 10]); 8];
        LEAGUE_TABLES.write().unwrap()[0] = (5, [1, 2, 3, 4, uids[16], 137, 140, 0, 0, 0]);
        LEAGUE_TABLES.write().unwrap()[1] = (5, [5, 6, 7, 8, uids[17], 0, 0, 0, 0, 0]);
        assert_eq!(club_country(&mut m.mem, 105), 5);
        assert_eq!(club_country(&mut m.mem, 9000), 0x33, "unmapped");
        // the diary: the start, 8 Wednesdays of round 42 (18 blanks each), the end
        let year = guest::rec::base_year() as i32;
        let pack = |(y, mo, d): (i32, u32, u32)| -> [u8; 2] { ((d as u16) << 11 | ((mo - 1) as u16) << 7 | ((y - year) as u16) << 2).to_be_bytes() };
        let rec = |date, kind: u8, n: u8, body: &[u8]| -> Vec<u8> {
            let mut r = pack(date).to_vec();
            r.extend_from_slice(&[n, kind]);
            r.extend_from_slice(&((6 + body.len()) as u16).to_be_bytes());
            r.extend_from_slice(body);
            r
        };
        let mut diary = rec((year, 7, 1), 2, 1, &[0xff, 0xff]);
        let days = [(year, 9, 13), (year, 9, 27), (year, 10, 18), (year, 11, 1), (year, 11, 22), (year, 12, 6), (year + 1, 1, 17), (year + 1, 1, 24)];
        for d in days {
            let mut body = vec![55, 42, 18, 0];
            for _ in 0..18 {
                body.extend_from_slice(&[0, 0, 0, 0, 0xff, 0]);
            }
            diary.extend(rec(d, 0, 1, &body));
        }
        diary.extend(rec((year + 1, 6, 30), 3, 1, &[0, 0, 0xff, 0xff]));
        m.mem.write32(DIARY, DIARY_AT);
        m.mem.write32(DIARY + 4, diary.len() as u32);
        m.mem.write(DIARY_AT, &diary);
        league_phase_draw(&mut m.mem, 5).unwrap();
        assert_eq!((0..6).map(|k| m.mem.read32(UEFA_FIRST_ROUND + 4 * k)).collect::<Vec<_>>(), [140, uefa_set[1], 0, 0, uefa_set[4], 0], "the extras left the UEFA Cup, their leagues' next clubs in");
        let mut out = vec![0u8; diary.len()];
        m.mem.read(DIARY_AT, &mut out);
        let recs = lma_data::diary::day_records(&out, year).unwrap();
        let mut days_seen = 0;
        for r in recs.iter().filter(|r| r.kind == 0) {
            let mut once = std::collections::HashSet::new();
            for g in 0..18 {
                let f = &r.bytes[10 + 6 * g..16 + 6 * g];
                let (h, a) = (u16::from_be_bytes([f[0], f[1]]) as u32, u16::from_be_bytes([f[2], f[3]]) as u32);
                assert!(uids.contains(&h) && uids.contains(&a) && f[4] == 0xff, "{:?} game {g}", r.date);
                assert_ne!(club_country(&mut m.mem, h), club_country(&mut m.mem, a));
                assert!(once.insert(h) && once.insert(a), "{:?}: every club once", r.date);
            }
            days_seen += 1;
        }
        assert_eq!(days_seen, 8);
        // the league is played: the home side wins by its club's seeding (a lower uid wins), 2-0
        let (order, played) = league_order(&mut m.mem).unwrap();
        assert_eq!((order.len(), played), (36, 0));
        assert!(league_phase_end(&mut m.mem).is_ok_and(|done| !done), "nothing to do before the last game");
        let mut played_diary = out.clone();
        let mut p = 0;
        for rec in lma_data::diary::day_records(&out, year).unwrap() {
            if rec.kind == 0 {
                for g in 0..18 {
                    let f = p + 10 + 6 * g;
                    let (h, a) = (u16::from_be_bytes([out[f], out[f + 1]]), u16::from_be_bytes([out[f + 2], out[f + 3]]));
                    played_diary[f + 4] = if h < a { 0x20 } else { 0x02 };
                }
            }
            p += rec.bytes.len();
        }
        m.mem.write(DIARY_AT, &played_diary);
        let (order, played) = league_order(&mut m.mem).unwrap();
        assert_eq!(played, 144);
        const STORE: u32 = 0x4200_0000;
        m.mem.write32(STANDINGS, STORE);
        assert!(league_phase_end(&mut m.mem).unwrap());
        assert_eq!((0..8).map(|k| m.mem.read32(UEFA_THIRDS + 4 * k)).collect::<Vec<_>>(), order[24..32], "25th-32nd to the UEFA Cup");
        // the screens' store: places 1-32 as groups A-H on the last matchday it keeps
        let entry = |m: &mut Machine, md: u32, group: u32, pos: u32| m.mem.read64(STORE + 8 * ((md * 8 + group) * 4 + pos + STANDINGS_FIRST));
        for place in 0..32 {
            assert_eq!((entry(&mut m, 5, place / 4, place % 4) >> 48) as u32, order[place as usize], "place {place}");
        }
        assert_eq!(entry(&mut m, 5, 0, 0) & 0xffff_ffff_ffff, 0x08_00_00_10_00_18, "the top club: won 8, 16-0, 24 points");
        assert_eq!(entry(&mut m, 4, 0, 0), 0, "earlier matchdays are left as they were");
        assert!((0..16).all(|k| m.mem.read32(QUALIFIERS + 4 * k) == 0 && m.mem.read32(DIRECT_ENTRANTS + 4 * k) == 0));
        // the play-off: 17th-24th at home first against 9th-16th
        let po = playoff_ties(&mut m.mem, 9).unwrap();
        assert_eq!(po.len(), 8);
        for &(first, second) in &po {
            let (f, s) = (order.iter().position(|&u| u == first).unwrap(), order.iter().position(|&u| u == second).unwrap());
            assert!((16..24).contains(&f) && (8..16).contains(&s));
        }
        // the list the knockout writer takes
        const LIST: u32 = 0x4140_0000;
        write_tie_list(&mut m.mem, LIST, &po);
        assert_eq!((m.mem.read32(LIST), m.mem.read32(LIST + 0x404)), (1, 16));
        assert_eq!((m.mem.read32(LIST + 4), m.mem.read32(LIST + 8)), po[0]);
        // the seeded clubs win; the game collects them into 0x827414e0
        assert!(last_16_ties(&mut m.mem, 9).is_err(), "no winners yet");
        for (k, &(_, s)) in po.iter().enumerate() {
            m.mem.write32(QUALIFIERS + 4 * k as u32, s);
        }
        let l16 = last_16_ties(&mut m.mem, 9).unwrap();
        assert_eq!(l16.len(), 8);
        for &(first, second) in &l16 {
            assert!(order[..8].contains(&second), "a top-8 club at home second");
            assert!(po.iter().any(|t| t.1 == first), "against a play-off winner");
        }
        // a diary of the old shape (here: already full) is refused and left as it is
        assert!(league_phase_draw(&mut m.mem, 6).is_err());
        let mut again = vec![0u8; diary.len()];
        m.mem.read(DIARY_AT, &mut again);
        assert_eq!(again, played_diary);
        // and so are 35 entrants; with the extras from the UEFA Cup, an empty straight place
        // (no holder) takes a 5th
        m.mem.write32(QUALIFIERS + 60, 0);
        assert!(league_entrants(&mut m.mem, &uids[16..20]).is_none());
        // (the group end wrote the winners and runners-up over the sets' words)
        for k in 0..16 {
            m.mem.write32(QUALIFIERS + 4 * k, uids[20 + k as usize]);
            m.mem.write32(DIRECT_ENTRANTS + 4 * k, uids[k as usize]);
        }
        m.mem.write32(DIRECT_ENTRANTS + 8, 0);
        for (k, u) in [uids[16], uids[17], uids[18], uids[19], uids[2]].into_iter().enumerate() {
            m.mem.write32(UEFA_FIRST_ROUND + 4 * k as u32, u);
        }
        assert_eq!(uefa_extras(&mut m.mem, 5).len(), 5, "5 countries");
        assert!(league_entrants(&mut m.mem, &[uids[16], uids[17], uids[18], uids[19], uids[2]]).is_some());
        LEAGUE_EXTRAS.write().unwrap().clear();
    }

    /// The mod's places replace the nation's hard-coded counts and the game's function runs on;
    /// a nation the mod doesn't list keeps the game's.
    #[test]
    fn europe_places_rewrites_the_counts_and_declines() {
        let mut m = machine();
        *EUROPE_PLACES_TABLE.write().unwrap() = vec![[4, 0, 2], [0, 2, 1]];
        let record = 0x4200_0000;
        for (nation, want) in [(0u32, [0, 4, 2]), (1, [2, 0, 1]), (7, [1, 2, 2])] {
            // vanilla England: qualifying 2, group 2, UEFA Cup 1 (Portugal 1, 2, 2)
            let vanilla = if nation == 7 { [1, 2, 2] } else { [2, 2, 1] };
            for (k, v) in vanilla.into_iter().enumerate() {
                m.mem.write32(record + 4 * k as u32, v);
            }
            (m.cpu.gpr[3], m.cpu.gpr[4]) = (nation as u64, record as u64);
            assert!(!europe_places(&mut m), "the game's function hands out the places");
            assert_eq!([0, 1, 2].map(|k| m.mem.read32(record + 4 * k)), want, "nation {nation}");
        }
        assert_eq!(m.hook_calls[&EUROPE_PLACES], 2);
        EUROPE_PLACES_TABLE.write().unwrap().clear();
    }

    /// The transfer search's pre-filter answers "no" only where the game would: the searching club's
    /// own player, a flagged player or club, an unwanted country, a club tier out of range, too low an
    /// ability, or both positions off the one searched for; anything else goes to the game.
    #[test]
    fn search_prefilter_rejects_only_what_the_game_rejects() {
        let mut m = machine();
        const COUNT_AT: u32 = 0x4100_0000;
        m.mem.write32(PLAYER_COUNT, COUNT_AT);
        m.mem.write32(COUNT_AT + 4, 3);
        // player 2: first position 1 (bits 2-3), second position 3 (bits 0-1)
        m.mem.write32(rec(2) + 4, (1 << 2) | 3);
        let (entry, criteria) = (0x4200_0000, 0x4200_1000);
        // countries wanted: 0x33 (an unmapped club's) and 0 (a club whose stadium record reads zero)
        m.mem.write32(criteria + (0x33 + 10) * 4, 1);
        m.mem.write32(criteria + 10 * 4, 1);
        m.mem.write16(entry, 2);
        m.mem.write16(entry + 2, 41);
        let ask = |m: &mut Machine, club: u32, pos: u32| {
            m.mem.write32(criteria + 16, club);
            m.mem.write32(criteria + 24, pos);
            search_rejects(&mut m.mem, entry, criteria)
        };
        assert!(ask(&mut m, 41, 4), "the searching club's own player");
        assert!(!ask(&mut m, 7, 4), "any position: the game decides");
        assert!(!ask(&mut m, 7, 1) && !ask(&mut m, 7, 3), "either position matches: the game decides");
        assert!(ask(&mut m, 7, 2) && ask(&mut m, 7, 0), "neither matches");
        // the club's tier (runtime club record word 0 >> 28) must lie in [criteria + 12, criteria + 8]
        const MAP: u32 = 0x4300_0000;
        const CLUBS: u32 = 0x4310_0000;
        m.mem.write32(CLUB_UID_MAP, MAP);
        m.mem.write32(CLUB_RECORDS, CLUBS);
        m.mem.write16(MAP + 41 * 2, 5);
        m.mem.write32(CLUBS + 5 * 0x3d8, 3 << 28);
        m.mem.write32(criteria + 8, 4);
        m.mem.write32(criteria + 12, 2);
        assert!(!ask(&mut m, 7, 4), "tier 3 in [2, 4]");
        m.mem.write32(criteria + 12, 4);
        assert!(ask(&mut m, 7, 4), "tier 3 below the minimum 4");
        m.mem.write32(criteria + 12, 2);
        m.mem.write32(CLUBS + 5 * 0x3d8 + 4, 2);
        assert!(ask(&mut m, 7, 4), "a flagged club (word +4 bit 1)");
        m.mem.write32(CLUBS + 5 * 0x3d8 + 4, 0);
        // the club's country (its stadium's, record +0xc bits 15-20) must be wanted
        const STADIUMS: u32 = 0x4320_0000;
        m.mem.write32(STADIUM_RECORDS, STADIUMS);
        m.mem.write32(CLUBS + 5 * 0x3d8 + 4, 9 << 21);
        m.mem.write32(STADIUMS + 9 * 0x20 + 0xc, 7 << 15);
        assert!(ask(&mut m, 7, 4), "country 7 not wanted");
        m.mem.write32(criteria + (7 + 10) * 4, 1);
        assert!(!ask(&mut m, 7, 4), "country 7 wanted");
        m.mem.write32(rec(2) + 0x28, 1);
        assert!(ask(&mut m, 7, 4), "a flagged player (record +0x28 bit 0)");
        m.mem.write32(rec(2) + 0x28, 0);
        // the ability must reach criteria + 0 (no rating weights here: overall 0)
        m.mem.write32(criteria, 1);
        assert!(ask(&mut m, 7, 4), "ability 0 below 1");
        m.mem.write32(criteria, 0);
        // a club stored as -1 in the entry compares signed, as the game's extsh
        m.mem.write16(entry + 2, 0xffff);
        assert!(ask(&mut m, u32::MAX, 4));
    }

    /// Mod `bench` (#530): a match-mode team screen (16 rows a side) gets the extra rows; the squad
    /// screens' 32 and 48 stay.
    #[test]
    fn team_screen_rows_grow_by_the_extra_bench() {
        let _serial = RESERVES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = machine();
        let obj = 0x4500_0000u32;
        let entries = obj - ENTRIES_FROM_OBJ + ENTRY_TEAM_STRIDE;
        m.mem.write32(MATCH_GLOBALS, obj - OBJ_FROM_GLOBALS);
        m.mem.write32(TEAM_SCREEN_TEAM, 1);
        for k in 16..32u32 {
            // team 1 has three reserves past a gap
            m.mem.write32(entries + k * ENTRY_STRIDE, if matches!(k, 18 | 20 | 21) { 300 + k } else { u32::MAX });
        }
        *RESERVES.lock().unwrap() = None;
        m.mem.write32(TEAM_SCREEN_ROWS, 16);
        m.mem.write32(TEAM_SCREEN_ROWS + 0x20, 32);
        team_screen_rows(&mut m.mem, 4);
        assert_eq!((m.mem.read32(TEAM_SCREEN_ROWS), m.mem.read32(TEAM_SCREEN_ROWS + 0x20)), (19, 32), "a row per reserve");
        team_screen_rows(&mut m.mem, 4);
        assert_eq!(m.mem.read32(TEAM_SCREEN_ROWS), 19, "only a 16 grows");
        m.mem.write32(TEAM_SCREEN_ROWS, 16);
        team_screen_rows(&mut m.mem, 2);
        assert_eq!(m.mem.read32(TEAM_SCREEN_ROWS), 18, "bench 7: two");
    }

    #[test]
    fn the_team_line_drawer_looks_up_the_scrolled_row() {
        let mut m = machine();
        (m.cpu.gpr[3], m.cpu.gpr[22], m.cpu.lr) = (2, 18, 0x821D_00F8);
        scrolled_row(&mut m.cpu);
        assert_eq!(m.cpu.gpr[3], 18, "line 2 of a screen scrolled by 16");
        (m.cpu.gpr[3], m.cpu.lr) = (2, 0x821D_0E0C);
        scrolled_row(&mut m.cpu);
        assert_eq!(m.cpu.gpr[3], 2, "other callers keep their row");
    }

    /// Mod `bench` (#530): the lookup finds slot players as the game's does; only the substitution's
    /// incoming lookup, with extra reserves on, brings a reserve (pre-match entry 16..) into a bench
    /// slot: a used one first, else slot 15. The substitute it displaces is kept and can come on later.
    /// The slot getter with reserves: 0-15 as the game, 16.. a virtual slot made from the pre-match
    /// entry (or 0 for an empty entry, a reserve already in a slot, a team past 1, no bench key).
    #[test]
    fn bench_reserves_show_through_the_slot_getter() {
        let _serial = RESERVES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = machine();
        let obj = 0x4500_0000u32;
        let virt = 0x4600_0000u32;
        let entries = obj - ENTRIES_FROM_OBJ;
        for team in 0..2u32 {
            for i in 0..16u32 {
                let s = slot_at(obj, team, i);
                m.mem.write32(s, 100 * (team + 1) + i);
                m.mem.write32(s + 600, 0x5100 + i);
            }
            for k in 16..32u32 {
                let e = entries + team * ENTRY_TEAM_STRIDE + k * ENTRY_STRIDE;
                m.mem.write32(e, if matches!(k, 16..=18 | 20 | 21) { 100 * (team + 1) + k } else { u32::MAX });
                m.mem.write(e + 8, &[k as u8; 220]);
            }
        }
        *RESERVES.lock().unwrap() = None;
        assert_eq!(slot_get(&mut m.mem, obj, 0, 5, 4, virt), slot_at(obj, 0, 5), "the game's own slots");
        assert_eq!(slot_get(&mut m.mem, obj, 1, 15, 0, virt), slot_at(obj, 1, 15));
        assert_eq!(slot_get(&mut m.mem, obj, 2, 3, 4, virt), 0, "no third team");
        assert_eq!(slot_get(&mut m.mem, obj, 0, 16, 0, virt), 0, "bench 5: no reserves");
        assert_eq!(slot_get(&mut m.mem, obj, 0, 16, 4, 0), 0, "no virtual slots allocated");
        assert_eq!(slot_get(&mut m.mem, obj, 0, 20, 4, virt), 0, "only four reserves");
        assert_eq!(slot_get(&mut m.mem, obj, 0, 18, 2, virt), 0, "bench 7: reserves 16 and 17");
        let v = slot_get(&mut m.mem, obj, 0, 19, 4, virt);
        assert_eq!(m.mem.read32(v), 120, "row 19: the entry after the gap");
        let v = slot_get(&mut m.mem, obj, 1, 17, 4, virt);
        assert!((virt + 4 * SLOT_SIZE as u32..virt + 8 * SLOT_SIZE as u32).contains(&v), "team B's second reserve, in a team B slot");
        let mut img = [0u8; SLOT_SIZE];
        m.mem.read(v, &mut img);
        assert_eq!((word(&img, 0), word(&img, 4)), (217, 17), "the entry's id, the row's index");
        assert!(img[8..228].iter().chain(&img[228..448]).all(|&x| x == 17), "the entry's player data, twice");
        assert_eq!(word(&img, 600) & 0xff00, 0x5100, "the rest from the team's last bench slot");
        assert_eq!((word(&img, SLOT_STATE), word(&img, SLOT_ON_PITCH)), (0, 0), "before kick-off");
        m.mem.write32(slot_at(obj, 1, 0) + SLOT_STATE as u32, 1);
        let v = slot_get(&mut m.mem, obj, 1, 17, 4, virt);
        assert_eq!(m.mem.read32(v + SLOT_STATE as u32), STATE_BENCH, "in the match: on the bench");
        // brought on (his id in a real slot now): the rows after his move up
        m.mem.write32(slot_at(obj, 1, 14), 217);
        let v = slot_get(&mut m.mem, obj, 1, 17, 4, virt);
        assert_eq!(m.mem.read32(v), 218);
        assert_eq!(slot_get(&mut m.mem, obj, 1, 19, 4, virt), 0, "three left");
    }

    /// The team screen's rows 16..: the named reserves not in a slot, then the substitutes displaced
    /// earlier for that team only (by id, once each), at most four.
    #[test]
    fn the_reserve_list_puts_displaced_substitutes_after_the_named_reserves() {
        let _serial = RESERVES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = machine();
        let obj = 0x4500_0000u32;
        let entries = obj - ENTRIES_FROM_OBJ;
        for team in 0..2u32 {
            for i in 0..16u32 {
                m.mem.write32(slot_at(obj, team, i), 100 * (team + 1) + i);
            }
            for k in 16..32u32 {
                let e = entries + team * ENTRY_TEAM_STRIDE + k * ENTRY_STRIDE;
                m.mem.write32(e, if matches!(k, 16 | 18) { 100 * (team + 1) + k } else { u32::MAX });
            }
        }
        let ids = |l: Vec<(u32, u32)>| l.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        *RESERVES.lock().unwrap() = None;
        assert_eq!(ids(reserve_list(&mut m.mem, obj, 0, 4)), [116, 118], "two named reserves past the gaps");
        assert_eq!(ids(reserve_list(&mut m.mem, obj, 0, 1)), [116], "bench 6: one");
        *RESERVES.lock().unwrap() = Some(HashMap::from([((0, 115), vec![]), ((0, 150), vec![]), ((1, 215), vec![]), ((0, 116), vec![])]));
        // 115 is still in slot 15 and 116 is a named reserve already listed, so only 150 is added
        assert_eq!(ids(reserve_list(&mut m.mem, obj, 0, 4)), [116, 118, 150], "team B's displaced player stays on B");
        m.mem.write32(slot_at(obj, 0, 15), 118);
        assert_eq!(ids(reserve_list(&mut m.mem, obj, 0, 4)), [116, 115, 150], "118 came on, 115 went to the reserves");
        *RESERVES.lock().unwrap() = Some((100..110).map(|id| ((0, id + 50), vec![])).collect());
        assert_eq!(reserve_list(&mut m.mem, obj, 0, 4).len(), 4, "never more than four rows");
        *RESERVES.lock().unwrap() = None;
    }

    #[test]
    fn bench_reserves_come_on_through_the_slot_lookup() {
        let _serial = RESERVES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = machine();
        let obj = 0x4500_0000u32;
        let entries = obj - ENTRIES_FROM_OBJ;
        for team in 0..2u32 {
            for i in 0..16u32 {
                let s = slot_at(obj, team, i);
                m.mem.write32(s, 100 * (team + 1) + i);
                m.mem.write32(s + 4, i);
                m.mem.write32(s + 28, if i >= 11 { 10 } else { 0 });
                m.mem.write32(s + 600, 0x5100 + i); // something the builder puts in every bench slot
            }
            for k in 16..32u32 {
                let e = entries + team * ENTRY_TEAM_STRIDE + k * ENTRY_STRIDE;
                m.mem.write32(e, if k < 21 { 100 * (team + 1) + k } else { u32::MAX });
                m.mem.write(e + 8, &[k as u8; 220]);
            }
        }
        *RESERVES.lock().unwrap() = None;
        assert_eq!(slot_lookup(&mut m.mem, obj, 105, false, 4), slot_at(obj, 0, 5), "the game's own lookup");
        assert_eq!(slot_lookup(&mut m.mem, obj, 214, true, 4), slot_at(obj, 1, 14));
        assert_eq!(slot_lookup(&mut m.mem, obj, 116, false, 4), 0, "another caller: no swap");
        assert_eq!(slot_lookup(&mut m.mem, obj, 116, true, 0), 0, "bench 5: no reserves");
        assert_eq!(slot_lookup(&mut m.mem, obj, 120, true, 4), 0, "only the first 4 reserves");
        assert_eq!(slot_lookup(&mut m.mem, obj, 118, true, 2), 0, "bench 7: reserves 16 and 17");

        // reserve 116 comes on into slot 15 (no bench player has gone off yet)
        let s15 = slot_at(obj, 0, 15);
        assert_eq!(slot_lookup(&mut m.mem, obj, 116, true, 4), s15);
        let mut img = [0u8; SLOT_SIZE];
        m.mem.read(s15, &mut img);
        assert_eq!(word(&img, 0), 116);
        assert_eq!(word(&img, 4), 15, "the slot keeps its index");
        assert_eq!(word(&img, 28), 10);
        // the entry's player data, twice, apart from the slot's own +28 word inside the first copy
        assert!(img[8..28].iter().chain(&img[32..448]).all(|&x| x == 16), "the entry's player data, twice");
        assert_eq!(word(&img, 600) & 0xff00, 0x5100, "the rest from an unused bench slot");
        assert_eq!(slot_lookup(&mut m.mem, obj, 116, true, 4), s15, "now found in its slot");
        // the team screen's rows: the other reserves, then the substitute 116 displaced
        let virt = 0x4600_0000u32;
        let rows: Vec<u32> = (16..20).map(|i| {
            let v = slot_get(&mut m.mem, obj, 0, i, 4, virt);
            m.mem.read32(v)
        }).collect();
        assert_eq!(rows, [117, 118, 119, 115]);

        // substitute 115, displaced, can still come on: into the last unused bench slot (15 again,
        // 116 kept in turn); a player who went off (+996 = 4) keeps his slot
        let s14 = slot_at(obj, 0, 14);
        m.mem.write32(s14 + SLOT_STATE as u32, 4);
        assert_eq!(slot_lookup(&mut m.mem, obj, 115, true, 4), s15);
        assert_eq!(m.mem.read32(s15), 115);
        assert_eq!(m.mem.read32(s15 + 600), 0x510f, "his own image back");
        assert_eq!(m.mem.read32(s14), 114, "the player who went off stays");
        assert_eq!(slot_lookup(&mut m.mem, obj, 116, true, 4), s15, "and 116 can come back");
        // every bench slot used: nobody more comes on
        for i in 11..16 {
            m.mem.write32(slot_at(obj, 0, i) + SLOT_STATE as u32, 4);
        }
        assert_eq!(slot_lookup(&mut m.mem, obj, 117, true, 4), 0);
        *RESERVES.lock().unwrap() = None;

        // in the match the unused substitutes have state 2 (1 on the pitch): 117 comes on into slot 15
        for i in 0..16 {
            m.mem.write32(slot_at(obj, 0, i), 100 + i);
            m.mem.write32(slot_at(obj, 0, i) + SLOT_STATE as u32, if i < 11 { 1 } else { STATE_BENCH });
        }
        assert_eq!(slot_lookup(&mut m.mem, obj, 117, true, 4), s15);
        assert_eq!(m.mem.read32(s15 + SLOT_STATE as u32), STATE_BENCH, "the slot's own state until the swap");
        *RESERVES.lock().unwrap() = None;
    }

    /// #530 undo: a reserve queued by the subs menu (his virtual slot's state 6) keeps that slot as
    /// the menu left it, and the undo's lookups find him there instead of swapping him in.
    #[test]
    fn a_queued_reserve_can_be_undone() {
        let _serial = RESERVES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = machine();
        let obj = 0x4500_0000u32;
        let virt = 0x4600_0000u32;
        let entries = obj - ENTRIES_FROM_OBJ;
        for team in 0..2u32 {
            for i in 0..16u32 {
                let s = slot_at(obj, team, i);
                m.mem.write32(s, 100 * (team + 1) + i);
                m.mem.write32(s + SLOT_STATE as u32, if i < 11 { 1 } else { STATE_BENCH });
            }
            for k in 16..32u32 {
                let e = entries + team * ENTRY_TEAM_STRIDE + k * ENTRY_STRIDE;
                m.mem.write32(e, if k < 20 { 100 * (team + 1) + k } else { u32::MAX });
            }
        }
        *RESERVES.lock().unwrap() = None;
        // the menu shows reserve 117 on row 17 and queues him (state 6, a flag at +0x228)
        let v = slot_get(&mut m.mem, obj, 0, 17, 4, virt);
        assert_eq!((m.mem.read32(v), m.mem.read32(v + SLOT_STATE as u32)), (117, STATE_BENCH));
        m.mem.write32(v + SLOT_STATE as u32, STATE_QUEUED);
        m.mem.write32(v + 0x228, 1);
        // the screen asks again: the slot stays as queued
        assert_eq!(slot_get(&mut m.mem, obj, 0, 17, 4, virt), v);
        assert_eq!((m.mem.read32(v + SLOT_STATE as u32), m.mem.read32(v + 0x228)), (STATE_QUEUED, 1));
        // the undo's lookup: his virtual slot, nobody swapped into the bench
        assert_eq!(slot_lookup(&mut m.mem, obj, 117, false, 4), 0);
        assert_eq!(reserve_slot(&mut m.mem, obj, 117, 4, virt), v);
        assert!((11..16).all(|i| m.mem.read32(slot_at(obj, 0, i)) == 100 + i), "the bench is untouched");
        // once undone (state 2 again) the row is rebuilt as before
        m.mem.write32(v + SLOT_STATE as u32, STATE_BENCH);
        assert_eq!(slot_get(&mut m.mem, obj, 0, 17, 4, virt), v);
        assert_eq!(m.mem.read32(v), 117);
        assert_eq!(reserve_slot(&mut m.mem, obj, 999, 4, virt), 0, "not a reserve");
        *RESERVES.lock().unwrap() = None;
    }

    /// The lookup block itself: only the undo's call sites fall back to a reserve's virtual slot;
    /// a substitute on the real bench is found where he is, and any other caller still gets 0 for
    /// a reserve (the game's own answer).
    #[test]
    fn only_the_undo_lookups_find_a_reserve() {
        use std::sync::atomic::Ordering::Relaxed;
        let _serial = RESERVES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = machine();
        let obj = 0x4500_0000u32;
        let virt = 0x4600_0000u32;
        let entries = obj - ENTRIES_FROM_OBJ;
        for i in 0..16u32 {
            let s = slot_at(obj, 0, i);
            m.mem.write32(s, 100 + i);
            m.mem.write32(s + SLOT_STATE as u32, if i < 11 { 1 } else { STATE_BENCH });
        }
        for k in 16..32u32 {
            m.mem.write32(entries + k * ENTRY_STRIDE, if k < 20 { 100 + k } else { u32::MAX });
        }
        *RESERVES.lock().unwrap() = None;
        let (old_extra, old_virt) = (BENCH_EXTRA.swap(4, Relaxed), VIRTUAL_SLOTS.swap(virt, Relaxed));
        let v = slot_get(&mut m.mem, obj, 0, 17, 4, virt);
        let mut ask = |m: &mut Machine, lr: u32, id: u32| {
            m.cpu.lr = lr as u64;
            m.cpu.gpr[3] = obj as u64;
            m.cpu.gpr[4] = id as u64;
            let mut b = 0i64;
            assert_eq!(slot_lookup_block(&mut m.cpu, &mut m.mem, 0, &mut b).unwrap(), lr);
            m.cpu.gpr[3] as u32
        };
        for &lr in &UNDO_LOOKUP_RETS {
            assert_eq!(ask(&mut m, lr, 112), slot_at(obj, 0, 12), "a real substitute, from {lr:#x}");
            assert_eq!(ask(&mut m, lr, 117), v, "a reserve's virtual slot, from {lr:#x}");
            assert_eq!(ask(&mut m, lr, 999), 0, "nobody, from {lr:#x}");
        }
        assert_eq!(ask(&mut m, 0x8200_0000, 117), 0, "another caller: no reserve");
        assert_eq!(ask(&mut m, 0x8200_0000, 112), slot_at(obj, 0, 12));
        assert!((11..16).all(|i| m.mem.read32(slot_at(obj, 0, i)) == 100 + i), "the bench is untouched");
        BENCH_EXTRA.store(old_extra, Relaxed);
        VIRTUAL_SLOTS.store(old_virt, Relaxed);
        *RESERVES.lock().unwrap() = None;
    }

    /// #530: a reserve swapped into an empty bench slot (id -1, zeros) gets a player's slot image (his
    /// team at +0x230, which the substitution counts substitutions through), the slot's own index,
    /// the bench state, and nobody is kept for the empty slot.
    #[test]
    fn a_reserve_in_an_empty_slot_gets_a_players_slot_image() {
        let _serial = RESERVES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = machine();
        let obj = 0x4500_0000u32;
        let entries = obj - ENTRIES_FROM_OBJ;
        for i in 0..16u32 {
            let s = slot_at(obj, 0, i);
            let empty = i == 13 || i == 15;
            m.mem.write32(s, if empty { u32::MAX } else { 100 + i });
            m.mem.write32(s + 4, if empty { 0 } else { i });
            m.mem.write32(s + 0x230, if empty { 0 } else { obj + 0x30 });
            m.mem.write32(s + SLOT_STATE as u32, if empty { 0 } else if i < 11 { 1 } else { STATE_BENCH });
        }
        m.mem.write32(entries + 16 * ENTRY_STRIDE, 116);
        for k in 17..32u32 {
            m.mem.write32(entries + k * ENTRY_STRIDE, u32::MAX);
        }
        *RESERVES.lock().unwrap() = None;
        let s = slot_lookup(&mut m.mem, obj, 116, true, 4);
        assert_eq!(s, slot_at(obj, 0, 15), "the last unused bench slot");
        assert_eq!((m.mem.read32(s), m.mem.read32(s + 4)), (116, 15), "his id, the slot's index");
        assert_eq!(m.mem.read32(s + 0x230), obj + 0x30, "his team, from slot 14");
        assert_eq!(m.mem.read32(s + SLOT_STATE as u32), STATE_BENCH);
        assert!(RESERVES.lock().unwrap().as_ref().is_none_or(|r| r.is_empty()), "no one kept for the empty slot");
        *RESERVES.lock().unwrap() = None;
    }

    /// #530 (the maintainer, 28 Oct: "the 5th sub glitched"): the subs menu keeps its own id per row and queues
    /// the cursor row's slot, so a virtual row holds the reserve with the menu's id for it (a gap or a
    /// player who can't come on is a blank row); the other team, with no menu over it, keeps the
    /// reserve list's order, and the undo finds a reserve on the row the menu has him.
    #[test]
    fn virtual_rows_follow_the_menus_row_ids() {
        let _serial = RESERVES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = machine();
        let obj = 0x4500_0000u32;
        let virt = 0x4600_0000u32;
        let ids = 0x4700_0000u32;
        let entries = obj - ENTRIES_FROM_OBJ;
        for team in 0..2u32 {
            for i in 0..16u32 {
                m.mem.write32(slot_at(obj, team, i), 100 * (team + 1) + i);
            }
            for k in 16..32u32 {
                let e = entries + team * ENTRY_TEAM_STRIDE + k * ENTRY_STRIDE;
                m.mem.write32(e, if k < 20 { 100 * (team + 1) + k } else { u32::MAX });
            }
        }
        *RESERVES.lock().unwrap() = None;
        m.mem.write32(TEAM_SCREEN_TEAM, 0);
        m.mem.write32(MENU_ROW_IDS, ids);
        for (i, id) in [(16u32, 119i16), (17, -1), (18, 116), (19, 150)] {
            m.mem.write16(ids + 2 * i, id as u16);
        }
        let row = |m: &mut Machine, team, i| {
            let v = slot_get(&mut m.mem, obj, team, i, 4, virt);
            if v == 0 { 0 } else { m.mem.read32(v) }
        };
        assert_eq!((16..20).map(|i| row(&mut m, 0, i)).collect::<Vec<_>>(), [119, 0, 116, 0], "the menu's ids; a gap and a non-reserve blank");
        assert_eq!((16..20).map(|i| row(&mut m, 1, i)).collect::<Vec<_>>(), [216, 217, 218, 219], "the other team: the reserve list");
        assert_eq!(reserve_slot(&mut m.mem, obj, 116, 4, virt), slot_get(&mut m.mem, obj, 0, 18, 4, virt), "the undo finds 116 on row 18");
        assert_eq!(reserve_slot(&mut m.mem, obj, 117, 4, virt), 0, "117 is on none of the menu's rows");
        // queued (state 6): the menu swaps the two ids in its table while the substitution waits; the
        // row keeps the reserve, and once he has come on (in a real slot) it follows the table again
        let v = slot_get(&mut m.mem, obj, 0, 18, 4, virt);
        m.mem.write32(v + SLOT_STATE as u32, STATE_QUEUED);
        m.mem.write16(ids + 2 * 18, 105);
        assert_eq!(slot_get(&mut m.mem, obj, 0, 18, 4, virt), v);
        assert_eq!((m.mem.read32(v), m.mem.read32(v + SLOT_STATE as u32)), (116, STATE_QUEUED), "still the queued reserve");
        assert_eq!(reserve_slot(&mut m.mem, obj, 116, 4, virt), v, "the undo finds him queued, off the reserve rows");
        VIRTUAL_SLOTS.store(virt, std::sync::atomic::Ordering::Relaxed);
        m.mem.write16(ids + 2 * 19, -1i16 as u16);
        assert_eq!(reserve_rows(&mut m.mem, obj, 0, 4), 3, "his row (18) still counts, past the table's last reserve (16)");
        VIRTUAL_SLOTS.store(0, std::sync::atomic::Ordering::Relaxed);
        m.mem.write16(ids + 2 * 19, 150);
        let r16 = slot_get(&mut m.mem, obj, 0, 16, 4, virt);
        assert_eq!(m.mem.read32(r16), 119, "the other rows keep their own slots");
        assert_ne!(slot_get(&mut m.mem, obj, 0, 16, 4, virt), v);
        m.mem.write32(slot_at(obj, 0, 15), 116);
        assert_eq!(slot_get(&mut m.mem, obj, 0, 18, 4, virt), 0, "on now: the row is the table's again (105, not a reserve)");
        m.mem.write32(slot_at(obj, 0, 15), 115);
        m.mem.write32(v + SLOT_STATE as u32, 0);
        // the menu's table also lists injured players (no reserves): a reserve past row 19 gets a row
        m.mem.write16(ids + 2 * 18, 116);
        m.mem.write16(ids + 2 * 21, 117);
        assert_eq!(reserve_rows(&mut m.mem, obj, 0, 4), 6, "rows 16..21");
        assert_eq!(row(&mut m, 0, 21), 117);
        assert_eq!(reserve_rows(&mut m.mem, obj, 1, 4), 4, "the other team: one per reserve");
        m.mem.write32(MENU_ROW_IDS, 0);
        assert_eq!((16..20).map(|i| row(&mut m, 0, i)).collect::<Vec<_>>(), [116, 117, 118, 119], "no menu: the reserve list");
        *RESERVES.lock().unwrap() = None;
    }

    /// A mod club's badge entry comes from the table, above 511 too; other clubs, an unmapped uid
    /// and a call before the uid table exists go to the game's getter, with r3 untouched.
    #[test]
    fn badge_get_answers_for_mod_clubs() {
        let mut m = machine();
        m.badge_entries.insert(7, 640);
        const MAP: u32 = 0x4100_0000;
        m.cpu.gpr[3] = 0x123;
        assert!(!badge_get(&mut m), "no uid table yet");
        assert_eq!(m.cpu.gpr[3], 0x123);
        m.mem.write32(CLUB_UID_MAP, MAP);
        for u in 0..0x2056u32 {
            m.mem.write16(MAP + u * 2, 0xffff);
        }
        m.mem.write16(MAP + 0x123 * 2, 7);
        m.mem.write16(MAP + 0x124 * 2, 8);
        assert!(badge_get(&mut m));
        assert_eq!(m.cpu.gpr[3], 640);
        m.cpu.gpr[3] = 0x124;
        assert!(!badge_get(&mut m), "no badge in the mod");
        m.cpu.gpr[3] = 0xffff_ffff;
        assert!(!badge_get(&mut m), "-1 is the game's own no-club value");
    }

    /// The native overall: the role from the position bits and flags, the
    /// weighted mean of the 13 ratings, and the game's answer for an id past the table (record 0).
    #[test]
    fn player_overall_is_the_weighted_mean_for_the_role() {
        let mut m = machine();
        const COUNT_AT: u32 = 0x4100_0000;
        m.mem.write32(PLAYER_COUNT, COUNT_AT);
        m.mem.write32(COUNT_AT + 4, 2);
        // player 0: a keeper rated 50 + k on rating k; player 1: a defender with the flag set
        let words = |base: i32| {
            let mut w = [0u32; 8];
            for (k, &(word, rot)) in OVERALL_RATINGS.iter().enumerate() {
                w[word as usize / 4] |= (((base + k as i32) as u32) & 0x7f).rotate_right(rot);
            }
            w
        };
        for (p, (pos, flag)) in [(0u32, 0u32), (1, 1)].into_iter().enumerate() {
            let mut w = words(50);
            w[1] |= pos << 2;
            w[6] |= flag;
            for (i, v) in w.iter().enumerate() {
                m.mem.write32(TABLE + p as u32 * 300 + 4 * i as u32, *v);
            }
        }
        for role in 0..10 {
            for k in 0..13 {
                m.mem.write8(OVERALL_WEIGHTS + role * 14 + k, if role == 2 { (k == 12) as u8 * 3 } else { 1 });
            }
        }
        assert_eq!(player_role(&mut m.mem, 0), 0);
        assert_eq!(player_overall_of(&mut m.mem, 0), Some((50..63).sum::<u32>() / 13), "the keeper's mean");
        assert_eq!(player_role(&mut m.mem, 1), 2, "a defender with the flag");
        assert_eq!(player_overall_of(&mut m.mem, 1), Some(62), "role 2 weighs only the last rating");
        // past the table and negative: record 0, low memory (zero here), so the keeper's row of zeros
        assert_eq!(player_overall_of(&mut m.mem, 2), Some(0));
        assert_eq!(player_overall_of(&mut m.mem, u32::MAX), Some(0));
        m.cpu.gpr[3] = 1;
        assert!(player_overall(&mut m) && m.cpu.gpr[3] == 62);
        // the block-table versions (the default): r3, back to lr, one unit of budget
        let mut budget = 10;
        m.cpu.gpr[3] = 1;
        m.cpu.lr = 0x8250_0004;
        assert_eq!(overall_block(&mut m.cpu, &mut m.mem, PLAYER_OVERALL, &mut budget).unwrap(), 0x8250_0004);
        assert_eq!((m.cpu.gpr[3], budget), (62, 9));
        m.cpu.gpr[3] = 1;
        assert_eq!(role_block(&mut m.cpu, &mut m.mem, PLAYER_ROLE, &mut budget).unwrap(), 0x8250_0004);
        assert_eq!(m.cpu.gpr[3], 2);
    }

    /// A squad_overflow.csv player belongs to his club (its uid, through the uid table); anyone
    /// else, and any call before the uid table exists, goes to the game's own lookup.
    #[test]
    fn player_club_answers_for_overflow_players() {
        let mut m = machine();
        m.squad_overflow.insert(4_000, 7);
        m.cpu.gpr[3] = 4_000;
        assert!(!player_club(&mut m), "no uid table yet: the game's lookup");
        const MAP: u32 = 0x4100_0000;
        m.mem.write32(CLUB_UID_MAP, MAP);
        for u in 0..0x2056u32 {
            m.mem.write16(MAP + u * 2, 0xffff);
        }
        m.mem.write16(MAP + 0x123 * 2, 7);
        assert!(player_club(&mut m));
        assert_eq!(m.cpu.gpr[3], 0x123);
        assert_eq!(m.squad_overflow_uids.get(&7), Some(&0x123), "the club's uid is kept");
        m.mem.write16(MAP + 0x123 * 2, 0xffff);
        m.cpu.gpr[3] = 4_000;
        assert!(player_club(&mut m) && m.cpu.gpr[3] == 0x123, "and used without scanning the table again");
        m.cpu.gpr[3] = 4_001;
        assert!(!player_club(&mut m), "not an overflow player");
    }

    /// The club lookup as a native block (#39): an overflow player's club from the uid table,
    /// returning to lr; anyone else goes to the game's lookup behind it.
    #[test]
    fn player_club_block_answers_or_falls_back_to_the_game() {
        fn game(c: &mut guest::cpu::Cpu, _m: &mut guest::mem::Memory, _pc: u32, _b: &mut i64) -> Result<u32, guest::cpu::Stop> {
            c.gpr[3] = 0x77;
            Ok(c.lr as u32 & !3)
        }
        let mut m = machine();
        const MAP: u32 = 0x4100_0000;
        m.mem.write32(CLUB_UID_MAP, MAP);
        for u in 0..0x2056u32 {
            m.mem.write16(MAP + u * 2, 0xffff);
        }
        m.mem.write16(MAP + 0x123 * 2, 7);
        let block = player_club_block_for(game, &[(4_000u16, 7u16)].into_iter().collect());
        let mut b = 100i64;
        m.cpu.lr = 0x8200_1234;
        m.cpu.gpr[3] = 4_000;
        assert_eq!(block(&mut m.cpu, &mut m.mem, PLAYER_CLUB, &mut b).unwrap(), 0x8200_1234);
        assert_eq!(m.cpu.gpr[3], 0x123, "the overflow player's club");
        m.cpu.gpr[3] = 4_001;
        block(&mut m.cpu, &mut m.mem, PLAYER_CLUB, &mut b).unwrap();
        assert_eq!(m.cpu.gpr[3], 0x77, "anyone else: the game's lookup");
    }

    /// The real wage goes into the cache once (the goals count in the low bits kept), the mark into
    /// +0x5c; later calls leave the cache to the game, so a renewal sticks.
    #[test]
    fn wage_applies_once_and_keeps_the_goals() {
        let mut m = machine();
        m.wages.insert(5, (300_000, Some(3)));
        m.mem.write32(rec(5) + 0x38, (36_000 << 13) | 2);
        m.cpu.gpr[3] = 5;
        assert!(wage(&mut m));
        assert_eq!(m.cpu.gpr[3], 300_000);
        assert_eq!(m.mem.read32(rec(5) + 0x38), (300_000 << 13) | 2, "two goals kept");
        assert_eq!(m.mem.read32(rec(5) + MARKS) & WAGE_APPLIED, WAGE_APPLIED);
        m.mem.write32(rec(5) + 0x38, (350_000 << 13) | 2);
        m.cpu.gpr[3] = 5;
        assert!(!wage(&mut m), "a renewed wage is the game's");
        m.cpu.gpr[3] = 6;
        assert!(!wage(&mut m), "a player without a wages.csv row is the game's");
    }

    /// `call_to_the_end` runs a call with the slice lengthened: inside it the guest's time base reads
    /// as just before it (real-time and deterministic), the budget can't run out, and afterwards the
    /// slice is as it was, less the call's instructions.
    #[test]
    fn call_to_the_end_keeps_the_time_base() {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        static SEEN: AtomicU64 = AtomicU64::new(0);
        fn game(c: &mut guest::cpu::Cpu, _: &mut guest::mem::Memory, _: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
            SEEN.store(guest::rec::timebase_at(c, *b), Relaxed);
            assert!(*b > 1 << 39, "a lengthened slice");
            *b -= 500; // the call's instructions
            Ok(c.lr as u32 & !3)
        }
        for rt in [false, true] {
            let mut m = machine();
            (m.cpu.rt, m.cpu.rt_base, m.cpu.slice_len, m.cpu.slice_end, m.cpu.lr) = (rt, 7_000, 10_000, 1_000_000, 0x8200_1000);
            let (len, end) = (m.cpu.slice_len, m.cpu.slice_end);
            let mut b = 3; // all but spent
            let before = guest::rec::timebase_at(&m.cpu, b);
            assert_eq!(call_to_the_end(&mut m.cpu, &mut m.mem, AI_OFFER, &mut b, game).unwrap(), 0x8200_1000);
            assert_eq!(SEEN.load(Relaxed), before, "rt {rt}: the time base inside the call");
            assert_eq!((m.cpu.slice_len, m.cpu.slice_end, b), (len, end, 3 - 500), "rt {rt}: the slice restored, the call charged");
        }
    }

    /// Formations 14/15 (#749): the offset getters answer the game's 0.0 without calling the game,
    /// the setters drop the write, formations 0-13 reach the game; the diagram's placement runs
    /// with the record's "no custom positions" byte set for the call and restored after.
    #[test]
    fn formation_offsets_past_14_are_zero_and_unwritten() {
        use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
        static CALLS: AtomicU32 = AtomicU32::new(0);
        static FLAG_SEEN: AtomicU32 = AtomicU32::new(0);
        const RECORD: u32 = 0x8300_0000;
        fn get(c: &mut guest::cpu::Cpu, _: &mut guest::mem::Memory, _: u32, _: &mut i64) -> Result<u32, guest::cpu::Stop> {
            CALLS.fetch_add(1, Relaxed);
            c.fpr[1] = 7.0;
            Ok(c.lr as u32 & !3)
        }
        fn set(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, _: u32, _: &mut i64) -> Result<u32, guest::cpu::Stop> {
            CALLS.fetch_add(1, Relaxed);
            m.write32(RECORD + 0x100, 1);
            Ok(c.lr as u32 & !3)
        }
        fn place(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, _: u32, _: &mut i64) -> Result<u32, guest::cpu::Stop> {
            FLAG_SEEN.store(m.read8(RECORD + 0x37) as u32, Relaxed);
            Ok(c.lr as u32 & !3)
        }
        let blocks = formation_offset_blocks_for([get, set, get, set, place]);
        let mut m = machine();
        m.mem.write32(FORMATION_OFFSET_ZERO, 0.0f32.to_bits());
        m.mem.write32(FORMATION_DIAGRAM_STATE + 8, RECORD);
        m.cpu.lr = 0x8200_1000;
        let mut b = 1000;
        let run = |m: &mut crate::machine::Machine, k: usize, f: u64, b: &mut i64| {
            m.cpu.gpr[4] = f;
            m.cpu.gpr[3] = f;
            m.cpu.fpr[1] = -1.0;
            (blocks[k].1)(&mut m.cpu, &mut m.mem, blocks[k].0, b).unwrap()
        };
        assert_eq!(run(&mut m, 0, 3, &mut b), 0x8200_1000);
        assert_eq!((m.cpu.fpr[1], CALLS.load(Relaxed)), (7.0, 1), "formation 3: the game's getter");
        run(&mut m, 2, 14, &mut b);
        assert_eq!((m.cpu.fpr[1], CALLS.load(Relaxed)), (0.0, 1), "formation 14: 0.0, the game not called");
        run(&mut m, 3, 15, &mut b);
        assert_eq!((m.mem.read32(RECORD + 0x100), CALLS.load(Relaxed)), (0, 1), "formation 15: the write dropped");
        run(&mut m, 1, 13, &mut b);
        assert_eq!((m.mem.read32(RECORD + 0x100), CALLS.load(Relaxed)), (1, 2), "formation 13: the game's setter");
        m.mem.write8(RECORD + 0x37, 0);
        run(&mut m, 4, 14, &mut b);
        assert_eq!((FLAG_SEEN.load(Relaxed), m.mem.read8(RECORD + 0x37)), (1, 0), "formation 14: no custom positions during the call, the byte restored");
        run(&mut m, 4, 2, &mut b);
        assert_eq!(FLAG_SEEN.load(Relaxed), 0, "formation 2: the record's own byte");
    }

    /// `round_wage` rounds to the game's steps (the table read from memory) and clamps at the cap.
    #[test]
    fn round_wage_uses_the_steps_and_the_cap() {
        let mut m = machine();
        for (k, (step, below)) in [(10, 2000), (50, 5000), (100, 20_000), (500, 50_000), (1000, 100_000), (10_000, 250_000)].into_iter().enumerate() {
            m.mem.write32(0x8200_560C + 8 * k as u32, step);
            m.mem.write32(0x8200_5610 + 8 * k as u32, below);
        }
        assert_eq!(round_wage(&mut m.mem, 1234, 250_000), 1230);
        assert_eq!(round_wage(&mut m.mem, 26_260, 250_000), 26_500);
        assert_eq!(round_wage(&mut m.mem, 331_000, 250_000), 250_000, "the game's clamp");
        assert_eq!(round_wage(&mut m.mem, 331_000, WAGE_MAX), 330_000, "the table's last step above 250,000");
        assert_eq!(round_wage(&mut m.mem, 364_100, WAGE_MAX), 360_000, "a 331k player's floor stays about +10%");
        assert_eq!(round_wage(&mut m.mem, 528_000, WAGE_MAX), WAGE_MAX, "clamped at WAGE_MAX");
        assert_eq!(round_wage(&mut m.mem, 20, 250_000), 150, "at least 150");
    }

    /// `rating_wage_scale`: the game's wage x the scale, rounded as the game rounds, capped.
    #[test]
    fn rating_wage_scales_and_rounds() {
        let mut m = machine();
        for (k, (step, below)) in [(10, 2000), (50, 5000), (100, 20_000), (500, 50_000), (1000, 100_000), (10_000, 250_000)].into_iter().enumerate() {
            m.mem.write32(0x8200_560C + 8 * k as u32, step);
            m.mem.write32(0x8200_5610 + 8 * k as u32, below);
        }
        assert_eq!(scale_rating_wage(&mut m.mem, 80_000, 5000, WAGE_MAX), 400_000, "a capped top-flight regen x 5");
        assert_eq!(scale_rating_wage(&mut m.mem, 80_000, 5000, 250_000), 250_000, "the game's cap without ai_wage_offers");
        assert_eq!(scale_rating_wage(&mut m.mem, 1_234, 1000, WAGE_MAX), round_wage(&mut m.mem, 1_234, WAGE_MAX), "x 1 is the game's rounding");
        assert!(guest::hooks::ADDRESSES.contains(&RATING_WAGE), "recompiled callers must reach the block");
    }

    /// The native block answers as the hook does, and leaves the rest to the game's getter.
    #[test]
    fn wage_block_matches_the_hook() {
        fn game(c: &mut guest::cpu::Cpu, _: &mut guest::mem::Memory, _: u32, _: &mut i64) -> Result<u32, guest::cpu::Stop> {
            c.gpr[3] = 1234;
            Ok(c.lr as u32 & !3)
        }
        let mut m = machine();
        let wages = HashMap::from([(5, (300_000, Some(3)))]);
        let block = wage_block_for(game, &wages);
        m.mem.write32(rec(5) + 0x38, (36_000 << 13) | 2);
        let mut b = 100;
        m.cpu.lr = 0x8200_1000;
        m.cpu.gpr[3] = 5;
        assert_eq!(block(&mut m.cpu, &mut m.mem, WAGE, &mut b).unwrap(), 0x8200_1000);
        assert_eq!(m.cpu.gpr[3], 300_000);
        assert_eq!(m.mem.read32(rec(5) + 0x38), (300_000 << 13) | 2, "two goals kept");
        m.cpu.gpr[3] = 5;
        block(&mut m.cpu, &mut m.mem, WAGE, &mut b).unwrap();
        assert_eq!(m.cpu.gpr[3], 1234, "applied once: then the game's");
        m.cpu.gpr[3] = 6;
        block(&mut m.cpu, &mut m.mem, WAGE, &mut b).unwrap();
        assert_eq!(m.cpu.gpr[3], 1234, "no row: the game's");
    }

    /// Records marked by builds 24-25 (bits 5 and 6 of +0x38, the goals count) are repaired.
    #[test]
    fn old_marks_are_taken_out_of_the_goals() {
        let mut m = machine();
        m.wages.insert(7, (100_000, None));
        m.mem.write32(rec(7) + 0x38, (100_000 << 13) | OLD_MARKS | 3);
        m.cpu.gpr[3] = 7;
        assert!(!wage(&mut m), "already applied by the old build");
        assert_eq!(m.mem.read32(rec(7) + 0x38) & 0x7f, 3, "back to 3 goals");
        assert_eq!(m.mem.read32(rec(7) + MARKS) & (WAGE_APPLIED | CONTRACT_APPLIED), WAGE_APPLIED | CONTRACT_APPLIED);
    }

    /// The real contract goes through the game's setter once, within the game's 5 seasons.
    #[test]
    fn contract_set_once_within_five_seasons() {
        let mut m = machine();
        m.wages.insert(9, (50_000, Some(7)));
        m.cpu.gpr[3] = 9;
        m.cpu.gpr[4] = 2;
        assert!(!contract_set(&mut m), "the game's setter still runs");
        assert_eq!(m.cpu.gpr[4], 5);
        m.cpu.gpr[3] = 9;
        m.cpu.gpr[4] = 2;
        contract_set(&mut m);
        assert_eq!(m.cpu.gpr[4], 2, "a later contract is the game's");
    }

    /// `gate_scale` (#619): the gate's sites take the gate's own factor, every other income
    /// `income_scale`; the statement line follows the credit.
    #[test]
    fn gate_scale_applies_at_the_gate_sites_only() {
        assert!(GATE_CALLERS.iter().all(|a| INCOME_CALLERS.contains(a)) && GATE_STATEMENTS.iter().all(|a| INCOME_STATEMENTS.contains(a)));
        let mut m = machine();
        m.income_scale = 4.0;
        m.gate_scale = Some(8.0);
        m.cpu.gpr[4] = 100_000;
        m.cpu.lr = GATE_CALLERS[0] as u64;
        add_funds(&mut m);
        assert_eq!(m.cpu.gpr[4], 800_000, "the gate");
        m.cpu.gpr[4] = 100_000;
        m.cpu.lr = 0x820E_CA28; // prize money
        add_funds(&mut m);
        assert_eq!(m.cpu.gpr[4], 400_000, "prize money keeps income_scale");
        (m.cpu.gpr[5], m.cpu.gpr[6], m.cpu.lr) = (0, 100_000, GATE_STATEMENTS[0] as u64);
        statement(&mut m);
        assert_eq!(m.cpu.gpr[6], 800_000, "the gate's statement line");
        m.gate_scale = None;
        (m.cpu.gpr[4], m.cpu.lr) = (100_000, GATE_CALLERS[1] as u64);
        add_funds(&mut m);
        assert_eq!(m.cpu.gpr[4], 400_000, "no gate_scale: income_scale");
    }

    /// A stored value is worth itself (x unit) while ratings and form haven't moved; a player
    /// without one is priced by the game.
    #[test]
    fn valuation_is_the_stored_value() {
        let mut m = machine();
        m.values_as_stored = true;
        m.mem.write16(rec(11) + 0x14, 100);
        m.cpu.gpr[3] = 11;
        assert!(valuation(&mut m));
        let v = m.cpu.gpr[3] as i64;
        assert!((97_500..=102_500).contains(&v), "{v}");
        m.cpu.gpr[3] = 12;
        assert!(!valuation(&mut m), "no stored value: the game's own price");
    }

    /// The valuation as a native block (#39) answers as the hook does and returns to lr; a player
    /// without a stored value goes to the game's function behind it.
    #[test]
    fn valuation_block_answers_or_falls_back_to_the_game() {
        fn game(c: &mut guest::cpu::Cpu, _m: &mut guest::mem::Memory, _pc: u32, _b: &mut i64) -> Result<u32, guest::cpu::Stop> {
            c.gpr[3] = 4242;
            Ok(c.lr as u32 & !3)
        }
        let mut m = machine();
        m.values_as_stored = true;
        m.mem.write16(rec(11) + 0x14, 100);
        m.cpu.gpr[3] = 11;
        assert!(valuation(&mut m));
        let hooked = m.cpu.gpr[3];
        let block = valuation_block_for(game);
        let mut b = 100i64;
        m.cpu.lr = 0x8200_1234;
        m.cpu.gpr[3] = 11;
        assert_eq!(block(&mut m.cpu, &mut m.mem, VALUATION, &mut b).unwrap(), 0x8200_1234);
        assert_eq!(m.cpu.gpr[3], hooked, "the hook's answer");
        m.cpu.gpr[3] = 12;
        block(&mut m.cpu, &mut m.mem, VALUATION, &mut b).unwrap();
        assert_eq!(m.cpu.gpr[3], 4242, "no stored value: the game's function");
    }

    /// The value's factors: 12% a rating point within 0.25..4; match form only with appearances,
    /// in full from 10, up above the level's expected rating and down below it.
    #[test]
    fn value_factors() {
        assert_eq!(form_factor(70.0, 70.0), 1.0);
        assert!((form_factor(75.0, 70.0) - 1.12f64.powi(5)).abs() < 1e-9);
        assert_eq!((form_factor(99.0, 40.0), form_factor(40.0, 99.0)), (4.0, 0.25));
        assert_eq!((match_form(0, 10, 70.0), match_form(80, 0, 70.0)), (1.0, 1.0), "no season yet");
        // level 70 expects 62.7 + 0.072 x 70 = 67.74
        assert!(match_form(75, 10, 70.0) > 1.0 && match_form(60, 10, 70.0) < 1.0);
        assert!((match_form(75, 5, 70.0).powi(2) - match_form(75, 10, 70.0)).abs() < 1e-9, "half at 5 appearances");
        assert_eq!(match_form(75, 30, 70.0), match_form(75, 10, 70.0), "full from 10");
    }

    /// The dequantising offset: within half a unit either side, in GBP 100 steps, and spread (the
    /// 668 players at exactly GBP 80k get distinct values).
    #[test]
    fn dequantise_spreads_within_half_a_unit() {
        let offs: Vec<f64> = (0..1000).map(|i| dequantise(i, 5000)).collect();
        assert!(offs.iter().all(|o| o.abs() <= 2500.0 && o % 100.0 == 0.0));
        let distinct: std::collections::HashSet<i64> = offs.iter().map(|o| *o as i64).collect();
        assert!(distinct.len() > 40, "{} distinct offsets", distinct.len());
        assert_eq!(dequantise(7, 5000), dequantise(7, 5000), "fixed per player");
    }
}

#[cfg(test)]
mod fence_tests {
    use super::*;

    #[test]
    fn a_fence_naps_after_the_threshold_on_every_other_poll() {
        let mut s = (0u32, 0u32);
        let (t, nap) = fence_nap(s, 100);
        s = t;
        assert!(!nap, "the read pointer moved: run on");
        for i in 1..FENCE_NAP_AFTER {
            let (t, nap) = fence_nap(s, 100);
            s = t;
            assert!(!nap, "unmoved poll {i} runs on: a short wait never naps");
        }
        let (s, nap) = fence_nap(s, 100);
        assert!(nap, "poll {FENCE_NAP_AFTER} naps");
        assert_eq!(s, (100, FENCE_NAP_AFTER));
        let (s, nap) = fence_nap(s, 100);
        assert!(!nap, "the poll after a nap runs the watchdog and the caller's own test");
        let (s, nap) = fence_nap(s, 100);
        assert!(nap, "then it naps again");
        let (s, nap) = fence_nap(s, 104);
        assert!(!nap && s == (104, 0), "progress resets the count");
    }

    /// LMA_FENCE_NAP unset is the native wait in real time and no hook at all in a deterministic run
    /// (fingerprints unchanged); `0` turns it off everywhere, and the explicit modes win over both.
    #[test]
    fn the_fence_wait_is_the_real_time_default() {
        assert_eq!(super::fence_nap_mode(None, false), Some(3), "real time: the native wait");
        assert_eq!(super::fence_nap_mode(None, true), None, "deterministic: no hook");
        assert_eq!(super::fence_nap_mode(Some("0"), false), None, "off");
        assert_eq!(super::fence_nap_mode(Some("1"), true), Some(1), "the timed nap, asked for");
        assert_eq!(super::fence_nap_mode(Some("wait"), true), Some(3));
        assert_eq!(super::fence_nap_mode(Some("2"), false), Some(2), "observe only");
    }

    /// A custom threshold (LMA_FENCE_NAP_AFTER) moves the first nap; every other unmoved poll after it
    /// naps, and progress resets the count, as with the default.
    #[test]
    fn a_custom_threshold_moves_the_first_nap() {
        let mut s = (0u32, 0u32);
        let mut naps = Vec::new();
        for i in 0..12u32 {
            let (t, nap) = fence_nap_after(s, 7, 8);
            s = t;
            if nap {
                naps.push(i);
            }
        }
        assert_eq!(naps, vec![8, 10], "unmoved polls 8 and 10 (the first poll only records the pointer)");
        let (s, nap) = fence_nap_after(s, 9, 8);
        assert!(!nap && s == (9, 0), "progress resets the count");
    }

    /// The hook on a Machine: it reads the wait state's device, the ring and the read pointer,
    /// never handles the call (the watchdog runs), keeps the per-thread poll state and asks for a
    /// nap only once the threshold of unmoved polls is reached.
    #[test]
    fn the_watchdog_hook_declines_and_naps_after_the_threshold() {
        std::env::set_var("LMA_FENCE_NAP", "1");
        let x = xex::Xex { module_flags: 0, entry_point: 0x8200_0000, image_base: 0x8200_0000, image: vec![0; 0x1000], libraries: vec![], used_devkit_key: false, resources: vec![] };
        let dir = std::env::temp_dir();
        let mut m = Machine::new(&x, &dir, &dir);
        m.deterministic = true;
        let (state, device, ring) = (0x1000u32, 0x2000u32, 0x3000u32);
        m.mem.write32(state, device);
        m.mem.write32(device + 0x2a10, ring);
        m.mem.write32(ring + 0x3c, 0x67);
        m.cpu.gpr[3] = state as u64;
        // the first poll records the read pointer; then FENCE_NAP_AFTER unmoved polls run on
        for i in 0..FENCE_NAP_AFTER {
            assert!(!fence_watchdog(&mut m), "the watchdog itself runs");
            assert!(m.nap_until.is_none(), "poll {i}: no nap before the threshold");
        }
        assert!(!fence_watchdog(&mut m));
        assert_eq!(m.nap_until, Some(m.now() + FENCE_NAP_TICKS), "unmoved poll {FENCE_NAP_AFTER}: nap");
        assert_eq!(m.fence_polls.get(&m.cur).copied(), Some((0x67, FENCE_NAP_AFTER)));
        m.nap_until = None;
        m.mem.write32(ring + 0x3c, 0x6f); // the GPU moved on
        assert!(!fence_watchdog(&mut m));
        assert!(m.nap_until.is_none(), "a moved read pointer resets the count");
        assert_eq!(m.fence_polls.get(&m.cur).copied(), Some((0x6f, 0)));
        m.mem.write32(device + 0x2a10, 0); // no ring yet (device not up): nothing recorded
        m.fence_polls.clear();
        assert!(!fence_watchdog(&mut m) && m.fence_polls.is_empty());
    }
}

/// A tactics record (1472 bytes) keeps each formation's player-position offsets: x at word
/// 21 + formation x 11 + player, y at word 175 + ..., 14 formations x 11 floats each, filling the
/// record to byte 0x524 with other saved data after it. Formations 14 and 15 (mod formations.csv)
/// have no room there: the game would read the y array and the data past it as their offsets
/// (the pitch diagram drew them off the pitch, #749) and a position edit would write into them. The
/// four accessors (r4 = formation, r5 = player) and the diagram's slot placement (r3 = formation,
/// reading the arrays directly) are wrapped, when the mod has more than 14 formations: past 14
/// the getters return the game's "no offset" (0.0, the constant they return for a player past
/// 10), the setters do nothing and the placement runs with the record's "no custom positions"
/// byte (+0x37) set for the call.
pub const FORMATION_OFFSET_X_GET: u32 = 0x8210_F330;
pub const FORMATION_OFFSET_X_SET: u32 = 0x8210_F3F8;
pub const FORMATION_OFFSET_Y_GET: u32 = 0x8210_F4A8;
pub const FORMATION_OFFSET_Y_SET: u32 = 0x8210_F570;
pub const FORMATION_DIAGRAM_SLOT: u32 = 0x8219_1B40;
/// The getters' default: `lfs f1,0x968(0x82000000)`.
const FORMATION_OFFSET_ZERO: u32 = 0x8200_0968;
/// The diagram's state (`addi r31,r10,-12760` at 0x82191b74): its word +8 is the record shown.
const FORMATION_DIAGRAM_STATE: u32 = 0x8274_CE28;
static FORMATION_OFFSET_GAME: std::sync::OnceLock<[guest::rec::FuncFn; 5]> = std::sync::OnceLock::new();

/// The five wrapped addresses with their blocks, given the game's blocks in the same order.
pub fn formation_offset_blocks_for(game: [guest::rec::FuncFn; 5]) -> [(u32, guest::rec::FuncFn); 5] {
    let _ = FORMATION_OFFSET_GAME.set(game);
    [
        (FORMATION_OFFSET_X_GET, formation_offset_x_get),
        (FORMATION_OFFSET_X_SET, formation_offset_x_set),
        (FORMATION_OFFSET_Y_GET, formation_offset_y_get),
        (FORMATION_OFFSET_Y_SET, formation_offset_y_set),
        (FORMATION_DIAGRAM_SLOT, formation_diagram_slot),
    ]
}

fn formation_past_vanilla(f: u64) -> bool {
    f as u32 >= guest::rec::FORMATION_COUNT_VANILLA
}

fn formation_offset_game(k: usize) -> guest::rec::FuncFn {
    let Some(g) = FORMATION_OFFSET_GAME.get() else { unreachable!("the formation offset blocks are only installed through formation_offset_blocks_for") };
    g[k]
}

fn formation_offset_get(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64, k: usize) -> Result<u32, guest::cpu::Stop> {
    if formation_past_vanilla(c.gpr[4]) {
        c.fpr[1] = f32::from_bits(m.read32(FORMATION_OFFSET_ZERO)) as f64;
        return Ok(c.lr as u32 & !3);
    }
    call_to_the_end(c, m, pc, b, formation_offset_game(k))
}

fn formation_offset_set(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64, k: usize) -> Result<u32, guest::cpu::Stop> {
    if formation_past_vanilla(c.gpr[4]) {
        return Ok(c.lr as u32 & !3);
    }
    call_to_the_end(c, m, pc, b, formation_offset_game(k))
}

fn formation_offset_x_get(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    formation_offset_get(c, m, pc, b, 0)
}
fn formation_offset_x_set(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    formation_offset_set(c, m, pc, b, 1)
}
fn formation_offset_y_get(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    formation_offset_get(c, m, pc, b, 2)
}
fn formation_offset_y_set(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    formation_offset_set(c, m, pc, b, 3)
}

fn formation_diagram_slot(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let game = formation_offset_game(4);
    let record = m.read32(FORMATION_DIAGRAM_STATE + 8);
    if !formation_past_vanilla(c.gpr[3]) || record == 0 {
        return call_to_the_end(c, m, pc, b, game);
    }
    let flag = m.read8(record + 0x37);
    m.write8(record + 0x37, 1);
    let r = call_to_the_end(c, m, pc, b, game);
    m.write8(record + 0x37, flag);
    r
}
