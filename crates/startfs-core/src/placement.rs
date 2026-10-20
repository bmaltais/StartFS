//! Which members hold a chunk: weighted rendezvous hashing.
//!
//! Every member gets a score for a chunk from a hash of the two, scaled by
//! its share (its weight, or its space offer when it has none), and the N best scores win. A member's chance of winning
//! is proportional to its share, and when membership changes a chunk only
//! moves if the member that came or went was among its winners.

use crate::chunkstore::ChunkId;
use crate::members::{Member, MemberId};
use std::collections::HashSet;

/// A pseudo-random number in (0, 1) fixed by the chunk and the member.
fn unit(chunk: &ChunkId, member: MemberId) -> f64 {
    let mut h = blake3::Hasher::new();
    h.update(chunk);
    h.update(&member.0.to_le_bytes());
    let raw = u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap());
    ((raw >> 11) as f64 + 0.5) / (1u64 << 53) as f64
}

/// The members that should hold `chunk`, best first: up to `copies` distinct
/// members that offer space and are not in `skip` (full or otherwise out).
pub fn place(
    chunk: &ChunkId,
    members: &[Member],
    copies: u32,
    skip: &HashSet<MemberId>,
) -> Vec<MemberId> {
    let mut scored: Vec<(f64, MemberId)> = members
        .iter()
        .filter(|m| m.offer_bytes > 0 && !m.leaving && !skip.contains(&m.id))
        .map(|m| (-unit(chunk, m.id).ln() / m.share() as f64, m.id))
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(copies as usize)
        .map(|s| s.1)
        .collect()
}

/// Whether `me` is the one of the members holding `chunk` that sends it on to
/// targets that lack it. The choice is a hash of chunk and member, so the
/// sending is spread over the holders instead of falling on the lowest id.
pub fn is_steward(
    chunk: &ChunkId,
    me: MemberId,
    others: impl IntoIterator<Item = MemberId>,
) -> bool {
    let rank = |m: MemberId| (unit(chunk, m), m);
    let mine = rank(me);
    others.into_iter().all(|o| {
        let theirs = rank(o);
        mine.0
            .total_cmp(&theirs.0)
            .then(mine.1.cmp(&theirs.1))
            .is_le()
    })
}
