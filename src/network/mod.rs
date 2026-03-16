pub use ember_core::network::ip;

#[cfg(target_os = "linux")]
pub use ember_linux::network::{cleanup, dns, nat, tap, wan};
