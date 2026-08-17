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

/// Placeholder for a figure the storage backend could not measure.
///
/// Distinct from a measured zero, which renders as `0 B`.
pub const UNKNOWN: &str = "-";

/// [`format_bytes_binary`] for a value the backend may not know.
pub fn format_bytes_opt(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| UNKNOWN.to_string(), format_bytes_binary)
}

/// Format a compression ratio as `2.01x`.
pub fn format_ratio(ratio: Option<f64>) -> String {
    ratio.map_or_else(|| UNKNOWN.to_string(), |r| format!("{r:.2}x"))
}

/// Format a fill level as a whole percentage. A zero-capacity pool
/// reads as 0% rather than dividing by zero.
pub fn format_percent(used: u64, capacity: u64) -> String {
    if capacity == 0 {
        return "0%".to_string();
    }
    format!("{:.0}%", (used as f64 / capacity as f64) * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_values_render_as_dash() {
        assert_eq!(format_bytes_opt(None), "-");
        assert_eq!(format_ratio(None), "-");
        // A measured zero must stay distinguishable from unknown.
        assert_eq!(format_bytes_opt(Some(0)), "0 B");
    }

    #[test]
    fn ratios_keep_two_decimals() {
        assert_eq!(format_ratio(Some(2.0)), "2.00x");
        assert_eq!(format_ratio(Some(1.9666)), "1.97x");
    }

    #[test]
    fn percentages_round_and_guard_zero_capacity() {
        assert_eq!(format_percent(0, 0), "0%");
        assert_eq!(format_percent(1, 4), "25%");
        assert_eq!(format_percent(2, 3), "67%");
    }

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
