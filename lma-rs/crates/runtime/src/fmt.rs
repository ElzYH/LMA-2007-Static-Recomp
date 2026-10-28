//! printf-style formatting for the guest's sprintf/_snprintf/_vsnprintf/DbgPrint.
//! Arguments come as 8-byte slots (va_list on the 360 points at a list of 64-bit values).

use crate::mem::Memory;

pub fn format(mem: &mut Memory, fmt_ptr: u32, mut next_arg: impl FnMut(&mut Memory) -> u64) -> String {
    let f = mem.cstr(fmt_ptr, 4096).into_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < f.len() {
        if f[i] != b'%' {
            out.push(f[i] as char);
            i += 1;
            continue;
        }
        i += 1;
        let mut flags = String::new();
        while i < f.len() && b"-+ #0".contains(&f[i]) {
            flags.push(f[i] as char);
            i += 1;
        }
        let mut width = 0usize;
        while i < f.len() && f[i].is_ascii_digit() {
            width = width * 10 + (f[i] - b'0') as usize;
            i += 1;
        }
        if i < f.len() && f[i] == b'*' {
            width = next_arg(mem) as usize;
            i += 1;
        }
        let mut prec: Option<usize> = None;
        if i < f.len() && f[i] == b'.' {
            i += 1;
            let mut p = 0;
            while i < f.len() && f[i].is_ascii_digit() {
                p = p * 10 + (f[i] - b'0') as usize;
                i += 1;
            }
            prec = Some(p);
        }
        // size modifiers: values are full 64-bit slots; ll, q and I64 print all 64 bits
        let mut wide = false;
        while i < f.len() && b"hlLIq".contains(&f[i]) {
            wide |= f[i] == b'q' || (f[i] == b'l' && f.get(i + 1) == Some(&b'l')) || (f[i] == b'I' && f.get(i + 1..i + 3) == Some(&b"64"[..]));
            i += if f[i] == b'I' && f.get(i + 1..i + 3) == Some(&b"64"[..]) { 3 } else { 1 };
        }
        if i >= f.len() {
            break;
        }
        let conv = f[i] as char;
        i += 1;
        let s = match conv {
            '%' => "%".to_string(),
            'd' | 'i' if wide => (next_arg(mem) as i64).to_string(),
            'd' | 'i' => (next_arg(mem) as u32 as i32).to_string(),
            'u' if wide => next_arg(mem).to_string(),
            'u' => (next_arg(mem) as u32).to_string(),
            'x' if wide => format!("{:x}", next_arg(mem)),
            'x' => format!("{:x}", next_arg(mem) as u32),
            'X' if wide => format!("{:X}", next_arg(mem)),
            'X' => format!("{:X}", next_arg(mem) as u32),
            'p' => format!("{:08X}", next_arg(mem) as u32),
            'c' => ((next_arg(mem) as u8) as char).to_string(),
            's' => {
                let p = next_arg(mem) as u32;
                if p == 0 { "(null)".into() } else { mem.cstr(p, prec.unwrap_or(4096)) }
            }
            'f' | 'g' | 'e' => format!("{:.*}", prec.unwrap_or(6), f64::from_bits(next_arg(mem))),
            other => format!("%{other}"),
        };
        let pad = if flags.contains('0') && !flags.contains('-') { '0' } else { ' ' };
        if s.len() < width {
            let fill: String = std::iter::repeat_n(pad, width - s.len()).collect();
            if flags.contains('-') {
                out.push_str(&s);
                out.push_str(&fill)
            } else if pad == '0' && s.starts_with('-') {
                // zeros go after the sign: %05d of -42 is -0042
                out.push('-');
                out.push_str(&fill);
                out.push_str(&s[1..])
            } else {
                out.push_str(&fill);
                out.push_str(&s)
            }
        } else {
            out.push_str(&s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Formats `fmt` (written into guest memory) with `args` as the 64-bit slots.
    fn run(fmt: &str, args: &[u64]) -> String {
        let mut mem = Memory::new();
        let f = 0x4000_0000;
        mem.write(f, fmt.as_bytes());
        mem.write8(f + fmt.len() as u32, 0);
        let mut it = args.iter().copied();
        format(&mut mem, f, |_| it.next().unwrap_or(0))
    }

    #[test]
    fn integers_and_padding() {
        assert_eq!(run("%d/%i/%u", &[(-5i64) as u64, 7, 0xFFFF_FFFF]), "-5/7/4294967295");
        assert_eq!(run("%02d:%02d", &[9, 5]), "09:05");
        assert_eq!(run("[%5d][%-5d]", &[42, 42]), "[   42][42   ]");
        assert_eq!(run("%05d", &[(-42i64) as u64]), "-0042", "zeros after the sign");
        assert_eq!(run("%x %X %08X %p", &[255, 255, 0xBEEF, 0x8200_1000]), "ff FF 0000BEEF 82001000");
        assert_eq!(run("%*d", &[4, 7]), "   7", "width from an argument");
        assert_eq!(run("100%%", &[]), "100%");
    }

    #[test]
    fn sixty_four_bit() {
        let big = 0x1_0000_0002u64;
        assert_eq!(run("%d %ld", &[big, big]), "2 2", "plain and l: 32 bits");
        assert_eq!(run("%lld %I64d %qd", &[big, big, big]), "4294967298 4294967298 4294967298");
        assert_eq!(run("%llx %I64X", &[big, big]), "100000002 100000002");
        assert_eq!(run("%lld", &[(-3i64) as u64]), "-3");
    }

    #[test]
    fn strings_chars_floats() {
        let mut mem = Memory::new();
        mem.write(0x4000_1000, b"Arsenal\0");
        let f = 0x4000_0000;
        let fmt = "%s|%.3s|%c|%s";
        mem.write(f, fmt.as_bytes());
        mem.write8(f + fmt.len() as u32, 0);
        let mut args = [0x4000_1000u64, 0x4000_1000, b'X' as u64, 0].into_iter();
        assert_eq!(format(&mut mem, f, |_| args.next().unwrap()), "Arsenal|Ars|X|(null)");
        assert_eq!(run("%.2f %f", &[1.5f64.to_bits(), 0.25f64.to_bits()]), "1.50 0.250000");
    }
}
