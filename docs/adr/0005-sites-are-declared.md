# Sites are declared, not detected, and placement spreads copies across them

## Status

Accepted (#59).

## Context

Placement is weighted rendezvous hashing and knows nothing about where members are. With N
copies, four home nodes and one cloud node, many chunks land entirely on home nodes, so one
correlated failure (fire, theft, power, router) loses them although an off-site member exists.
A large offer on the cloud node only shifts the odds.

## Decision

**Declare.** Each member record has a `site` label, set at join (`--site`) and changed with
`startfs config --site`. A member with no label is its own site. Nothing is detected.

**Spread.** Placement keeps the weighted scores. It then walks the members in score order and
accepts one if its site is not yet covered, or if the slots left still exceed the number of
uncovered sites. The result spans `min(copies, eligible sites)` sites, is a pure function of the
member set, and is today's result when there is one site or no labels. Only eligible members
count: those offering space, not leaving and not skipped. If a site has none, placement uses
fewer sites rather than refusing writes, and `status` warns.

**Protocol.** The field is on the gossiped member record, so the protocol version is bumped
(ADR 0002) and every member is upgraded together.

## Considered options

- **Detect it from round-trip time.** Rejected: RTT is a poor proxy. A laptop on a VPN looks far
  and a cloud node in the same city looks near.
- **Detect it from Tailscale.** Rejected: Tailscale has no notion of a cloud node.
- **Swap the lowest-scoring winner for the best member of another site.** Rejected: it can need
  repeated swaps and its result is harder to state as a property.
- **A large offer on the cloud node.** Stays as the interim workaround, but only changes the odds.

## Consequences

The off-site site takes at least 1/N of all data, so its upload bandwidth and disk cost are part
of choosing to label it. Labelling or relabelling a member re-places chunks, throttled by
`repair_bytes_per_sec`. Reads are unchanged.
