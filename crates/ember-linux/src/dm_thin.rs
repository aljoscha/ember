//! Linux device-mapper thin provisioning backend.
//!
//! Thin pools provide block-level copy-on-write storage. A single
//! per-installation pool aggregates two backing devices (metadata and
//! data) and exposes any number of independent thin volumes addressed
//! by 64-bit numeric IDs. The pool name comes from [`pool::name`],
//! which derives from the install's namespace. Snapshots and clones
//! are the same primitive ([`thin::create_snap`]) — snapshotting a
//! thin volume produces another thin volume that shares blocks until
//! divergence.
//!
//! Target-agnostic `dmsetup` plumbing lives in [`crate::dm`]; this
//! module only owns what is specific to `thin-pool` and `thin`.
//!
//! See `docs/DM-THIN-SPEC.md` for the full design.

pub mod loop_device;
pub mod pool;
pub mod thin;
pub mod tools;
