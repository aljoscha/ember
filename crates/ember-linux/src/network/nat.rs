//! Per-VM iptables rules.
//!
//! Three rules give one guest outbound connectivity:
//!
//! 1. **POSTROUTING MASQUERADE** rewrites the guest source IP on the
//!    way out, so the guest's private address never leaves the host.
//! 2. **FORWARD (outbound)** permits traffic from the VM's TAP to the
//!    WAN interface.
//! 3. **FORWARD (inbound)** permits established and related return
//!    traffic back from the WAN interface to the TAP.
//!
//! Rules 2 and 3 live in the installation's own FORWARD chain (see
//! [`super::policy`]), which is where the install-wide policy that
//! decides VM-to-VM and host reachability lives. Rule 1 stays in the
//! shared `nat` POSTROUTING chain, because address translation is not
//! a policy decision and has no ordering interaction with anything the
//! host keeps there.
//!
//! Rules are added on VM start and removed on stop, delete, and
//! crash recovery. Removal is idempotent.

use super::iptables::Rule;
use ember_core::error::Result;

/// iptables comment that scopes rule cleanup to one ember install.
///
/// `Some(ns)` → `ember:{ns}`, embedded via `-m comment --comment` in
/// every rule that lives in a chain ember does not own, so `-D` only
/// matches *this* install's rules. `None` returns the empty string,
/// which [`with_comment`] uses as the signal to omit the `-m comment`
/// match entirely. Older binaries added rules without a comment
/// match, so emitting one on legacy installs would make `iptables -D`
/// silently no-op and rules would accumulate forever. Empty preserves
/// the original rule shape.
pub fn comment(instance_id: Option<&str>) -> String {
    match instance_id {
        None => String::new(),
        Some(id) => format!("ember:{id}"),
    }
}

/// The iptables rules belonging to one VM.
///
/// One value describes both the add and the remove path, so a rule
/// can never be deleted in a shape that differs from how it was
/// added. Built from live allocation data at VM start and rebuilt from
/// the persisted [`NetworkInfo`](ember_core::state::vm::NetworkInfo)
/// at teardown.
pub struct VmRules<'a> {
    /// FORWARD chain the VM's two forwarding rules live in.
    ///
    /// `None` selects the legacy shape: rules appended straight to the
    /// built-in FORWARD chain and tagged with `comment`, which is what
    /// binaries predating [`super::policy`] wrote. Teardown of a VM
    /// started by such a binary has to delete them from there, so this
    /// is persisted per VM rather than derived from the current
    /// config.
    pub chain: Option<&'a str>,
    pub tap_device: &'a str,
    pub guest_ip: &'a str,
    pub wan_iface: &'a str,
    /// Per-installation tag from [`comment`]. Empty on legacy
    /// installs.
    pub comment: &'a str,
}

impl VmRules<'_> {
    /// Add every rule, skipping any that is already present.
    ///
    /// Idempotent so a retried VM start cannot leave duplicates
    /// behind.
    pub fn add(&self) -> Result<()> {
        for rule in self.rules() {
            rule.ensure()?;
        }
        Ok(())
    }

    /// Remove every rule, best effort.
    ///
    /// Called from stop, delete, and crash recovery, where a rule that
    /// is already gone is the normal case and a failure to remove one
    /// must not abort cleanup of the rest.
    pub fn remove(&self) {
        for rule in self.rules() {
            let _ = rule.remove();
        }
    }

    /// The rules, in the order `add` applies them.
    fn rules(&self) -> Vec<Rule> {
        let guest_cidr = format!("{}/32", self.guest_ip);

        // The masquerade rule keeps the same shape in both modes: it
        // has always lived in the shared POSTROUTING chain with the
        // comment as its only scoping, so rules written before and
        // after the policy chains existed are byte-for-byte identical
        // and stay mutually deletable.
        let masquerade = Rule::nat(
            "POSTROUTING",
            &with_comment(
                &["-s", &guest_cidr, "-o", self.wan_iface],
                self.comment,
                &["-j", "MASQUERADE"],
            ),
        );

        let outbound = &["-i", self.tap_device, "-o", self.wan_iface];
        let inbound = &[
            "-i",
            self.wan_iface,
            "-o",
            self.tap_device,
            "-m",
            "conntrack",
            "--ctstate",
            "RELATED,ESTABLISHED",
        ];

        let (outbound, inbound) = match self.chain {
            // Inside a chain ember owns, the chain itself is the
            // scope, so the comment match would be noise. Front
            // placement keeps both ACCEPTs above the chain's terminal
            // DROP without having to inspect rule order.
            Some(chain) => (
                Rule::filter(chain, &[outbound.as_slice(), &["-j", "ACCEPT"]].concat()).at_front(),
                Rule::filter(chain, &[inbound.as_slice(), &["-j", "ACCEPT"]].concat()).at_front(),
            ),
            None => (
                Rule::filter(
                    "FORWARD",
                    &with_comment(outbound, self.comment, &["-j", "ACCEPT"]),
                ),
                Rule::filter(
                    "FORWARD",
                    &with_comment(inbound, self.comment, &["-j", "ACCEPT"]),
                ),
            ),
        };

        vec![masquerade, outbound, inbound]
    }
}

/// Splice `-m comment --comment <comment>` between rule head and tail
/// when `comment` is non-empty. An empty comment yields the unwrapped
/// rule, matching what older ember binaries emitted byte-for-byte,
/// which matters because iptables compares full rules during `-D`.
fn with_comment<'a>(head: &[&'a str], comment: &'a str, tail: &[&'a str]) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(head.len() + tail.len() + 4);
    out.extend_from_slice(head);
    if !comment.is_empty() {
        out.extend_from_slice(&["-m", "comment", "--comment", comment]);
    }
    out.extend_from_slice(tail);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagged(chain: Option<&'static str>) -> VmRules<'static> {
        VmRules {
            chain,
            tap_device: "ema3f4-550e840",
            guest_ip: "10.100.0.2",
            wan_iface: "en0",
            comment: "ember:a3f4",
        }
    }

    /// The exact `iptables` invocations `add` would run.
    fn invocations(rules: &VmRules<'_>) -> Vec<Vec<String>> {
        rules.rules().iter().map(|r| r.add_args()).collect()
    }

    #[test]
    fn comment_for_new_install_tags_namespace() {
        assert_eq!(comment(Some("a3f4")), "ember:a3f4");
    }

    /// Locked: legacy mode must return an empty string so the rule
    /// shape stays byte-for-byte identical to what older binaries
    /// emitted (no `-m comment` match), or `iptables -D` silently
    /// no-ops on upgraded hosts.
    #[test]
    fn comment_for_legacy_install_is_empty() {
        assert_eq!(comment(None), "");
    }

    #[test]
    fn with_comment_skips_match_when_empty() {
        // Legacy mode (empty comment) must produce byte-for-byte the
        // same rule the old binary added, otherwise `iptables -D`
        // won't match existing rules on upgraded hosts.
        let args = with_comment(&["-A", "FORWARD", "-i", "tap0"], "", &["-j", "ACCEPT"]);
        assert_eq!(args, vec!["-A", "FORWARD", "-i", "tap0", "-j", "ACCEPT"]);
    }

    #[test]
    fn with_comment_inserts_comment_match_when_non_empty() {
        let args = with_comment(
            &["-A", "FORWARD", "-i", "tap0"],
            "ember:a3f4",
            &["-j", "ACCEPT"],
        );
        assert_eq!(
            args,
            vec![
                "-A",
                "FORWARD",
                "-i",
                "tap0",
                "-m",
                "comment",
                "--comment",
                "ember:a3f4",
                "-j",
                "ACCEPT"
            ]
        );
    }

    #[test]
    fn every_vm_owns_exactly_three_rules() {
        assert_eq!(tagged(Some("ember-a3f4-forward")).rules().len(), 3);
        assert_eq!(tagged(None).rules().len(), 3);
    }

    /// Masquerade must not move or change shape between modes: rules
    /// written before and after the policy chains existed have to stay
    /// mutually deletable.
    #[test]
    fn masquerade_is_identical_in_both_modes() {
        let chained = invocations(&tagged(Some("ember-a3f4-forward")));
        let legacy = invocations(&tagged(None));
        assert_eq!(chained[0], legacy[0]);
        assert_eq!(
            chained[0],
            [
                "-w",
                "5",
                "-t",
                "nat",
                "-A",
                "POSTROUTING",
                "-s",
                "10.100.0.2/32",
                "-o",
                "en0",
                "-m",
                "comment",
                "--comment",
                "ember:a3f4",
                "-j",
                "MASQUERADE"
            ]
        );
    }

    /// Inside an ember-owned chain the chain is the scope, so the
    /// comment match is dropped, and both ACCEPTs are inserted at the
    /// front so they sit above the chain's terminal DROP.
    #[test]
    fn chain_mode_forward_rules_are_untagged_and_front_placed() {
        let rules = invocations(&tagged(Some("ember-a3f4-forward")));
        assert_eq!(
            rules[1],
            [
                "-w",
                "5",
                "-I",
                "ember-a3f4-forward",
                "1",
                "-i",
                "ema3f4-550e840",
                "-o",
                "en0",
                "-j",
                "ACCEPT"
            ]
        );
        assert_eq!(
            rules[2],
            [
                "-w",
                "5",
                "-I",
                "ember-a3f4-forward",
                "1",
                "-i",
                "en0",
                "-o",
                "ema3f4-550e840",
                "-m",
                "conntrack",
                "--ctstate",
                "RELATED,ESTABLISHED",
                "-j",
                "ACCEPT"
            ]
        );
    }

    /// Locked: a VM started by a binary that predates the policy
    /// chains has its rules appended to the built-in FORWARD chain and
    /// tagged with the comment. Teardown has to reproduce that exactly
    /// or `iptables -D` no-ops and the rules leak.
    #[test]
    fn legacy_mode_forward_rules_are_appended_to_builtin_chain() {
        let rules = invocations(&tagged(None));
        assert_eq!(
            rules[1],
            [
                "-w",
                "5",
                "-A",
                "FORWARD",
                "-i",
                "ema3f4-550e840",
                "-o",
                "en0",
                "-m",
                "comment",
                "--comment",
                "ember:a3f4",
                "-j",
                "ACCEPT"
            ]
        );
        assert_eq!(
            rules[2],
            [
                "-w",
                "5",
                "-A",
                "FORWARD",
                "-i",
                "en0",
                "-o",
                "ema3f4-550e840",
                "-m",
                "conntrack",
                "--ctstate",
                "RELATED,ESTABLISHED",
                "-m",
                "comment",
                "--comment",
                "ember:a3f4",
                "-j",
                "ACCEPT"
            ]
        );
    }

    /// A legacy install has no namespace, so its rules carry no
    /// comment match at all.
    #[test]
    fn legacy_install_rules_carry_no_comment_match() {
        let rules = VmRules {
            comment: "",
            ..tagged(None)
        };
        for invocation in invocations(&rules) {
            assert!(
                !invocation.contains(&"comment".to_string()),
                "unexpected comment match: {invocation:?}"
            );
        }
    }
}
