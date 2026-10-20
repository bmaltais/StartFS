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
