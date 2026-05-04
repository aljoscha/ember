//! IP allocation for VM networking.
//!
//! Two allocation strategies, picked per backend:
//!
//! * [`allocate`] — Linux: hand out `/30` blocks for point-to-point
//!   TAP routing. Each VM gets its own host (.1) and guest (.2) IPs
//!   on a dedicated /30. With a `/16` base, ~16,384 VMs fit.
//! * [`allocate_single`] — macOS: hand out single `/32` addresses on
//!   a shared subnet (vmnet's `192.168.64.0/24`). All VMs sit on the
//!   same L2 segment behind one shared gateway, so a /30 link per VM
//!   is overkill and would waste 75% of the address space.
//!
//! Both share the same `allocations.json` persistence layer with
//! flock-based locking; the `block_index` field's unit (4 addresses
//! for `allocate`, 1 for `allocate_single`) is implicit in which
//! function reads the file. An installation must use exactly one
//! strategy across its lifetime — the persisted `base_subnet` is
//! verified on every read.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::state::store::StateStore;

/// Persisted IP allocation state, stored as `allocations.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpAllocations {
    /// Base subnet in CIDR notation (e.g., "10.100.0.0/16" or
    /// "192.168.64.0/27").
    pub base_subnet: String,
    /// Map from block index to VM name. Block size depends on which
    /// allocator wrote the file: 4 addresses for [`allocate`], 1 for
    /// [`allocate_single`].
    pub allocations: HashMap<u32, String>,
}

/// A single IP allocation for one VM.
#[derive(Debug, Clone, PartialEq)]
pub struct IpAllocation {
    /// Index within the base subnet. Unit is allocator-dependent.
    pub block_index: u32,
    /// Host-side IP. For [`allocate`] (Linux): first usable address
    /// in the /30 (e.g., "10.100.0.1"). For [`allocate_single`]
    /// (macOS): the shared gateway passed by the caller.
    pub host_ip: String,
    /// Guest-side IP.
    pub guest_ip: String,
    /// Netmask for the link.
    pub netmask: String,
}

/// Netmask for a /30 subnet.
const NETMASK_30: &str = "255.255.255.252";

/// Parse a CIDR subnet string into (base address, prefix length).
/// Accepts any prefix `/0`..`/32`; per-allocator constraints (e.g.
/// `/30` minimum for [`allocate`]) are enforced at the call site.
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

    if prefix > 32 {
        return Err(Error::Network(format!(
            "invalid CIDR prefix /{prefix}: must be 0..=32"
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
    if prefix > 30 {
        return Err(Error::Network(format!(
            "subnet /{prefix} is too small for /30 allocations"
        )));
    }
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

/// Allocate a single /32 address for a VM in a shared subnet.
///
/// Used by macOS where vmnet provides a shared L2 bridge — every guest
/// sits on the same subnet behind one gateway, so a /30 P2P link per
/// VM (the [`allocate`] strategy) would waste 75% of the address
/// space. Block index here means "address offset from the subnet
/// base", so a /27 holds 32 candidate slots.
///
/// `host_ip` is returned to the caller as-is and conventionally
/// contains the shared gateway. `reserved` lists addresses the
/// allocator must never hand out — typically the surrounding /24's
/// network, broadcast, and gateway when the caller carved a /27 out
/// of vmnet's /24.
pub fn allocate_single(
    store: &StateStore,
    subnet: &str,
    vm_name: &str,
    host_ip: &str,
    netmask: &str,
    reserved: &[Ipv4Addr],
) -> Result<IpAllocation> {
    let path = store.network_allocations_path();
    let (base, prefix) = parse_cidr(subnet)?;
    let max = 1u32 << (32 - prefix);

    let mut allocs: IpAllocations = store
        .read_optional(&path)?
        .unwrap_or_else(|| IpAllocations {
            base_subnet: subnet.to_string(),
            allocations: HashMap::new(),
        });

    if allocs.base_subnet != subnet {
        return Err(Error::Network(format!(
            "subnet mismatch: state has '{}', requested '{subnet}'",
            allocs.base_subnet
        )));
    }

    let base_u32 = u32::from(base);

    // Walk the subnet looking for an unallocated, non-reserved slot.
    // Skipping reserved addresses keeps the gateway (and the wider
    // /24's network/broadcast when carved into /27s) un-handout-able
    // without the caller having to seed allocations.json.
    let block_index = (0..max)
        .find(|i| {
            if allocs.allocations.contains_key(i) {
                return false;
            }
            let addr = Ipv4Addr::from(base_u32 + i);
            !reserved.contains(&addr)
        })
        .ok_or_else(|| {
            Error::Network(format!(
                "no free addresses in {subnet} (all {max} candidates allocated or reserved)"
            ))
        })?;

    let guest_ip = Ipv4Addr::from(base_u32 + block_index);
    allocs.allocations.insert(block_index, vm_name.to_string());
    store.write(&path, &allocs)?;

    Ok(IpAllocation {
        block_index,
        host_ip: host_ip.to_string(),
        guest_ip: guest_ip.to_string(),
        netmask: netmask.to_string(),
    })
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
    fn parse_cidr_accepts_up_to_slash_32() {
        // The /30-minimum constraint moved into `allocate` so the
        // shared parser can also serve `allocate_single`, which
        // accepts narrow prefixes (a /27 or even /32).
        assert!(parse_cidr("10.0.0.0/31").is_ok());
        assert!(parse_cidr("192.168.64.0/27").is_ok());
        assert!(parse_cidr("10.0.0.5/32").is_ok());
    }

    #[test]
    fn parse_cidr_rejects_slash_above_32() {
        assert!(parse_cidr("10.0.0.0/33").is_err());
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

    #[test]
    fn allocate_rejects_too_narrow_subnet() {
        // /31 is too small for /30 P2P allocation; the constraint
        // lives on `allocate` (not `parse_cidr`) since the shared
        // parser also serves `allocate_single`.
        let (_dir, store) = test_store();
        let err = allocate(&store, "10.0.0.0/31", "vm1").unwrap_err();
        assert!(matches!(err, Error::Network(_)));
    }

    // --- allocate_single (macOS shared-subnet path) ---

    /// Helper: vmnet's host-global reservations carved out of the /24.
    fn vmnet_reserved() -> [Ipv4Addr; 3] {
        [
            Ipv4Addr::new(192, 168, 64, 0),   // /24 network
            Ipv4Addr::new(192, 168, 64, 1),   // vmnet gateway
            Ipv4Addr::new(192, 168, 64, 255), // /24 broadcast
        ]
    }

    #[test]
    fn allocate_single_skips_network_and_gateway_in_slot_zero() {
        // Slot 0 (192.168.64.0/27) contains both the /24 network
        // (.0) and the vmnet gateway (.1). The first guest allocated
        // must land on .2, not .0 or .1.
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        let alloc = allocate_single(
            &store,
            "192.168.64.0/27",
            "vm1",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
        )
        .unwrap();
        assert_eq!(alloc.guest_ip, "192.168.64.2");
        assert_eq!(alloc.host_ip, "192.168.64.1");
        assert_eq!(alloc.netmask, "255.255.255.0");
        // block_index reflects the address offset, not a /30 index.
        assert_eq!(alloc.block_index, 2);
    }

    #[test]
    fn allocate_single_packs_addresses_one_per_vm() {
        // /27 with 2 reserved addresses (.0, .1) yields 30 usable
        // single-IP slots — 4× what /30 allocation gets.
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        let mut last_octet = None;
        for i in 0..30 {
            let alloc = allocate_single(
                &store,
                "192.168.64.0/27",
                &format!("vm{i}"),
                "192.168.64.1",
                "255.255.255.0",
                &reserved,
            )
            .unwrap();
            let octet: u8 = alloc.guest_ip.split('.').nth(3).unwrap().parse().unwrap();
            // Strictly monotonic, never .0 or .1.
            assert!(octet >= 2);
            if let Some(prev) = last_octet {
                assert!(octet > prev, "expected strictly monotonic guest IPs");
            }
            last_octet = Some(octet);
        }
    }

    #[test]
    fn allocate_single_skips_broadcast_in_top_slot() {
        // Slot 7 (192.168.64.224/27) ends at .255, which is the
        // surrounding /24's broadcast. The allocator must not hand
        // it out, so the slot holds 31 (not 32) usable addresses.
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        for i in 0..31 {
            allocate_single(
                &store,
                "192.168.64.224/27",
                &format!("vm{i}"),
                "192.168.64.1",
                "255.255.255.0",
                &reserved,
            )
            .unwrap();
        }
        // 32nd allocation hits .255 → reserved → no free slot.
        let err = allocate_single(
            &store,
            "192.168.64.224/27",
            "overflow",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Network(_)));
    }

    #[test]
    fn allocate_single_reuses_released_addresses() {
        // Drop vm2, allocate vm4 → vm4 should land on vm2's freed
        // slot (lowest unused index).
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        let a1 = allocate_single(
            &store,
            "192.168.64.32/27",
            "vm1",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
        )
        .unwrap();
        let a2 = allocate_single(
            &store,
            "192.168.64.32/27",
            "vm2",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
        )
        .unwrap();
        let a3 = allocate_single(
            &store,
            "192.168.64.32/27",
            "vm3",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
        )
        .unwrap();
        assert_ne!(a1.guest_ip, a2.guest_ip);
        assert_ne!(a2.guest_ip, a3.guest_ip);

        release(&store, "vm2").unwrap();

        let a4 = allocate_single(
            &store,
            "192.168.64.32/27",
            "vm4",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
        )
        .unwrap();
        assert_eq!(
            a4.guest_ip, a2.guest_ip,
            "freed slot should be reused before allocating beyond the high-water mark"
        );
    }

    #[test]
    fn allocate_single_rejects_subnet_mismatch_on_reread() {
        // allocations.json pins the base subnet so an install can't
        // accidentally re-interpret block indices in a different
        // layout (e.g. switching between /30 and /32 strategies
        // would re-stamp every existing entry).
        let (_dir, store) = test_store();
        let reserved = vmnet_reserved();
        allocate_single(
            &store,
            "192.168.64.32/27",
            "vm1",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
        )
        .unwrap();
        let err = allocate_single(
            &store,
            "192.168.64.64/27",
            "vm2",
            "192.168.64.1",
            "255.255.255.0",
            &reserved,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Network(msg) if msg.contains("subnet mismatch")));
    }
}
