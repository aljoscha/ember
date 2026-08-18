//! Human-readable byte sizes with mandatory unit suffixes.
//!
//! Parses strings like `"512M"`, `"16G"`, `"2T"` into a [`ByteSize`] value.
//! Bare integers are rejected — a unit suffix is always required.
//!
//! Accepted suffixes (case-insensitive, binary / powers of 1024):
//! `K` / `KiB`, `M` / `MiB`, `G` / `GiB`, `T` / `TiB`.

use std::fmt;
use std::str::FromStr;

use serde::de;

/// A parsed size value with mandatory unit suffix.
///
/// Internally stores the value in bytes (`u64`).  Use [`to_mib`](ByteSize::to_mib)
/// or [`to_gib`](ByteSize::to_gib) to convert at module boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize {
    bytes: u64,
}

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

impl ByteSize {
    /// Construct from a count of mebibytes.  Usable in `const` context.
    pub const fn from_mib(mib: u64) -> Self {
        Self { bytes: mib * MIB }
    }

    /// Construct from a count of gibibytes.  Usable in `const` context.
    pub const fn from_gib(gib: u64) -> Self {
        Self { bytes: gib * GIB }
    }

    /// Construct from a raw byte count. Usable in `const` context.
    ///
    /// The parser only accepts whole units, so this is the way to
    /// express a size that is not one, which is mostly sizes that came
    /// from a device rather than from a person.
    pub const fn from_bytes(bytes: u64) -> Self {
        Self { bytes }
    }

    /// Raw byte count.
    pub fn bytes(self) -> u64 {
        self.bytes
    }

    /// Convert to whole mebibytes.
    ///
    /// Returns an error if the value is not evenly divisible by 1 MiB or
    /// exceeds `u32::MAX` MiB.
    pub fn to_mib(self) -> Result<u32, String> {
        if !self.bytes.is_multiple_of(MIB) {
            return Err(format!("{self} is not a whole number of MiB",));
        }
        let mib = self.bytes / MIB;
        u32::try_from(mib).map_err(|_| format!("{self} exceeds maximum of {} MiB", u32::MAX))
    }

    /// Convert to whole gibibytes.
    ///
    /// Returns an error if the value is not evenly divisible by 1 GiB or
    /// exceeds `u32::MAX` GiB.
    pub fn to_gib(self) -> Result<u32, String> {
        if !self.bytes.is_multiple_of(GIB) {
            return Err(format!("{self} is not a whole number of GiB",));
        }
        let gib = self.bytes / GIB;
        u32::try_from(gib).map_err(|_| format!("{self} exceeds maximum of {} GiB", u32::MAX))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.bytes == 0 {
            return write!(f, "0M");
        }
        if self.bytes.is_multiple_of(TIB) {
            write!(f, "{}T", self.bytes / TIB)
        } else if self.bytes.is_multiple_of(GIB) {
            write!(f, "{}G", self.bytes / GIB)
        } else if self.bytes.is_multiple_of(MIB) {
            write!(f, "{}M", self.bytes / MIB)
        } else if self.bytes.is_multiple_of(KIB) {
            write!(f, "{}K", self.bytes / KIB)
        } else {
            write!(f, "{}B", self.bytes)
        }
    }
}

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty size string".to_string());
        }

        // Find the boundary between the numeric prefix and the unit suffix.
        let num_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());

        let num_part = &s[..num_end];
        let suffix = s[num_end..].trim();

        if suffix.is_empty() {
            return Err(format!(
                "bare number '{s}' requires a unit suffix (e.g., {s}M or {s}G)"
            ));
        }

        if num_part.is_empty() {
            return Err(format!("missing number before unit suffix '{suffix}'"));
        }

        let number: u64 = num_part
            .parse()
            .map_err(|_| format!("invalid number '{num_part}'"))?;

        let multiplier = match suffix.to_ascii_lowercase().as_str() {
            "k" | "kib" => KIB,
            "m" | "mib" => MIB,
            "g" | "gib" => GIB,
            "t" | "tib" => TIB,
            _ => return Err(format!("unknown unit '{suffix}' — use K, M, G, or T")),
        };

        number
            .checked_mul(multiplier)
            .ok_or_else(|| format!("{num_part}{suffix} overflows maximum representable size"))
            .map(|bytes| ByteSize { bytes })
    }
}

// ---------------------------------------------------------------------------
// Serde support
// ---------------------------------------------------------------------------

impl<'de> de::Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = ByteSize;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "a size string with unit suffix (e.g., \"512M\", \"8G\")")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse().map_err(de::Error::custom)
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Err(de::Error::custom(format!(
                    "bare number {v} requires a unit suffix — use \"{v}M\" for MiB or \"{v}G\" for GiB"
                )))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Err(de::Error::custom(format!(
                    "bare number {v} requires a unit suffix — use \"{v}M\" for MiB or \"{v}G\" for GiB"
                )))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- FromStr: valid inputs --

    #[test]
    fn parse_mib() {
        let s: ByteSize = "512M".parse().unwrap();
        assert_eq!(s.bytes(), 512 * MIB);
        assert_eq!(s.to_mib().unwrap(), 512);
    }

    #[test]
    fn parse_gib() {
        let s: ByteSize = "8G".parse().unwrap();
        assert_eq!(s.bytes(), 8 * GIB);
        assert_eq!(s.to_gib().unwrap(), 8);
    }

    #[test]
    fn parse_tib() {
        let s: ByteSize = "2T".parse().unwrap();
        assert_eq!(s.bytes(), 2 * TIB);
    }

    #[test]
    fn parse_kib() {
        let s: ByteSize = "1024K".parse().unwrap();
        assert_eq!(s.to_mib().unwrap(), 1);
    }

    #[test]
    fn parse_long_suffix() {
        assert_eq!("16GiB".parse::<ByteSize>().unwrap().to_gib().unwrap(), 16);
        assert_eq!("512MiB".parse::<ByteSize>().unwrap().to_mib().unwrap(), 512);
        assert_eq!("1024KiB".parse::<ByteSize>().unwrap().to_mib().unwrap(), 1);
        assert_eq!("1TiB".parse::<ByteSize>().unwrap().bytes(), TIB);
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!("512m".parse::<ByteSize>().unwrap().to_mib().unwrap(), 512);
        assert_eq!("8g".parse::<ByteSize>().unwrap().to_gib().unwrap(), 8);
        assert_eq!("2t".parse::<ByteSize>().unwrap().bytes(), 2 * TIB);
        assert_eq!("1024k".parse::<ByteSize>().unwrap().to_mib().unwrap(), 1);
    }

    #[test]
    fn parse_zero() {
        let s: ByteSize = "0M".parse().unwrap();
        assert_eq!(s.bytes(), 0);
        assert_eq!(s.to_mib().unwrap(), 0);
    }

    // -- FromStr: invalid inputs --

    #[test]
    fn reject_bare_integer() {
        let err = "512".parse::<ByteSize>().unwrap_err();
        assert!(err.contains("requires a unit suffix"), "got: {err}");
    }

    #[test]
    fn reject_empty() {
        assert!("".parse::<ByteSize>().is_err());
    }

    #[test]
    fn reject_suffix_only() {
        let err = "M".parse::<ByteSize>().unwrap_err();
        assert!(err.contains("missing number"), "got: {err}");
    }

    #[test]
    fn reject_unknown_suffix() {
        let err = "512X".parse::<ByteSize>().unwrap_err();
        assert!(err.contains("unknown unit"), "got: {err}");
    }

    #[test]
    fn reject_decimal_suffix() {
        // MB/GB are ambiguous (decimal vs binary) — reject them.
        assert!("512MB".parse::<ByteSize>().is_err());
        assert!("8GB".parse::<ByteSize>().is_err());
    }

    #[test]
    fn reject_float() {
        assert!("1.5G".parse::<ByteSize>().is_err());
    }

    #[test]
    fn reject_negative() {
        assert!("-1G".parse::<ByteSize>().is_err());
    }

    #[test]
    fn reject_overflow() {
        let err = "99999999999T".parse::<ByteSize>().unwrap_err();
        assert!(err.contains("overflows"), "got: {err}");
    }

    // -- Conversion --

    #[test]
    fn to_mib_not_divisible() {
        let s: ByteSize = "500K".parse().unwrap();
        let err = s.to_mib().unwrap_err();
        assert!(err.contains("not a whole number of MiB"), "got: {err}");
    }

    #[test]
    fn to_gib_not_divisible() {
        let s: ByteSize = "1500M".parse().unwrap();
        let err = s.to_gib().unwrap_err();
        assert!(err.contains("not a whole number of GiB"), "got: {err}");
    }

    #[test]
    fn gib_to_mib() {
        let s: ByteSize = "2G".parse().unwrap();
        assert_eq!(s.to_mib().unwrap(), 2048);
    }

    // -- Display --

    #[test]
    fn display_natural_unit() {
        assert_eq!(ByteSize::from_gib(16).to_string(), "16G");
        assert_eq!(ByteSize::from_mib(512).to_string(), "512M");
        assert_eq!(ByteSize::from_mib(2048).to_string(), "2G");
        assert_eq!(ByteSize { bytes: 0 }.to_string(), "0M");
    }

    // -- Serde --

    #[test]
    fn serde_string() {
        let val: ByteSize = serde_yaml::from_str("\"512M\"").unwrap();
        assert_eq!(val.to_mib().unwrap(), 512);
    }

    #[test]
    fn serde_unquoted_string() {
        // YAML treats "512M" (unquoted) as a string because it contains letters.
        let val: ByteSize = serde_yaml::from_str("512M").unwrap();
        assert_eq!(val.to_mib().unwrap(), 512);
    }

    #[test]
    fn serde_reject_bare_integer() {
        let err = serde_yaml::from_str::<ByteSize>("512").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("requires a unit suffix"), "got: {msg}");
    }
}
