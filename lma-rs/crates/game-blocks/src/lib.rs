//! Placeholder: run `lma-recomp <default.xex> all crates/game-blocks 999999 32` to generate the
//! recompiled game code from your own copy. Without it the runtime interprets everything.
//!
//! The parts are empty stubs (`stubs/`) with the same names, versions and dependencies as the
//! 32 parts lma-recomp writes, so `Cargo.lock` is identical for both builds.
use guest::rec::FuncFn;

/// Every part's (block leader pc, recompiled function) table.
pub static PARTS: &[&[(u32, FuncFn)]] = &[
    gb00::BLOCKS,
    gb01::BLOCKS,
    gb02::BLOCKS,
    gb03::BLOCKS,
    gb04::BLOCKS,
    gb05::BLOCKS,
    gb06::BLOCKS,
    gb07::BLOCKS,
    gb08::BLOCKS,
    gb09::BLOCKS,
    gb10::BLOCKS,
    gb11::BLOCKS,
    gb12::BLOCKS,
    gb13::BLOCKS,
    gb14::BLOCKS,
    gb15::BLOCKS,
    gb16::BLOCKS,
    gb17::BLOCKS,
    gb18::BLOCKS,
    gb19::BLOCKS,
    gb20::BLOCKS,
    gb21::BLOCKS,
    gb22::BLOCKS,
    gb23::BLOCKS,
    gb24::BLOCKS,
    gb25::BLOCKS,
    gb26::BLOCKS,
    gb27::BLOCKS,
    gb28::BLOCKS,
    gb29::BLOCKS,
    gb30::BLOCKS,
    gb31::BLOCKS,
];
