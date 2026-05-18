pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;

/// Column alignment for [`print_table`].
#[derive(Clone, Copy)]
pub enum Align {
    Left,
    Right,
}

/// Print a table whose column widths size to fit the data.
///
/// Each column's width is `max(header_len, max(cell_len))`. Columns are
/// separated by a single space. Trailing whitespace is omitted on the
/// rightmost column when it is left-aligned.
pub fn print_table(headers: &[&str], aligns: &[Align], rows: &[Vec<String>]) {
    debug_assert_eq!(headers.len(), aligns.len());
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        debug_assert_eq!(row.len(), headers.len());
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    print_row(headers.iter().copied(), &widths, aligns);
    for row in rows {
        print_row(row.iter().map(String::as_str), &widths, aligns);
    }
}

fn print_row<'a>(cells: impl Iterator<Item = &'a str>, widths: &[usize], aligns: &[Align]) {
    let cells: Vec<&str> = cells.collect();
    let last = cells.len().saturating_sub(1);
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let w = widths[i];
        match aligns[i] {
            Align::Left if i == last => line.push_str(cell),
            Align::Left => line.push_str(&format!("{cell:<w$}")),
            Align::Right => line.push_str(&format!("{cell:>w$}")),
        }
    }
    println!("{line}");
}

/// Format a byte count as a human-readable string using binary units
/// (powers of 1,024): GiB, MiB, KiB, B.
///
/// Strips trailing `.0` on whole values (e.g., `512 MiB` not `512.0 MiB`).
pub fn format_bytes_binary(bytes: u64) -> String {
    fn fmt(value: f64, unit: &str) -> String {
        let s = format!("{value:.1} {unit}");
        // "512.0 MiB" → "512 MiB"
        s.replace(".0 ", " ")
    }

    if bytes >= GIB {
        fmt(bytes as f64 / GIB as f64, "GiB")
    } else if bytes >= MIB {
        fmt(bytes as f64 / MIB as f64, "MiB")
    } else if bytes >= KIB {
        fmt(bytes as f64 / KIB as f64, "KiB")
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_values_have_no_decimal() {
        assert_eq!(format_bytes_binary(512 * MIB), "512 MiB");
        assert_eq!(format_bytes_binary(8 * GIB), "8 GiB");
        assert_eq!(format_bytes_binary(KIB), "1 KiB");
    }

    #[test]
    fn fractional_values_keep_decimal() {
        assert_eq!(format_bytes_binary(3 * MIB + 200 * KIB), "3.2 MiB");
        assert_eq!(format_bytes_binary(GIB + 512 * MIB), "1.5 GiB");
    }

    #[test]
    fn auto_promotes_unit() {
        assert_eq!(format_bytes_binary(2048 * MIB), "2 GiB");
        assert_eq!(format_bytes_binary(1024 * KIB), "1 MiB");
    }

    #[test]
    fn small_values() {
        assert_eq!(format_bytes_binary(0), "0 B");
        assert_eq!(format_bytes_binary(42), "42 B");
        assert_eq!(format_bytes_binary(1023), "1023 B");
    }
}
