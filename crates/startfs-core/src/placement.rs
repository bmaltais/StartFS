//! Which members hold a chunk: weighted rendezvous hashing.
//!
//! Every member gets a score for a chunk from a hash of the two, scaled by
//! its share (its weight, or its space offer when it has none), and the N best scores win. A member's chance of winning
//! is proportional to its share, and when membership changes a chunk only
//! moves if the member that came or went was among its winners.
//!
//! The winners are then spread over the members' declared sites (ADR 0005):
//! walking the scores in order, a member is taken if its site is not yet
//! covered, or if the slots left still outnumber the uncovered sites.

use crate::chunkstore::ChunkId;
use crate::members::{Member, MemberId};
use std::collections::HashSet;

/// Where a member lives for the purpose of spreading copies: its declared
/// label, or itself when it has none.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Site<'a> {
    Declared(&'a str),
    Alone(MemberId),
}

impl<'a> Site<'a> {
    fn of(m: &'a Member) -> Self {
        if m.site.is_empty() {
            Site::Alone(m.id)
        } else {
            Site::Declared(&m.site)
        }
    }
}

/// A pseudo-random number in (0, 1) fixed by the chunk and the member.
fn unit(chunk: &ChunkId, member: MemberId) -> f64 {
    let mut h = blake3::Hasher::new();
    h.update(chunk);
    h.update(&member.0.to_le_bytes());
    let raw = u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap());
    ((raw >> 11) as f64 + 0.5) / (1u64 << 53) as f64
}

/// The members that should hold `chunk`, best first: up to `copies` distinct
/// members that offer space and are not in `skip` (full or otherwise out),
/// spanning `min(copies, sites)` of the sites among them.
pub fn place(
    chunk: &ChunkId,
    members: &[Member],
    copies: u32,
    skip: &HashSet<MemberId>,
) -> Vec<MemberId> {
    let mut scored: Vec<(f64, &Member)> = members
        .iter()
        .filter(|m| m.offer_bytes > 0 && !m.leaving && !skip.contains(&m.id))
        .map(|m| (-unit(chunk, m.id).ln() / m.share() as f64, m))
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.id.cmp(&b.1.id)));
    let sites: HashSet<Site> = scored.iter().map(|s| Site::of(s.1)).collect();
    let mut covered = HashSet::new();
    let mut placed = Vec::new();
    for (_, m) in scored {
        let slots_left = (copies as usize).saturating_sub(placed.len());
        if slots_left == 0 {
            break;
        }
        let site = Site::of(m);
        if !covered.contains(&site) || slots_left > sites.len() - covered.len() {
            covered.insert(site);
            placed.push(m.id);
        }
    }
    placed
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::NodeId;

    fn member(id: u32, site: &str) -> Member {
        Member {
            id: MemberId(id),
            endpoint: NodeId(id),
            addr: String::new(),
            offer_bytes: 1 << 30,
            weight_bytes: 0,
            rev: 1,
            leaving: false,
            removed: false,
            site: site.into(),
        }
    }

    fn chunk(i: u32) -> ChunkId {
        *blake3::hash(&i.to_le_bytes()).as_bytes()
    }

    /// The members as home x4 and cloud x1.
    fn home_and_cloud() -> Vec<Member> {
        let mut ms: Vec<Member> = (0..4).map(|i| member(i, "home")).collect();
        ms.push(member(4, "cloud"));
        ms
    }

    #[test]
    fn every_chunk_has_a_copy_on_the_cloud_site() {
        let ms = home_and_cloud();
        for i in 0..500 {
            let got = place(&chunk(i), &ms, 2, &HashSet::new());
            assert_eq!(got.len(), 2);
            assert!(got.contains(&MemberId(4)), "chunk {i}: {got:?}");
        }
    }

    #[test]
    fn without_labels_or_with_one_site_the_result_is_the_plain_top_scores() {
        let plain: Vec<Member> = (0..5).map(|i| member(i, "")).collect();
        let one: Vec<Member> = (0..5).map(|i| member(i, "home")).collect();
        for i in 0..200 {
            let by_score = |ms: &[Member]| {
                let mut s: Vec<_> = ms
                    .iter()
                    .map(|m| (-unit(&chunk(i), m.id).ln() / m.share() as f64, m.id))
                    .collect();
                s.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                s.into_iter().take(3).map(|s| s.1).collect::<Vec<_>>()
            };
            assert_eq!(
                place(&chunk(i), &plain, 3, &HashSet::new()),
                by_score(&plain)
            );
            assert_eq!(place(&chunk(i), &one, 3, &HashSet::new()), by_score(&one));
        }
    }

    #[test]
    fn a_member_outside_a_chunks_result_can_come_and_go_without_moving_it() {
        let ms = home_and_cloud();
        for i in 0..200 {
            let before = place(&chunk(i), &ms, 2, &HashSet::new());
            for extra in [member(9, "home"), member(10, "cloud"), member(11, "")] {
                let mut more = ms.clone();
                more.push(extra.clone());
                let after = place(&chunk(i), &more, 2, &HashSet::new());
                if !after.contains(&extra.id) {
                    assert_eq!(after, before, "chunk {i} with {}", extra.id);
                }
            }
            let outside = ms.iter().find(|m| !before.contains(&m.id)).unwrap().id;
            let fewer: Vec<Member> = ms.iter().filter(|m| m.id != outside).cloned().collect();
            assert_eq!(place(&chunk(i), &fewer, 2, &HashSet::new()), before);
        }
    }

    #[test]
    fn unlabelled_members_are_each_their_own_site() {
        let mut ms = vec![member(0, "home"), member(1, "home"), member(2, "home")];
        ms.push(member(3, ""));
        for i in 0..100 {
            assert!(place(&chunk(i), &ms, 2, &HashSet::new()).contains(&MemberId(3)));
        }
    }

    #[test]
    fn a_site_with_no_eligible_member_does_not_count() {
        let mut ms = home_and_cloud();
        ms[4].offer_bytes = 0;
        for i in 0..100 {
            let got = place(&chunk(i), &ms, 2, &HashSet::new());
            assert_eq!(got.len(), 2);
            assert!(!got.contains(&MemberId(4)));
        }
        let skip: HashSet<MemberId> = [MemberId(4)].into();
        let ms = home_and_cloud();
        assert_eq!(place(&chunk(1), &ms, 2, &skip).len(), 2);
    }

    #[test]
    fn more_copies_than_sites_keeps_every_site_and_fills_up() {
        let ms = home_and_cloud();
        for i in 0..100 {
            let got = place(&chunk(i), &ms, 3, &HashSet::new());
            assert_eq!(got.len(), 3);
            assert!(got.contains(&MemberId(4)));
        }
    }
}
