//! Xbox 360 system export names by ordinal (tables derived from Xenia, see data/SOURCE.txt).

const XBOXKRNL: &str = include_str!("../data/xboxkrnl.txt");
const XAM: &str = include_str!("../data/xam.txt");

/// (name, is_variable) for `library` ("xboxkrnl.exe" / "xam.xex") and `ordinal`.
pub fn lookup(library: &str, ordinal: u32) -> Option<(&'static str, bool)> {
    let table = if library.starts_with("xboxkrnl") {
        XBOXKRNL
    } else if library.starts_with("xam") {
        XAM
    } else {
        return None;
    };
    table.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let ord = u32::from_str_radix(it.next()?.trim_start_matches("0x"), 16).ok()?;
        (ord == ordinal).then(|| (it.next().unwrap_or("?"), it.next() == Some("Variable")))
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn known() {
        assert_eq!(super::lookup("xboxkrnl.exe", 3), Some(("DbgPrint", false)));
        assert_eq!(super::lookup("xam.xex", 1).map(|t| t.0), Some("NetDll_WSAStartup"));
    }
}
