//! Thin wrapper around the `iptables` binary.
//!
//! Every iptables call in ember goes through here, so that the
//! xtables lock is always taken and so that "the thing you named
//! isn't there" is told apart from a real failure in exactly one
//! place.

use std::process::{Command, Output};

use ember_core::error::{Error, Result};

/// Seconds to wait for the xtables lock.
///
/// iptables exits rather than blocking when another process holds the
/// lock, and two concurrent `ember vm start` runs each insert rules.
/// Five seconds is far longer than a handful of insertions needs, and
/// still fails loudly if something is wedged holding the lock.
const LOCK_WAIT_SECS: &str = "5";

/// Where a rule goes in its chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Insert at position 1. Used for every ACCEPT, so it lands above
    /// the terminal DROP of an ember chain, and for chain jumps, so
    /// they land above whatever else the host keeps in INPUT/FORWARD.
    Front,
    /// Append. Used for a chain's single terminal DROP, which has to
    /// stay last, and for rules in shared chains where ember has no
    /// business jumping ahead of the host's own rules.
    Back,
}

/// One iptables rule, with the add and delete paths sharing a single
/// definition.
///
/// iptables compares the full rule text when deleting, so a `-D`
/// whose arguments differ in any way from the `-A` that created it
/// silently no-ops and the rule leaks. Building both paths from one
/// value removes that failure mode by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    table: Option<&'static str>,
    chain: String,
    args: Vec<String>,
    placement: Placement,
}

impl Rule {
    /// A rule in the `filter` table, which is iptables' default.
    pub fn filter(chain: impl Into<String>, args: &[&str]) -> Self {
        Self {
            table: None,
            chain: chain.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            placement: Placement::Back,
        }
    }

    /// A rule in the `nat` table.
    pub fn nat(chain: impl Into<String>, args: &[&str]) -> Self {
        Self {
            table: Some("nat"),
            ..Self::filter(chain, args)
        }
    }

    /// Place this rule at the front of its chain instead of appending.
    pub fn at_front(mut self) -> Self {
        self.placement = Placement::Front;
        self
    }

    /// Add the rule, unconditionally. Fails if it is already there
    /// only in the sense that a duplicate is created, which is why
    /// callers on idempotent paths use [`ensure`](Self::ensure).
    pub fn add(&self) -> Result<()> {
        run(&self.add_args()).map(|_| ())
    }

    /// Add the rule unless an identical one already exists.
    pub fn ensure(&self) -> Result<()> {
        if self.exists()? {
            return Ok(());
        }
        self.add()
    }

    /// Whether an identical rule is already present.
    ///
    /// A missing chain reports `false` rather than an error: the rule
    /// is not there, which is all the caller asked.
    pub fn exists(&self) -> Result<bool> {
        match run(&self.check_args()) {
            Ok(_) => Ok(true),
            Err(Error::Network(msg)) if is_absent(&msg) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Remove every copy of the rule.
    ///
    /// `iptables -D` deletes one match at a time, so we loop until
    /// iptables reports nothing left. Duplicates are possible when two
    /// `vm start` runs raced on an [`ensure`](Self::ensure), and a
    /// leftover copy would outlive the VM. Idempotent: a rule (or
    /// chain) that was never there is not an error.
    pub fn remove(&self) -> Result<()> {
        let args = self.delete_args();
        loop {
            match run(&args) {
                Ok(_) => continue,
                Err(Error::Network(msg)) if is_absent(&msg) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }

    /// The `iptables` arguments that [`add`](Self::add) runs.
    /// Crate-visible so rule shape can be asserted without touching
    /// the host.
    pub(crate) fn add_args(&self) -> Vec<String> {
        match self.placement {
            Placement::Front => self.args_with(&["-I", &self.chain, "1"]),
            Placement::Back => self.args_with(&["-A", &self.chain]),
        }
    }

    pub(crate) fn check_args(&self) -> Vec<String> {
        self.args_with(&["-C", &self.chain])
    }

    pub(crate) fn delete_args(&self) -> Vec<String> {
        self.args_with(&["-D", &self.chain])
    }

    /// Full argument vector: lock wait, table selection, the verb the
    /// caller wants, then the rule body.
    fn args_with(&self, verb: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = vec!["-w".into(), LOCK_WAIT_SECS.into()];
        if let Some(table) = self.table {
            out.push("-t".into());
            out.push(table.into());
        }
        out.extend(verb.iter().map(|s| s.to_string()));
        out.extend(self.args.iter().cloned());
        out
    }
}

/// Create a chain in the `filter` table if it isn't there already.
pub fn ensure_chain(chain: &str) -> Result<()> {
    match run(&args(&["-N", chain])) {
        Ok(_) => Ok(()),
        // iptables has no "create if absent", so an existing chain
        // comes back as a plain error we have to recognize by text.
        Err(Error::Network(msg)) if msg.contains("Chain already exists") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Flush and delete a chain in the `filter` table.
///
/// Idempotent. A chain that doesn't exist is not an error. The chain
/// must already be unreferenced, iptables refuses to delete a chain
/// that something still jumps to.
pub fn remove_chain(chain: &str) -> Result<()> {
    for verb in [["-F", chain], ["-X", chain]] {
        match run(&args(&verb)) {
            Ok(_) => {}
            Err(Error::Network(msg)) if is_absent(&msg) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Argument vector for a command that isn't a rule operation.
fn args(verb: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = vec!["-w".into(), LOCK_WAIT_SECS.into()];
    out.extend(verb.iter().map(|s| s.to_string()));
    out
}

/// True when iptables is saying the rule or chain we named isn't
/// there.
///
/// Both messages mean "nothing to do" on an idempotent path. The
/// first comes from `-C`/`-D` against a missing rule, the second from
/// naming a chain that doesn't exist. The strings are matched loosely
/// because their wording differs between the legacy and nft backends.
fn is_absent(stderr: &str) -> bool {
    stderr.contains("does a matching rule exist") || stderr.contains("No chain/target/match")
}

fn run(args: &[String]) -> Result<Output> {
    let output = Command::new("iptables")
        .args(args)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "iptables".into(),
            source: e,
        })?;

    if output.status.success() {
        return Ok(output);
    }

    // Errors carry iptables' own stderr because callers match on it
    // to recognize the absent-rule and existing-chain cases.
    Err(Error::Network(
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_is_the_default_placement() {
        let rule = Rule::filter("FORWARD", &["-i", "tap0", "-j", "ACCEPT"]);
        assert_eq!(
            rule.add_args(),
            ["-w", "5", "-A", "FORWARD", "-i", "tap0", "-j", "ACCEPT"]
        );
    }

    /// Front placement is what keeps ACCEPTs above a chain's terminal
    /// DROP without any rule-order comparison.
    #[test]
    fn front_placement_inserts_at_position_one() {
        let rule = Rule::filter("ember-forward", &["-i", "tap0", "-j", "ACCEPT"]).at_front();
        assert_eq!(
            rule.add_args(),
            [
                "-w",
                "5",
                "-I",
                "ember-forward",
                "1",
                "-i",
                "tap0",
                "-j",
                "ACCEPT"
            ]
        );
    }

    #[test]
    fn table_selection_precedes_the_verb() {
        let rule = Rule::nat("POSTROUTING", &["-j", "MASQUERADE"]);
        assert_eq!(
            rule.add_args(),
            [
                "-w",
                "5",
                "-t",
                "nat",
                "-A",
                "POSTROUTING",
                "-j",
                "MASQUERADE"
            ]
        );
    }

    /// The add, check and delete forms must differ only in the verb.
    /// Anything else and `-D` stops matching what `-A` created.
    #[test]
    fn check_and_delete_mirror_the_rule_body() {
        let rule = Rule::nat("POSTROUTING", &["-s", "10.0.0.2/32", "-j", "MASQUERADE"]).at_front();
        let body = ["-s", "10.0.0.2/32", "-j", "MASQUERADE"];

        assert_eq!(rule.check_args()[..5], ["-w", "5", "-t", "nat", "-C"]);
        assert_eq!(rule.check_args()[6..], body);
        assert_eq!(rule.delete_args()[..5], ["-w", "5", "-t", "nat", "-D"]);
        assert_eq!(rule.delete_args()[6..], body);
    }

    /// Placement is an add-time concern only. Deleting must not care,
    /// or a front-placed rule would be undeletable.
    #[test]
    fn placement_does_not_leak_into_delete() {
        let body = ["-i", "tap0", "-j", "ACCEPT"];
        let back = Rule::filter("FORWARD", &body);
        let front = Rule::filter("FORWARD", &body).at_front();
        assert_eq!(back.delete_args(), front.delete_args());
    }

    #[test]
    fn every_invocation_waits_for_the_xtables_lock() {
        let rule = Rule::filter("FORWARD", &["-j", "ACCEPT"]);
        for v in [rule.add_args(), rule.check_args(), rule.delete_args()] {
            assert_eq!(v[..2], ["-w", "5"], "missing lock wait in {v:?}");
        }
        assert_eq!(args(&["-N", "ember-input"])[..2], ["-w", "5"]);
    }

    #[test]
    fn absent_recognizes_both_iptables_wordings() {
        assert!(is_absent(
            "iptables: Bad rule (does a matching rule exist in that chain?)."
        ));
        assert!(is_absent("iptables: No chain/target/match by that name."));
        assert!(!is_absent("iptables: Chain already exists."));
        assert!(!is_absent(
            "iptables v1.8.13 (nf_tables): Permission denied (you must be root)"
        ));
    }
}
