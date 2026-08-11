//! Install-owned firewall chains: what an ember VM is allowed to
//! reach.
//!
//! An installation owns two chains, `ember-<id>-input` and
//! `ember-<id>-forward`, jumped to from position 1 of the built-in
//! INPUT and FORWARD chains. Together they deliver one contract:
//!
//! > A VM can reach the internet and the other VMs of its own
//! > installation. It cannot reach the host.
//!
//! Position 1 is what makes that a contract instead of a suggestion.
//! Appending cannot work: a pre-existing `-A INPUT -s 10.0.0.0/8 -j
//! ACCEPT`, a common "trust the LAN" rule that matches the default
//! guest range, would match first and the host block would silently
//! not apply. Appending to FORWARD has the mirror problem, a mid-chain
//! REJECT from ufw or firewalld is reached before our ACCEPT, which is
//! why VM-to-VM traffic does not work on a host running docker today.
//!
//! Jumping in at the top is only acceptable because both chains are
//! transparent to everything that is not an ember VM. Every rule in
//! them matches on an ember TAP interface, and a packet that matches
//! nothing falls off the end of the chain and resumes in the built-in
//! chain right after our jump. Non-ember traffic sees no change.
//!
//! Rule order inside the chains needs no comparison logic: the chains
//! hold ACCEPTs plus exactly one terminal DROP, ACCEPTs are inserted at
//! the front and the DROP is appended, so the DROP is always last no
//! matter which subset of rules already exists. That is what keeps
//! [`ensure`] a handful of idempotent calls rather than a
//! diff-and-rebuild.

use std::process::Command;

use ember_core::error::{Error, Result};

use super::iptables::{self, Rule};
use super::tap;

/// The chains one installation owns.
pub struct Chains {
    pub input: String,
    /// Also holds the per-VM forwarding rules from
    /// [`super::nat::VmRules`], which is why the name is persisted on
    /// each VM's `NetworkInfo`.
    pub forward: String,
}

/// Derive an installation's chain names.
///
/// `Some(ns)` → `ember-{ns}-input`, `None` → `ember-input` for installs
/// that predate instance ids. Lowercase-with-dashes matches every other
/// install-scoped ember resource name (`ember-aaaa-pool`, `emaaaa-`
/// TAPs, `ember:aaaa` comments) and keeps `iptables-save | grep ember`
/// useful, at the price of deviating from netfilter's uppercase
/// convention. iptables caps chain names at 28 characters, which the
/// longest form here (`ember-ffff-forward`, 18) sits well inside.
pub fn chains(instance_id: Option<&str>) -> Chains {
    match instance_id {
        None => Chains {
            input: "ember-input".to_string(),
            forward: "ember-forward".to_string(),
        },
        Some(id) => Chains {
            input: format!("ember-{id}-input"),
            forward: format!("ember-{id}-forward"),
        },
    }
}

/// Make the host ready to run this installation's VMs.
///
/// Idempotent, and called on every VM start rather than once at
/// `ember init`, because iptables state does not survive a reboot.
/// Creating the chains only at init time would mean a reboot silently
/// drops the host block, and silently losing a security property is
/// the failure mode this module exists to remove.
pub fn ensure(instance_id: Option<&str>) -> Result<()> {
    enable_ip_forwarding()?;

    let chains = chains(instance_id);
    let taps = tap::wildcard(instance_id);

    // Order matters twice over. A jump to a chain that does not exist
    // is an error, and a jump installed before the chain is populated
    // would expose an empty (so fully permissive) chain to live
    // traffic for as long as it takes to add the rules.
    iptables::ensure_chain(&chains.input)?;
    iptables::ensure_chain(&chains.forward)?;
    for rule in static_rules(&chains, &taps) {
        rule.ensure()?;
    }
    for rule in jumps(&chains) {
        rule.ensure()?;
    }
    Ok(())
}

/// Remove this installation's chains and the jumps into them.
///
/// Scoped to this install's chain names, so a second install on the
/// same host is untouched. Callers reach this through
/// `NetworkBackend::deinit`, which runs only once no VMs are
/// registered, so the forward chain holds no per-VM rules by then.
pub fn deinit(instance_id: Option<&str>) -> Result<()> {
    let chains = chains(instance_id);

    // Every step is attempted even after one fails, and only the first
    // error is reported. Bailing early would leave one chain behind
    // because the other could not be removed, and the caller's only
    // recourse is a warning either way.
    let mut failure = None;

    // Jumps first. iptables refuses to delete a chain that anything
    // still references.
    for rule in jumps(&chains) {
        if let Err(e) = rule.remove() {
            failure = failure.or(Some(e));
        }
    }
    for chain in [&chains.input, &chains.forward] {
        if let Err(e) = iptables::remove_chain(chain) {
            failure = failure.or(Some(e));
        }
    }

    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The install-wide rules, the ones that hold no per-VM state.
///
/// `taps` is the interface wildcard from [`tap::wildcard`], so one
/// rule covers every VM of the installation and none of anyone else's.
fn static_rules(chains: &Chains, taps: &str) -> Vec<Rule> {
    vec![
        // Return traffic for host-initiated connections. Mandatory,
        // not a convenience: when the host opens a connection to a
        // guest, the guest's replies arrive here from the TAP, so a
        // bare DROP below would break `ember ssh`, `exec` and `cp`.
        Rule::filter(
            &chains.input,
            &[
                "-i",
                taps,
                "-m",
                "conntrack",
                "--ctstate",
                "RELATED,ESTABLISHED",
                "-j",
                "ACCEPT",
            ],
        )
        .at_front(),
        // Everything else a guest sends to a host address, which is
        // every address the host owns and not just the TAP gateway.
        Rule::filter(&chains.input, &["-i", taps, "-j", "DROP"]),
        // VM to VM, both directions in one rule: A to B matches with
        // in=tapA out=tapB, B's reply matches with in=tapB out=tapA,
        // so no conntrack state is needed. Scoped to this install's
        // prefix on both sides, so it never covers another install's
        // VMs.
        Rule::filter(&chains.forward, &["-i", taps, "-o", taps, "-j", "ACCEPT"]).at_front(),
        // Terminal DROP. Without it, forwarded traffic that matches
        // none of our ACCEPTs would fall through to the host's own
        // FORWARD rules, and whether a VM could reach docker0, another
        // install's TAPs or a libvirt bridge would once again depend on
        // the host's configuration. The per-VM egress ACCEPT from
        // `nat::VmRules` is inserted at the front, so it is always
        // reached before this.
        Rule::filter(&chains.forward, &["-i", taps, "-j", "DROP"]),
    ]
}

/// The jumps from the built-in chains into ours.
fn jumps(chains: &Chains) -> Vec<Rule> {
    vec![
        Rule::filter("INPUT", &["-j", &chains.input]).at_front(),
        Rule::filter("FORWARD", &["-j", &chains.forward]).at_front(),
    ]
}

/// Enable IPv4 forwarding via sysctl.
///
/// Required before any VM can route traffic through the host, whether
/// out to the internet or across to a sibling VM. Safe to call
/// repeatedly.
fn enable_ip_forwarding() -> Result<()> {
    let output = Command::new("sysctl")
        .args(["-w", "net.ipv4.ip_forward=1"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "sysctl".into(),
            source: e,
        })?;
    Error::check_command("sysctl", output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocations(rules: &[Rule]) -> Vec<Vec<String>> {
        rules.iter().map(|r| r.add_args()).collect()
    }

    #[test]
    fn chain_names_embed_the_namespace() {
        let c = chains(Some("a3f4"));
        assert_eq!(c.input, "ember-a3f4-input");
        assert_eq!(c.forward, "ember-a3f4-forward");
    }

    /// Installs predating instance ids get unprefixed names. They are
    /// the only names such an install will ever use, so they have to
    /// stay stable.
    #[test]
    fn legacy_chain_names_are_unprefixed() {
        let c = chains(None);
        assert_eq!(c.input, "ember-input");
        assert_eq!(c.forward, "ember-forward");
    }

    #[test]
    fn chain_names_fit_the_iptables_budget() {
        let c = chains(Some("ffff"));
        for name in [c.input, c.forward] {
            assert!(name.len() <= 28, "chain name too long for iptables: {name}");
        }
    }

    /// The established-accept has to be reachable before the DROP, and
    /// front placement is what guarantees that without inspecting rule
    /// order. If this inverts, `ember ssh` breaks.
    #[test]
    fn host_block_accepts_established_before_dropping() {
        let c = chains(Some("a3f4"));
        let rules = invocations(&static_rules(&c, "ema3f4-+"));
        assert_eq!(
            rules[0],
            [
                "-w",
                "5",
                "-I",
                "ember-a3f4-input",
                "1",
                "-i",
                "ema3f4-+",
                "-m",
                "conntrack",
                "--ctstate",
                "RELATED,ESTABLISHED",
                "-j",
                "ACCEPT"
            ]
        );
        assert_eq!(
            rules[1],
            [
                "-w",
                "5",
                "-A",
                "ember-a3f4-input",
                "-i",
                "ema3f4-+",
                "-j",
                "DROP"
            ]
        );
    }

    /// One rule carries VM-to-VM in both directions, and the terminal
    /// DROP is appended so per-VM ACCEPTs inserted later still land
    /// above it.
    #[test]
    fn sibling_traffic_is_accepted_and_the_rest_dropped() {
        let c = chains(Some("a3f4"));
        let rules = invocations(&static_rules(&c, "ema3f4-+"));
        assert_eq!(
            rules[2],
            [
                "-w",
                "5",
                "-I",
                "ember-a3f4-forward",
                "1",
                "-i",
                "ema3f4-+",
                "-o",
                "ema3f4-+",
                "-j",
                "ACCEPT"
            ]
        );
        assert_eq!(
            rules[3],
            [
                "-w",
                "5",
                "-A",
                "ember-a3f4-forward",
                "-i",
                "ema3f4-+",
                "-j",
                "DROP"
            ]
        );
    }

    /// Every static rule matches on an ember TAP, which is what makes
    /// jumping in at position 1 transparent to the rest of the host.
    #[test]
    fn no_static_rule_matches_non_ember_traffic() {
        let c = chains(Some("a3f4"));
        for rule in invocations(&static_rules(&c, "ema3f4-+")) {
            let i = rule.iter().position(|a| a == "-i").expect("no -i match");
            assert_eq!(rule[i + 1], "ema3f4-+", "unscoped rule: {rule:?}");
        }
    }

    /// The jumps have to go in at position 1, or the host's own rules
    /// could accept guest traffic before our chains ever see it.
    #[test]
    fn jumps_go_in_at_the_top_of_the_builtin_chains() {
        let c = chains(Some("a3f4"));
        let rules = invocations(&jumps(&c));
        assert_eq!(
            rules[0],
            ["-w", "5", "-I", "INPUT", "1", "-j", "ember-a3f4-input"]
        );
        assert_eq!(
            rules[1],
            ["-w", "5", "-I", "FORWARD", "1", "-j", "ember-a3f4-forward"]
        );
    }

    /// Two installs must derive disjoint chain names and disjoint
    /// interface matches, or one install's policy would govern the
    /// other's VMs.
    #[test]
    fn installs_do_not_share_chains_or_interface_matches() {
        let a = chains(Some("aaaa"));
        let b = chains(Some("bbbb"));
        assert_ne!(a.input, b.input);
        assert_ne!(a.forward, b.forward);
        assert_ne!(tap::wildcard(Some("aaaa")), tap::wildcard(Some("bbbb")));
    }
}
