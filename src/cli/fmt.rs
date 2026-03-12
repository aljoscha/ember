pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;

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
