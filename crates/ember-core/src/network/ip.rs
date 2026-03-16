//! IP allocation from a configurable /N subnet in /30 blocks.
//!
//! Each VM gets a point-to-point /30 link: host gets .1, guest gets .2.
//! Allocations are tracked in `allocations.json` via the state store
//! with flock-based locking for concurrent safety.
//!
//! With the default /16 subnet (10.100.0.0/16), this supports ~16,384
//! concurrent VMs.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::state::store::StateStore;

/// Persisted IP allocation state, stored as `allocations.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpAllocations {
    /// Base subnet in CIDR notation (e.g., "10.100.0.0/16").
    pub base_subnet: String,
    /// Map from /30 block index to VM name.
    pub allocations: HashMap<u32, String>,
}

/// A single IP allocation for one VM.
#[derive(Debug, Clone, PartialEq)]
pub struct IpAllocation {
    /// Index of the /30 block within the base subnet.
    pub block_index: u32,
    /// Host-side IP — first usable address in the /30 (e.g., "10.100.0.1").
    pub host_ip: String,
    /// Guest-side IP — second usable address in the /30 (e.g., "10.100.0.2").
    pub guest_ip: String,
    /// Netmask for the /30 link ("255.255.255.252").
    pub netmask: String,
}

/// Default base subnet when none is configured.
pub const DEFAULT_SUBNET: &str = "10.100.0.0/16";

/// Netmask for a /30 subnet.
const NETMASK_30: &str = "255.255.255.252";

/// Parse a CIDR subnet string into (base address, prefix length).
fn parse_cidr(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let (ip_str, prefix_str) = cidr
        .split_once('/')
        .ok_or_else(|| Error::Network(format!("invalid CIDR notation: {cidr}")))?;

    let ip: Ipv4Addr = ip_str
        .parse()
        .map_err(|e| Error::Network(format!("invalid IP in CIDR '{cidr}': {e}")))?;

    let prefix: u8 = prefix_str
        .parse()
        .map_err(|e| Error::Network(format!("invalid prefix in CIDR '{cidr}': {e}")))?;

    if prefix > 30 {
        return Err(Error::Network(format!(
            "subnet /{prefix} is too small for /30 allocations"
        )));
    }

    // Verify the IP is properly masked (no host bits set).
    let ip_u32 = u32::from(ip);
    let mask = if prefix == 0 {
        0u32
    } else {
        !((1u32 << (32 - prefix)) - 1)
    };
    if ip_u32 & mask != ip_u32 {
        return Err(Error::Network(format!(
            "IP {ip} has host bits set for /{prefix}"
        )));
    }

    Ok((ip, prefix))
}

/// Maximum number of /30 blocks that fit in a given prefix.
fn max_blocks(prefix_len: u8) -> u32 {
    // A /30 has 4 addresses. A /prefix has 2^(32-prefix) addresses.
    // max_blocks = 2^(32-prefix) / 4 = 2^(30-prefix)
    1u32 << (30 - prefix_len)
}

/// Compute the IP addresses for a given /30 block.
fn block_ips(base: Ipv4Addr, block_index: u32) -> IpAllocation {
    let base_u32 = u32::from(base);
    let network = base_u32 + block_index * 4;
    let host = Ipv4Addr::from(network + 1);
    let guest = Ipv4Addr::from(network + 2);

    IpAllocation {
        block_index,
        host_ip: host.to_string(),
        guest_ip: guest.to_string(),
        netmask: NETMASK_30.to_string(),
    }
}

/// Allocate a /30 block for a VM.
///
/// Finds the lowest-numbered available block in the subnet, records the
/// allocation, and persists it to the state store. The state store's
/// flock ensures safe concurrent access.
pub fn allocate(store: &StateStore, subnet: &str, vm_name: &str) -> Result<IpAllocation> {
    let path = store.network_allocations_path();
    let (base, prefix) = parse_cidr(subnet)?;
    let max = max_blocks(prefix);

    let mut allocs: IpAllocations = store
        .read_optional(&path)?
        .unwrap_or_else(|| IpAllocations {
            base_subnet: subnet.to_string(),
            allocations: HashMap::new(),
        });

    // Verify the subnet hasn't changed since allocations started.
    if allocs.base_subnet != subnet {
        return Err(Error::Network(format!(
            "subnet mismatch: state has '{}', requested '{subnet}'",
            allocs.base_subnet
        )));
    }

    // Find the first free block.
    let block_index = (0..max)
        .find(|i| !allocs.allocations.contains_key(i))
        .ok_or_else(|| {
            Error::Network(format!(
                "no free /30 blocks in {subnet} (all {max} blocks allocated)"
            ))
        })?;

    let allocation = block_ips(base, block_index);
    allocs.allocations.insert(block_index, vm_name.to_string());
    store.write(&path, &allocs)?;

    Ok(allocation)
}

/// Release a VM's IP allocation.
///
/// Removes all allocation entries for the given VM name, making the /30
/// block(s) available for reuse. Idempotent — does nothing if the VM
/// has no allocation or the allocations file doesn't exist.
pub fn release(store: &StateStore, vm_name: &str) -> Result<()> {
    let path = store.network_allocations_path();
    let mut allocs: IpAllocations = match store.read_optional(&path)? {
        Some(a) => a,
        None => return Ok(()),
    };

    let before = allocs.allocations.len();
    allocs.allocations.retain(|_, name| name != vm_name);

    // Only write back if something changed.
    if allocs.allocations.len() != before {
        store.write(&path, &allocs)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- CIDR parsing ---

    #[test]
    fn parse_cidr_valid() {
        let (ip, prefix) = parse_cidr("10.100.0.0/16").unwrap();
        assert_eq!(ip, Ipv4Addr::new(10, 100, 0, 0));
        assert_eq!(prefix, 16);
    }

    #[test]
    fn parse_cidr_slash_30() {
        let (ip, prefix) = parse_cidr("192.168.1.0/30").unwrap();
        assert_eq!(ip, Ipv4Addr::new(192, 168, 1, 0));
        assert_eq!(prefix, 30);
    }

    #[test]
    fn parse_cidr_rejects_slash_31() {
        assert!(parse_cidr("10.0.0.0/31").is_err());
    }

    #[test]
    fn parse_cidr_rejects_host_bits() {
        assert!(parse_cidr("10.100.0.1/16").is_err());
    }

    #[test]
    fn parse_cidr_rejects_no_slash() {
        assert!(parse_cidr("10.100.0.0").is_err());
    }

    // --- Block math ---

    #[test]
    fn max_blocks_slash_16() {
        assert_eq!(max_blocks(16), 16384);
    }

    #[test]
    fn max_blocks_slash_24() {
        assert_eq!(max_blocks(24), 64);
    }

    #[test]
    fn max_blocks_slash_30() {
        assert_eq!(max_blocks(30), 1);
    }

    #[test]
    fn block_ips_first() {
        let alloc = block_ips(Ipv4Addr::new(10, 100, 0, 0), 0);
        assert_eq!(alloc.host_ip, "10.100.0.1");
        assert_eq!(alloc.guest_ip, "10.100.0.2");
        assert_eq!(alloc.netmask, "255.255.255.252");
    }

    #[test]
    fn block_ips_second() {
        let alloc = block_ips(Ipv4Addr::new(10, 100, 0, 0), 1);
        assert_eq!(alloc.host_ip, "10.100.0.5");
        assert_eq!(alloc.guest_ip, "10.100.0.6");
    }

    #[test]
    fn block_ips_wraps_octet() {
        // Block 64 in a 10.100.0.0 base: 64 * 4 = 256 → rolls into second octet.
        let alloc = block_ips(Ipv4Addr::new(10, 100, 0, 0), 64);
        assert_eq!(alloc.host_ip, "10.100.1.1");
        assert_eq!(alloc.guest_ip, "10.100.1.2");
    }

    // --- Allocate / release with state store ---

    fn test_store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();
        (dir, store)
    }

    #[test]
    fn allocate_first_block() {
        let (_dir, store) = test_store();
        let alloc = allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        assert_eq!(alloc.block_index, 0);
        assert_eq!(alloc.host_ip, "10.100.0.1");
        assert_eq!(alloc.guest_ip, "10.100.0.2");
    }

    #[test]
    fn allocate_sequential() {
        let (_dir, store) = test_store();
        let a1 = allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        let a2 = allocate(&store, "10.100.0.0/16", "vm2").unwrap();
        let a3 = allocate(&store, "10.100.0.0/16", "vm3").unwrap();

        assert_eq!(a1.block_index, 0);
        assert_eq!(a2.block_index, 1);
        assert_eq!(a3.block_index, 2);

        assert_eq!(a1.host_ip, "10.100.0.1");
        assert_eq!(a2.host_ip, "10.100.0.5");
        assert_eq!(a3.host_ip, "10.100.0.9");
    }

    #[test]
    fn allocate_reuses_released_block() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        allocate(&store, "10.100.0.0/16", "vm2").unwrap();
        allocate(&store, "10.100.0.0/16", "vm3").unwrap();

        // Release the middle one.
        release(&store, "vm2").unwrap();

        // Next allocation should reuse block 1.
        let a4 = allocate(&store, "10.100.0.0/16", "vm4").unwrap();
        assert_eq!(a4.block_index, 1);
        assert_eq!(a4.host_ip, "10.100.0.5");
    }

    #[test]
    fn allocate_exhausts_small_subnet() {
        let (_dir, store) = test_store();
        // A /30 has only 1 block.
        allocate(&store, "192.168.1.0/30", "vm1").unwrap();
        let err = allocate(&store, "192.168.1.0/30", "vm2").unwrap_err();
        assert!(err.to_string().contains("no free /30 blocks"));
    }

    #[test]
    fn allocate_rejects_subnet_mismatch() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        let err = allocate(&store, "10.200.0.0/16", "vm2").unwrap_err();
        assert!(err.to_string().contains("subnet mismatch"));
    }

    #[test]
    fn release_idempotent() {
        let (_dir, store) = test_store();
        // Release with no allocations file at all.
        release(&store, "nonexistent").unwrap();

        // Allocate then release twice.
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        release(&store, "vm1").unwrap();
        release(&store, "vm1").unwrap();
    }

    #[test]
    fn release_only_removes_target_vm() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        allocate(&store, "10.100.0.0/16", "vm2").unwrap();
        allocate(&store, "10.100.0.0/16", "vm3").unwrap();

        release(&store, "vm2").unwrap();

        // vm1 and vm3 should still be allocated.
        let path = store.network_allocations_path();
        let allocs: IpAllocations = store.read(&path).unwrap();
        assert_eq!(allocs.allocations.len(), 2);
        assert_eq!(allocs.allocations[&0], "vm1");
        assert_eq!(allocs.allocations[&2], "vm3");
    }

    #[test]
    fn allocations_persist_across_reads() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        allocate(&store, "10.100.0.0/16", "vm2").unwrap();

        // Read the file directly and verify structure.
        let path = store.network_allocations_path();
        let allocs: IpAllocations = store.read(&path).unwrap();
        assert_eq!(allocs.base_subnet, "10.100.0.0/16");
        assert_eq!(allocs.allocations.len(), 2);
    }
}
