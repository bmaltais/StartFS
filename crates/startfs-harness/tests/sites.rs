//! Sites: members declare where they live and each chunk's copies are spread across them.

use startfs_core::ops::*;
use startfs_harness::{Cluster, noise};
use std::time::Duration;

const BIG: u64 = 10 << 30;
const MINUTE: Duration = Duration::from_secs(60);

fn put(fs: &impl FsOps, name: &str, data: &[u8]) {
    let f = fs.create(ROOT_INO, name, 0o644).unwrap();
    assert_eq!(fs.write(f.ino, 0, data).unwrap(), data.len());
}

/// Four home nodes (0..4) and one cloud node (4), two copies. Sites are set
/// through `join --site` when `declared`, otherwise nobody is labelled.
fn home_and_cloud(declared: bool) -> Cluster {
    let c = Cluster::start(5);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    if declared {
        c.control(0).set_site("home").unwrap();
    }
    for i in 1..5 {
        let site = match (declared, i) {
            (false, _) => "",
            (true, 4) => "cloud",
            (true, _) => "home",
        };
        c.control(i).join_at(&token, Some(BIG), site).unwrap();
    }
    c.sync();
    c
}

fn write_files(c: &Cluster, count: u64) {
    for i in 0..count {
        put(&c.fs(0), &format!("f{i}"), &noise(8 * 1024, i + 1));
    }
    c.sync();
}

fn replicas(c: &Cluster) -> Vec<u64> {
    (0..c.node_count())
        .map(|i| c.control(i).status().map_or(0, |s| s.usage.chunks))
        .collect()
}

#[test]
fn every_chunk_has_a_copy_on_the_cloud_member() {
    let c = home_and_cloud(true);
    write_files(&c, 40);
    let held = replicas(&c);
    let total: u64 = held.iter().sum();
    // The cloud member holds every chunk, and each has a second copy at home.
    assert_eq!(held[4] * 2, total, "{held:?}");
    assert!(held[4] > 0, "{held:?}");
}

#[test]
fn status_shows_each_members_site_and_no_warning_while_sites_can_be_spanned() {
    let c = home_and_cloud(true);
    let s = c.control(0).status().unwrap();
    let sites: Vec<&str> = s.members.iter().map(|m| m.site.as_str()).collect();
    assert_eq!(sites, ["home", "home", "home", "home", "cloud"]);
    assert!(
        !s.warnings.iter().any(|w| w.contains("sites")),
        "{:?}",
        s.warnings
    );
}

#[test]
fn with_the_cloud_member_lost_writes_succeed_status_warns_and_copies_return_when_it_does() {
    let c = home_and_cloud(true);
    write_files(&c, 30);
    let before = replicas(&c);

    c.crash(4);
    c.advance(31 * MINUTE);
    c.sync();
    // Writes are not refused for want of a second site.
    for i in 0..10 {
        put(&c.fs(0), &format!("g{i}"), &noise(8 * 1024, 100 + i));
    }
    c.sync();
    let s = c.control(0).status().unwrap();
    assert!(
        s.warnings.iter().any(|w| w.contains("sites")),
        "{:?}",
        s.warnings
    );
    let home: u64 = replicas(&c)[..4].iter().sum();
    assert!(
        home >= before[..4].iter().sum::<u64>() + before[4],
        "{home}"
    );

    c.restart(4);
    c.advance(MINUTE);
    c.sync();
    // The copies made at home meanwhile are trimmed once the retention period is over.
    c.advance(Duration::from_secs(4 * 24 * 3600));
    c.sync();
    let s = c.control(0).status().unwrap();
    assert!(
        !s.warnings.iter().any(|w| w.contains("sites")),
        "{:?}",
        s.warnings
    );
    let held = replicas(&c);
    assert_eq!(held[4] * 2, held.iter().sum::<u64>(), "{held:?}");
}

#[test]
fn labelling_an_existing_cluster_re_places_chunks_until_it_is_site_diverse() {
    let c = home_and_cloud(false);
    write_files(&c, 40);
    let unlabelled = replicas(&c);
    // By chance alone some chunks sit wholly on home nodes.
    assert!(
        unlabelled[4] * 2 < unlabelled.iter().sum::<u64>(),
        "{unlabelled:?}"
    );

    for i in 0..4 {
        c.control(i).set_site("home").unwrap();
    }
    c.control(4).set_site("cloud").unwrap();
    c.advance(MINUTE);
    c.sync();
    c.advance(31 * MINUTE);
    c.sync();

    let held = replicas(&c);
    assert_eq!(
        held.iter().sum::<u64>(),
        unlabelled.iter().sum::<u64>(),
        "{held:?}"
    );
    assert_eq!(held[4] * 2, held.iter().sum::<u64>(), "{held:?}");
}

/// Three home nodes (0..3) and two cloud nodes (3..5), two copies: one at each site.
fn three_home_two_cloud() -> Cluster {
    let c = Cluster::start(5);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    c.control(0).set_site("home").unwrap();
    for i in 1..5 {
        let site = if i < 3 { "home" } else { "cloud" };
        c.control(i).join_at(&token, Some(BIG), site).unwrap();
    }
    c.sync();
    c
}

#[test]
fn a_new_copy_is_made_from_a_holder_at_the_same_site() {
    use startfs_core::protocol::Message;
    let c = three_home_two_cloud();
    write_files(&c, 40);
    for _ in 0..4 {
        c.advance(MINUTE);
        c.sync();
    }
    let sent = c.network().traffic().len();

    // A third copy lands at either site; one holder is at each, so the one at the new copy's
    // site makes it.
    c.control(1).set_copies(3).unwrap();
    for _ in 0..20 {
        c.advance(MINUTE);
        c.sync();
    }
    let site = |n: u32| if n < 3 { "home" } else { "cloud" };
    let mut pushes = 0;
    for (from, to, raw) in &c.network().traffic()[sent..] {
        if let Ok(Message::ChunkPush(_, Some(_))) = Message::decode(raw) {
            pushes += 1;
            assert_eq!(site(from.0), site(to.0), "{from:?} sent a copy to {to:?}");
        }
    }
    assert!(pushes > 0, "no copy was made");
}

/// Three home members and two cloud members, three copies.
fn three_home_two_cloud_three_copies() -> Cluster {
    let c = Cluster::start(5);
    let token = c.control(0).create("home", 3).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    c.control(0).set_site("home").unwrap();
    for i in 1..5 {
        let site = if i < 3 { "home" } else { "cloud" };
        c.control(i).join_at(&token, Some(BIG), site).unwrap();
    }
    c.sync();
    c
}

#[test]
fn a_member_that_comes_back_gets_its_copies_whichever_holder_outranks_the_other() {
    // A chunk held by a cloud member and one home member, short of a copy at a second home
    // member: the home holder is the source at the target's site, and it must send even when
    // the cloud holder outranks it in the plain rule. Both used to leave it to the other, so
    // roughly half the chunks written while a member was away stayed one copy short.
    for away in 1..5 {
        let c = three_home_two_cloud_three_copies();
        c.crash(away);
        write_files(&c, 80);
        c.advance(Duration::from_secs(3600));
        c.sync();
        c.restart(away);
        for _ in 0..60 {
            c.advance(MINUTE);
            c.sync();
        }
        for i in 0..c.node_count() {
            let short = c.control(i).status().unwrap().unreplicated;
            assert!(
                short.is_empty(),
                "member {away} was away; node {i}: {short:?}"
            );
        }
    }
}
