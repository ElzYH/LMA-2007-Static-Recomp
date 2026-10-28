//! Minimal CSV (RFC 4180: quoted cells, doubled quotes, CRLF or LF), enough for the editor's
//! spreadsheets. A UTF-8 byte order mark (Excel's "CSV UTF-8") is skipped.

use crate::{Error, Result};

/// A CSV file: header row plus data rows, with cells looked up by column name.
pub struct Sheet {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Sheet {
    pub fn parse(text: &str) -> Result<Self> {
        let mut rows = parse_rows(text.strip_prefix('\u{feff}').unwrap_or(text))?;
        if rows.is_empty() {
            return Err(Error::Format("empty CSV".into()));
        }
        let header = rows.remove(0).into_iter().map(|h| h.trim().to_string()).collect();
        rows.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
        Ok(Self { header, rows })
    }

    pub fn read(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text).map_err(|e| Error::Format(format!("{}: {e}", path.display())))
    }

    pub fn col(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|h| h == name)
    }

    /// The trimmed cell, or None when the column is missing or the cell is blank.
    pub fn get<'a>(&self, row: &'a [String], col: Option<usize>) -> Option<&'a str> {
        let v = row.get(col?)?.trim();
        (!v.is_empty()).then_some(v)
    }
}

fn parse_rows(text: &str) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (true, c) => cell.push(c),
            (false, '"') if cell.is_empty() => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut cell)),
            (false, '\r') if chars.peek() == Some(&'\n') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            (false, c) => cell.push(c),
        }
    }
    if quoted {
        return Err(Error::Format("unterminated quote".into()));
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    Ok(rows)
}

/// Append one CSV line (CRLF, like Python's csv module), quoting cells that need it.
pub fn write_row<S: AsRef<str>>(out: &mut String, cells: &[S]) {
    for (i, c) in cells.iter().enumerate() {
        let c = c.as_ref();
        if i > 0 {
            out.push(',');
        }
        if c.contains([',', '"', '\n', '\r']) {
            out.push('"');
            out.push_str(&c.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(c);
        }
    }
    out.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_bom_and_line_endings() {
        let s = Sheet::parse("\u{feff}id,name\r\n1,\"Smith, \"\"Jr\"\"\"\r\n\r\n2,plain\n3,\"two\nlines\"").unwrap();
        assert_eq!(s.header, ["id", "name"]);
        assert_eq!(s.rows, [vec!["1", "Smith, \"Jr\""], vec!["2", "plain"], vec!["3", "two\nlines"]]);
        assert_eq!(s.get(&s.rows[0], s.col("name")), Some("Smith, \"Jr\""));
        assert_eq!(s.get(&s.rows[0], s.col("missing")), None);
    }

    #[test]
    fn write_then_parse() {
        let mut out = String::new();
        write_row(&mut out, &["id", "name"]);
        write_row(&mut out, &["7", "a,\"b\""]);
        let s = Sheet::parse(&out).unwrap();
        assert_eq!(s.rows, [vec!["7", "a,\"b\""]]);
    }
}
