//! Which members count as able to hold a copy and which as behind is one answer:
//! what `status` reports is what the replicator acts on.

use startfs_harness::Cluster;
use std::time::Duration;

const G: u64 = 1 << 30;

#[test]
fn a_member_that_offers_nothing_does_not_count_toward_possible_copies() {
    let c = Cluster::with_offers(3, &[G, G, 0]);
    c.control(2).set_weight(G).unwrap();
    for _ in 0..3 {
        c.advance(Duration::from_secs(10));
        c.sync();
    }

    let s = c.control(0).status().unwrap();
    assert_eq!(s.members[2].offer_bytes, 0);
    assert_eq!(s.members[2].weight_bytes, G);
    assert_eq!(s.copies_possible, 2, "{s:?}");
}
