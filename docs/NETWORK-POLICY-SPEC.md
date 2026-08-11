# Network Policy Spec (Linux)

VM-to-VM reachability and host isolation for the Linux backend.

## Problem

Today's Linux rule set is three appended rules per VM
(`network/nat.rs::add_rules`):

```
-t nat -A POSTROUTING -s <guest_ip>/32 -o <wan> -m comment --comment ember:<id> -j MASQUERADE
-A FORWARD -i <tap> -o <wan> -m comment --comment ember:<id> -j ACCEPT
-A FORWARD -i <wan> -o <tap> -m conntrack --ctstate RELATED,ESTABLISHED -m comment --comment ember:<id> -j ACCEPT
```

Each VM sits on its own /30 with the host TAP address as its default
gateway, so VM-to-VM traffic has to be routed by the host between two
TAP devices. Nothing in the rule set above permits that, and nothing
denies it either. Whether two VMs can talk is decided by the host's
`FORWARD` policy: on a box with no firewall (policy `ACCEPT`) it
already works, on a box where docker, firewalld, or ufw set `FORWARD
DROP` it does not. The behavior is an accident of the host
configuration.

Host reachability is the same story in reverse. The guest's gateway is
a host address, ember adds no `INPUT` rules anywhere, so a guest can
reach the host at its TAP address and at every other address the host
owns, unless the host's own firewall happens to drop it. "VMs cannot
reach the host" is not a property ember currently provides.

This spec replaces both accidents with a contract that holds
regardless of what else is in the host's firewall:

> An ember VM can reach the internet and the other VMs of its own
> installation. It cannot reach the host.

## Goals

- VM-to-VM traffic works within one installation.
- VM-to-host traffic is denied, for every host address, not just the
  gateway.
- Outbound internet access keeps working (unchanged masquerade).
- Host-to-VM traffic keeps working (`ember ssh`, `exec`, `cp`).
- The policy does not depend on the host's `INPUT`/`FORWARD` policy or
  on rule ordering relative to other tools.
- Cross-installation VM-to-VM traffic stays denied, preserving the
  contract that `tests/isolation.rs` guards.
- Rules already written by an older ember binary stay deletable.

## Non-goals

- Blocking the host's LAN. A guest can still reach other machines on
  the host's network, as it can today. Only the host itself becomes
  unreachable.
- IPv6 connectivity of any kind. ember is IPv4-only, and this spec
  closes IPv6 on ember links rather than policing it.
- Per-VM network policy knobs. The design leaves room for them (see
  Open decisions) but does not add any.
- nftables. ember shells out to `iptables`, and that stays.

## Design

### Install-owned chains

All policy moves into two chains owned by the installation, jumped to
from position 1 of the built-in chains:

```
iptables -N ember-<id>-input
iptables -N ember-<id>-forward
iptables -I INPUT   1 -j ember-<id>-input
iptables -I FORWARD 1 -j ember-<id>-forward
```

Position 1 is what makes the policy a contract rather than a
suggestion. Appending to `INPUT` cannot work: a pre-existing
`-A INPUT -s 10.0.0.0/8 -j ACCEPT`, which is a common "trust the LAN"
rule and which matches the default `10.100.0.0/16` guest range, would
match first and the host block would silently not apply. Appending to
`FORWARD` has the mirror problem, a mid-chain `REJECT` from ufw or
firewalld is reached before our `ACCEPT`.

Inserting at the top is only acceptable because both chains are
transparent to everything that is not an ember VM. Every rule inside
them matches on an ember TAP interface, and a chain that matches
nothing falls off its end and returns to the built-in chain at the
rule right after our jump. Non-ember traffic sees no behavior change.

### Chain names

`ember-<id>-input` and `ember-<id>-forward`, where `<id>` is
`GlobalConfig::instance_namespace()`. Legacy installs with no
instance id get `ember-input` and `ember-forward`.

Lowercase-with-dashes matches every other install-scoped ember
resource name (`ember-aaaa-pool` for dm-thin, `emaaaa-` for TAPs,
`ember:aaaa` for the iptables comment) and keeps `iptables-save |
grep ember` useful. It deviates from netfilter's uppercase convention
(`DOCKER-USER`, `LIBVIRT_FWO`) on purpose, house consistency wins over
domain convention here because the instance id is lowercase hex and a
mixed-case name reads worse than either. iptables caps chain names at
28 characters, `ember-a3f4-forward` is 18.

Naming lives in the networking subsystem next to `tap::prefix` and
`nat::comment`, and derives from the namespace the same way.

### Interface wildcards

iptables matches an interface name ending in `+` as a prefix. Since
every TAP of an installation shares the prefix from `tap::prefix`, one
wildcard rule covers all of them:

```
-i ema3f4-+     matches every TAP of install a3f4
-i em-+         matches every TAP of a legacy install
```

The trailing dash keeps this away from physical NICs named `em1`,
`em2`. It also keeps installs apart, `em-+` does not match
`emaaaa-...` and vice versa. Two legacy installs on one host still
share a prefix, which is the pre-existing collision that instance ids
exist to fix, and this spec does not change it.

### Chain contents

`ember-<id>-input`, static, two rules, no per-VM state:

```
-A ember-<id>-input -i em<id>-+ -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
-A ember-<id>-input -i em<id>-+ -j DROP
```

The established accept is mandatory, not a convenience. When the host
opens a connection to a guest, the guest's reply packets arrive on
`INPUT` from the TAP, so a bare `DROP` would break `ember ssh`,
`exec`, and `cp`. With the accept in front, host-initiated flows work
and guest-initiated flows to any host address hit the `DROP`.

`ember-<id>-forward`, static:

```
-A ember-<id>-forward -i em<id>-+ -o em<id>-+ -j ACCEPT
-A ember-<id>-forward -i em<id>-+ -j DROP
```

One rule delivers VM-to-VM in both directions. A packet from VM A to
VM B matches with `i=tapA, o=tapB`, and B's reply matches with
`i=tapB, o=tapA`, so no conntrack state rule is needed. Because the
rule is scoped to this install's prefix on both sides, cross-install
traffic does not match it.

The terminal `DROP` is what makes the contract absolute. Without it,
traffic out of an ember TAP that matches none of our accepts falls
through to the host's `FORWARD` rules, and whether a VM can reach
docker0, another install's TAPs, or a libvirt bridge would again
depend on the host policy. With it, a VM's forwarded traffic can only
go to a sibling TAP or out the WAN interface.

`ember-<id>-forward`, per VM, unchanged in shape from today apart from
living in the chain and dropping the comment match:

```
-I ember-<id>-forward 1 -i <tap> -o <wan> -j ACCEPT
-I ember-<id>-forward 1 -i <wan> -o <tap> -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
```

These stay per-VM rather than being wildcarded because the WAN
interface is captured at VM start and persisted in `NetworkInfo`. A
laptop that moves from ethernet to wifi between two VM starts gets a
correct rule for each VM, and teardown deletes exactly what setup
added.

The comment match is dropped for rules inside our chains. Its only
job was scoping `-D` so one install cannot delete another's rules
(`nat.rs::comment`), and an install-owned chain does that
structurally.

### Rule order is correct by construction

The chains hold accepts and exactly one terminal drop, so ordering
needs no comparison logic and no rewriting:

- Accepts are added with `-C` then `-I <chain> 1`, so they land above
  the drop.
- The single drop is added with `-C` then `-A <chain>`, so it lands at
  the bottom.

Accepts commute with each other, so their relative order is
irrelevant, and the drop is always last. This holds no matter what
subset of rules already exists, which is what makes `ensure` below a
handful of idempotent calls instead of a diff-and-rebuild.

### NAT stays where it is

The `POSTROUTING` masquerade rule keeps its current form, its comment
tag, and its home in the shared `nat` table. It is a per-address
translation rather than a policy decision, ordering collisions there
are rare because masquerade rules are source-scoped, and the comment
already solves cross-install scoping for it. Giving `nat` a third
ember chain is possible but buys little (see Open decisions).

Masquerade only matches `-o <wan>`, so VM-to-VM traffic is not
SNATed. VM B sees VM A's real guest address.

### Lifecycle

**Ensure at VM start.** `LinuxNetwork::setup` calls
`policy::ensure(namespace)` right after `enable_ip_forwarding()`, for
the same reason that call is there: iptables state does not survive a
reboot, and re-asserting cheap host-wide prep on every VM start is how
ember already recovers. Creating the chains only at `ember init`
would mean a reboot silently drops the host block, and silently losing
a security property is the failure mode this spec exists to remove.

`ensure` is idempotent: create each chain, tolerating "Chain already
exists"; `-C || -I 1` each accept; `-C || -A` the drop; `-C || -I 1`
each jump. Two concurrent `vm start` runs can duplicate a rule, which
is harmless, and `iptables_delete` already loops to remove duplicates.

**Teardown at deinit.** `NetworkBackend` gains

```rust
/// Remove host-wide firewall state owned by this installation.
/// Default implementation is a no-op for backends that keep no such
/// state (macOS vmnet).
fn deinit(&self, config: &GlobalConfig) -> Result<()> { Ok(()) }
```

The Linux implementation deletes the two jumps, flushes, and deletes
the two chains, all scoped to this install's names so a second
install is untouched. `src/cli/deinit.rs` calls it before
`storage.deinit`, best-effort with a warning on failure, since a
leftover chain should not block a deinit. Deinit already refuses to
run while VMs are registered, so no per-VM rules are in the chain by
then.

**Reconcile does nothing new.** Idle chains with no matching TAPs have
no effect, so pruning them when the last VM stops would be churn for
nothing. Rules for dead VMs are already cleaned by
`network::cleanup`.

### IPv6

The stock kernel brings up IPv6 on the TAP, so both ends get a
link-local address and a guest can reach the host over IPv6 while the
IPv4 block is in force. Rather than mirroring every rule into
`ip6tables`, `tap::create` disables the stack on the interface it just
created:

```
sysctl -w net.ipv6.conf.<tap>.disable_ipv6=1
```

The host then has no IPv6 address on the link and ignores inbound
IPv6, which closes host access and, since IPv6 forwarding is off and
unaddressed, VM-to-VM over IPv6 too. A kernel built without IPv6 has
no such sysctl, so a missing key is ignored. This matches ember being
IPv4-only by design and keeps the rule surface half the size.

### Legacy and upgrade

The legacy path is the hot path, not a compat afterthought. The
reference install this spec was written against has a `config.json`
with no `instance_id`, three running VMs on `em-`-prefixed TAPs, and
docker on the host, which is both why VM-to-VM does not work there
(docker sets `FORWARD DROP`) and why `ember-input` / `ember-forward`
and the `em-+` wildcard need as much care as the tagged names.

A VM started by an older binary has its two `FORWARD` rules in the
built-in chain, tagged with the comment. After an upgrade, teardown
must delete them from there, while newly started VMs get rules in the
chain. `NetworkInfo` records which applies:

```rust
/// iptables chain holding this VM's FORWARD rules. `None` means the
/// rules were added by a binary that appended them to the built-in
/// FORWARD chain with a comment match, and must be deleted from
/// there.
#[serde(default)]
pub firewall_chain: Option<String>,
```

`serde(default)` makes an old `vm.json` deserialize to `None` and take
the legacy delete path. This is the same trick `NetworkInfo.wan_iface`
already uses, and it pins the chain name per VM so a later rename does
not orphan rules.

`nat::add_rules` and `remove_rules` grow past a comfortable positional
argument count, so both take one struct built at setup and rebuilt at
teardown from `NetworkInfo`:

```rust
pub struct VmRules<'a> {
    /// `None` selects the legacy shape: rules in the built-in FORWARD
    /// chain, with the comment match.
    pub chain: Option<&'a str>,
    pub tap_device: &'a str,
    pub guest_ip: &'a str,
    pub wan_iface: &'a str,
    pub comment: &'a str,
}
```

`comment` stays needed in both modes, for the masquerade rule in the
new mode and for all three rules in the legacy mode.

### Adopting VMs that are already running

A VM running at the moment the chains first appear is the sharp edge of
the upgrade. Its rules are in the built-in FORWARD chain, which is
below the jump, so the chain's terminal DROP cuts it off the instant
another VM start creates the chain. The VM keeps running and silently
loses its network.

Reconcile, which runs at the start of every command, therefore moves
such a VM's forwarding rules into the chain: add the in-chain pair,
delete the built-in pair, then record `firewall_chain` on the VM so
teardown deletes from the right place. Add before delete, so the VM is
never without a rule. A momentary duplicate ACCEPT is harmless.

The masquerade rule must be excluded from this move. Its shape is
identical in both modes, so a naive add-then-remove of the full rule
set would delete it immediately after re-adding it and leave the VM
with no NAT. That is why `VmRules` exposes the forwarding pair
separately from the full set.

The work is one-shot per VM. After it runs, every VM's record points at
a chain and the check costs nothing, so reconcile does not re-assert
rules for VMs that already have them.

### Landing order hazard

The terminal `DROP` in `ember-<id>-forward` must not exist until the
per-VM egress accepts live inside that chain. A half-landed change
where the chain is jumped from `FORWARD` position 1 with its drop in
place while per-VM accepts are still appended to the built-in chain
kills all VM egress, because the drop is reached first. Either land
the chain work and the per-VM move together, or add the drop last.

## Code changes

`crates/ember-linux/src/network/`:

- **`iptables.rs`** (new, small). The exec layer: `run`, `delete`
  (the existing duplicate-tolerant loop), `exists` (`-C`),
  `new_chain`, `delete_chain`, `flush_chain`. Every invocation gains
  `-w 5`. That flag is missing today, which means two concurrent
  `ember vm start` runs can already fail on the xtables lock, and this
  spec adds more calls per start.
- **`nat.rs`**. Keeps per-VM rules, takes `VmRules`, emits into the
  chain or the legacy shape. Loses the exec helpers to `iptables.rs`.
- **`policy.rs`** (new). Chain names, `ensure`, `deinit`, the static
  rule set. The boundary: `iptables.rs` knows how to run iptables,
  `nat.rs` owns per-VM rules, `policy.rs` owns install-scoped chains
  and the policy in them.
- **`tap.rs`**. Disable IPv6 on the device after bringing it up.

Outside networking:

- `network_backend.rs`: call `policy::ensure`, pass `VmRules`, put the
  chain name in `NetworkInfo`.
- `network.rs::cleanup`: rebuild `VmRules` from `NetworkInfo`.
- `ember-core/src/state/vm.rs`: `NetworkInfo.firewall_chain`.
- `ember-core/src/backend.rs`: `NetworkBackend::deinit` with a default
  no-op body.
- `src/cli/deinit.rs`: call it.
- `src/cli/info.rs`: print the two chain names, cheap diagnostic next
  to the existing dm-thin pool line.
- `docs/SPEC.md`: rewrite the Networking rule listing.

Rough size: 350 to 450 lines including tests, most of it in
`policy.rs` and its unit tests.

## Consequences and accepted limitations

- **The host is unreachable from a guest, including the gateway
  address.** A guest pinging its default gateway, or running
  traceroute, sees nothing. Traffic is dropped rather than rejected,
  so guest connections to the host hang until timeout instead of
  failing fast. `REJECT --reject-with icmp-admin-prohibited` is a
  one-line change if the hang turns out to be more annoying than the
  silence is principled.
- **A host-run DNS resolver breaks guest DNS.**
  `dns::detect_nameservers` filters loopback, but a host running
  dnsmasq or pihole bound to its LAN address hands the guest a
  nameserver that is a host address, which the block now drops. Worth
  a warning at VM start: compare the detected nameservers against the
  host's own addresses and say so out loud rather than letting DNS
  fail mysteriously.
- **VM egress now works on hosts where it silently did not.** Moving
  the egress accept into a chain at `FORWARD` position 1 punches
  through mid-chain rejects from ufw or firewalld. This is a bug fix
  against ember's documented promise of outbound access, and it is
  also ember overriding the host admin's firewall. Called out as a
  decision below.
- **A WAN interface change breaks a running VM's egress explicitly
  rather than accidentally.** Its egress accept and masquerade both
  name the old interface, so the terminal drop now stops the traffic.
  Today it would fall through to a permissive host policy and leave
  the guest sending unSNATed private-source packets out the new
  interface, which fails upstream anyway. Not theoretical: the
  reference install's WAN interface is `wg0-mullvad`, which comes and
  goes with the VPN.
- **Inbound from other host bridges is one-way blocked.** A docker
  container connecting to a VM is not matched by our chain on the way
  in, but the VM's replies hit the terminal drop, so the connection
  does not work. Consistent with the contract, worth knowing.
- **A host firewall flush while VMs run degrades the policy until the
  next VM start.** Rules are re-asserted at start, not continuously.
  Reporting policy health from `ember info` would close the
  observability gap.

## Open decisions

1. **Should VM-to-host be overridable?** Running a service on the
   host and hitting it from a VM is a common dev workflow, and this
   spec makes it impossible. The escape hatch would be an install-wide
   `ember init --allow-host-access` persisted on `GlobalConfig` and
   read by `policy::ensure`, which then omits the drop or accepts the
   TAP gateway address only. Not specced, since the ask was to block
   the host.
2. **Is punching through the host's `FORWARD` rules acceptable?** The
   alternative is leaving egress appended at the bottom of the
   built-in chain, which keeps ember deferential but makes egress and
   VM-to-VM behave inconsistently on restrictive hosts.
3. **Should masquerade move into an `ember-<id>-postrouting` chain
   too?** Uniform scoping and immunity to `POSTROUTING` ordering, at
   the cost of a third chain and lifecycle. It does not let the
   comment machinery retire, since legacy deletes need it regardless.
4. **Per-VM opt-out.** A `network.isolated: true` in the VM config
   would be a per-VM drop inserted above the sibling accept. Cheap to
   add later, out of scope now.

## Testing

Unit, pure functions, matching how `nat.rs` and `tap.rs` are tested
today:

- Chain name derivation for a tagged install and a legacy install,
  with the 28-character budget asserted.
- The static rule vectors for both chains, locking the established
  accept ahead of the drop and the wildcard form of both interface
  matches.
- `VmRules` rendering in chain mode and legacy mode, locking that
  legacy mode reproduces today's byte-for-byte rule including the
  comment, and that chain mode omits the comment on `FORWARD` rules
  and keeps it on masquerade.

Integration, `#[ignore]`, root plus iptables, in a new
`tests/network_policy.rs`:

- Two VMs of one install, ping and a TCP connect from A to B succeed.
- From inside a VM, `ping -c1 -W1 <host_ip>` and a TCP connect to the
  host's TAP address and to the host's LAN address all fail.
- `ember ssh` into a VM still works, which is the regression guard for
  the established accept.
- Egress still works, one outbound connectivity check from a guest.
- Deinit removes both chains and both jumps, and a second install's
  chains survive it. This extends the `tests/isolation.rs` family.
- Cross-install: install A's VM cannot reach install B's VM.

## Alternatives considered

**A shared bridge, like the macOS vmnet model.** Put every TAP on one
Linux bridge, switch the allocator to `allocate_single`, and VM-to-VM
becomes native L2 with no forwarding rules at all. Rejected as too
large a change for the ask: it replaces the documented /30
point-to-point model, needs a migration path for running VMs, and
still needs the whole `INPUT` design for host blocking, since the
bridge address is a host address. It also gives up a property worth
keeping, with point-to-point links the host routes every packet
between VMs, which is what makes a future per-VM policy knob a
one-rule change.

**Appending to the built-in chains.** Rejected, see Install-owned
chains. Ordering makes it a policy that silently might not apply.

**Per-VM-pair forward rules.** O(n^2) rules and churn on every start
and stop, where one wildcard rule does the job.

**An `ip6tables` mirror instead of disabling IPv6 on the TAP.**
Doubles the rule surface to police a stack ember never configures.
