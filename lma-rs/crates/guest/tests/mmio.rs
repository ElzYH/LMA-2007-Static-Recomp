//! The XMA command queue (`guest::mmio`, #199): stores to the command registers are queued in
//! order while the trap is on, and nothing is recorded with it off.

use guest::mmio::{set_xma_trap, store_le, take_xma_commands};

#[test]
fn queues_command_stores_only_while_on() {
    store_le(0x7FEA_1940, 1); // off: nothing recorded
    set_xma_trap(true);
    store_le(0x7FEA_1A40, 2);
    store_le(0x7FEA_1940, 4);
    store_le(0x7FEA_1800, 8); // not a command register
    store_le(0x7FEA_1AB0, 16); // just past the range
    assert_eq!(take_xma_commands(), [(0x7FEA_1A40, 2), (0x7FEA_1940, 4)]);
    assert!(take_xma_commands().is_empty());
    set_xma_trap(false);
    store_le(0x7FEA_1940, 1);
    assert!(take_xma_commands().is_empty());
}
