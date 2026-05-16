//! Linux-side IP allocation surface.
//!
//! Re-exports the shared `allocate`/`release` allocator from
//! `ember_core::network::ip` and adds the Linux-specific default-subnet
//! derivation that picks a `/16` slot inside `10.0.0.0/8` from the
//! installation's instance id. macOS has its own derivation that
//! sub-allocates inside vmnet's fixed `192.168.64.0/24` and lives in
//! the `ember-macos` crate.

pub use ember_core::network::ip::*;

use ember_core::config::fnv1a_32;

/// Derive a default `/16` IPv4 subnet from an instance id, chosen so
/// two installations on the same host rarely overlap:
/// `10.{slot}.0.0/16` where `slot` is the high byte of an FNV-1a hash
/// of the id. The /16 still gives ~16k VMs per install via /30 P2P
/// links — well above any realistic personal-use workload.
pub fn derive_default_subnet(instance_id: &str) -> String {
    let hash = fnv1a_32(instance_id.as_bytes());
    let slot = ((hash >> 8) & 0xff) as u8;
    format!("10.{slot}.0.0/16")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_subnet_lands_in_10_slash_8() {
        let subnet = derive_default_subnet("a3f4");
        assert!(subnet.starts_with("10."));
        assert!(subnet.ends_with(".0.0/16"));
    }

    #[test]
    fn derivation_is_stable() {
        // Reactivation must land on the same /16 the install was
        // initialized with, otherwise allocations.json's pinned
        // `base_subnet` mismatches and `allocate` refuses to hand out
        // IPs.
        assert_eq!(derive_default_subnet("a3f4"), derive_default_subnet("a3f4"));
    }
}
