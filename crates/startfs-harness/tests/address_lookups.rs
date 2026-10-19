//! Member addresses are names the transport looks up; a steady cluster must not do that on
//! every tick (on a live node it was 225 lookups a second and kept `systemd-resolved` busy).

use startfs_harness::Cluster;
use std::time::Duration;

#[test]
fn a_steady_cluster_does_not_look_up_member_names_on_every_tick() {
    let c = Cluster::start(3);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(1).join(&token, Some(0)).unwrap();
    c.control(2).join(&token, Some(0)).unwrap();
    c.sync();

    let before = c.network().address_lookups();
    for _ in 0..200 {
        c.tick();
    }
    assert_eq!(
        c.network().address_lookups(),
        before,
        "re-looked-up at once"
    );

    // A name can change address, so it is looked up again after a while, but once per member
    // and node, not once per tick.
    c.advance(Duration::from_secs(60));
    for _ in 0..200 {
        c.tick();
    }
    let again = c.network().address_lookups() - before;
    assert!(
        (1..=3 * 3).contains(&again),
        "{again} lookups after a minute"
    );
}
