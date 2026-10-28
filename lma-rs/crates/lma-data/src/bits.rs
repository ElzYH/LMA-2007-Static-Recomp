//! Bit fields of a record: MSB-first over the Xbox 360's big-endian records (players are 320-bit
//! records), LSB-first over the PS2's little-endian ones (`get_le`, #3).

/// Read `width` bits starting at bit `pos` (bit 0 = MSB of byte 0).
pub fn get(rec: &[u8], pos: usize, width: usize) -> u32 {
    let mut v = 0u32;
    for b in pos..pos + width {
        let bit = (rec[b / 8] >> (7 - b % 8)) & 1;
        v = (v << 1) | bit as u32;
    }
    v
}

/// Write `width` bits at `pos`. Panics if `val` does not fit; callers validate first.
pub fn set(rec: &mut [u8], pos: usize, width: usize, val: u32) {
    assert!(width == 32 || val < (1u32 << width), "{val} does not fit in {width} bits");
    for (i, b) in (pos..pos + width).enumerate() {
        let bit = (val >> (width - 1 - i)) & 1;
        let byte = &mut rec[b / 8];
        let mask = 1u8 << (7 - b % 8);
        if bit == 1 {
            *byte |= mask
        } else {
            *byte &= !mask
        }
    }
}

/// Read `width` bits starting at bit `pos` of a little-endian record (bit 0 = LSB of byte 0;
/// the field's low bit first), as the PS2's compiler lays out bit fields.
pub fn get_le(rec: &[u8], pos: usize, width: usize) -> u32 {
    let mut v = 0u32;
    for i in 0..width {
        let b = pos + i;
        v |= (((rec[b / 8] >> (b % 8)) & 1) as u32) << i;
    }
    v
}

/// Write `width` bits at `pos` of a little-endian record. Panics if `val` does not fit.
pub fn set_le(rec: &mut [u8], pos: usize, width: usize, val: u32) {
    assert!(width == 32 || val < (1u32 << width), "{val} does not fit in {width} bits");
    for i in 0..width {
        let b = pos + i;
        let mask = 1u8 << (b % 8);
        if (val >> i) & 1 == 1 {
            rec[b / 8] |= mask
        } else {
            rec[b / 8] &= !mask
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let mut r = [0u8; 40];
        super::set(&mut r, 3, 15, 19425);
        super::set(&mut r, 183, 5, 20);
        assert_eq!(super::get(&r, 3, 15), 19425);
        assert_eq!(super::get(&r, 183, 5), 20);
        assert_eq!(super::get(&r, 0, 3), 0);
    }

    /// LSB-first fields are the bits of the record read as one little-endian integer.
    #[test]
    fn little_endian_fields() {
        let mut r = [0u8; 40];
        super::set_le(&mut r, 18, 14, 693);
        super::set_le(&mut r, 0, 15, 3920);
        super::set_le(&mut r, 183, 5, 20);
        assert_eq!(u32::from_le_bytes(r[0..4].try_into().unwrap()), 693 << 18 | 3920);
        assert_eq!(super::get_le(&r, 18, 14), 693);
        assert_eq!(super::get_le(&r, 0, 15), 3920);
        assert_eq!(super::get_le(&r, 183, 5), 20);
        assert_eq!(super::get_le(&r, 15, 3), 0);
    }
}
