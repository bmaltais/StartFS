# A member's backlog is gossiped, and its placement weight is separate from its offer

## Status

Accepted (#84, #91).

## Context

A member with a slow disk can be up and minutes behind: its hellos run on their own thread, so
peers and `status` saw it as healthy while its tick took minutes and its inbox grew to thousands of
messages. Placement made it worse. It was weighted by the space a node offers, so the large
cloud disks with an IOPS cap held the most data and took the most pushes. `offer` meant two
things: how much space a node gives, and how much load it takes.

## Decision

**Report load.** `Health` carries a `Load`: seconds since the sender's last tick began, and the
messages and bytes waiting in its inbox. The age of the last tick is used, not an average tick,
because a stuck tick never finishes and so never enters an average; it also needs no activity
counters. It counts from the start of a tick, so a machine waking from sleep, whose first hello
leaves from inside its first tick, is not taken for behind. A member is *behind* when that age is 30 s or more, or its inbox holds
`behind_inbox_messages` (default 5000). `status` marks a member that is up and behind
(`vps1 up, 3 min behind (10k messages queued)`) and warns once per such member.

**Hold back, don't re-place.** The replicator sends nothing to a member that is behind and waits
for it to drain, like a peer whose transport backlog is full. Placement itself is unchanged and
the set of behind members is not part of the epoch, so a member that falls behind or recovers
starts no re-check (#82) and moves no data. A chunk with a behind target stays under-replicated
until that member catches up, rather than landing on another member and being moved again. A
member already taken for behind is let off only once both figures are under half their limits, so
one that hovers at a limit is not skipped and resumed on every hello.

**Weight.** Each member record has `weight_bytes`, in the units of the offer; 0 means the offer.
Placement scores by that share, `Member::share`. `offer` still decides how much a node can hold,
whether it is full, and the usable size. `startfs config --weight SIZE` sets it; a large but slow
disk can offer capacity and take a smaller share of the writes. Changing it revises the member's
record, so it is a placement change like a new offer: the chunks it touches are checked again.

## Consequences

- The hello changed shape: `PROTOCOL_VERSION` is 13 and every member must be upgraded together
  (ADR 0002). Nothing needs to read a missing load from an older member, since members of
  different versions do not talk.
- Nothing lowers a weight by itself. Whether a node's weight should follow its measured speed is
  left to the user for now.
- The write flood into `meta.redb` that made the disks slow is separate (needs its own issue).
