//! `startfs status` as text: a short, aligned summary with the verdict and its cause first,
//! and the diagnostic detail behind `verbose`.

use startfs_core::net::NodeId;
use startfs_core::status_view::render;
use startfs_core::sync::HELLO_INTERVAL;
use startfs_harness::Cluster;
use std::time::Duration;

const BIG: u64 = 10 << 30;

/// Where each cell of a table line starts and ends (cells are split by two or more spaces).
fn cells(line: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' ' {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while i < chars.len() && !(chars[i] == ' ' && chars.get(i + 1) == Some(&' ')) {
            if chars[i] != ' ' {
                end = i + 1;
            }
            i += 1;
        }
        out.push((start, end));
    }
    out
}

#[test]
fn the_summary_leads_with_the_verdict_and_aligns_members_that_are_down_asleep_or_incompatible() {
    let c = Cluster::start(4);
    let token = c.control(0).create("home", 2).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    for i in 1..4 {
        c.control(i).join(&token, Some(0)).unwrap();
    }
    // Mixed units: 10.0 GiB next to 1.0 GiB and 512.0 MiB.
    c.control(1).set_offer(1 << 30).unwrap();
    c.control(2).set_offer(512 << 20).unwrap();
    c.sync();
    c.advance(HELLO_INTERVAL);
    c.sync();

    // Node 2 is incompatible, node 1 is gone for good as far as the view goes.
    let why = "protocol version mismatch: the peer speaks version 5, this node speaks version 4; upgrade this node";
    c.network()
        .partition(&[&[NodeId(0), NodeId(3)], &[NodeId(1)], &[NodeId(2)]]);
    c.network().set_error(NodeId(3), NodeId(2), Some(why));
    c.crash(1);
    for _ in 0..4 {
        c.advance(HELLO_INTERVAL);
        c.sync();
    }
    c.advance(Duration::from_secs(10 * 60));
    c.sync();

    let status = c.control(3).status().unwrap();
    let text = render(&status, false);
    let lines: Vec<&str> = text.lines().collect();

    assert!(
        lines[0].starts_with("DEGRADED: ") && lines[0].contains("are down"),
        "verdict and cause on the first line:\n{text}"
    );
    assert!(lines.len() <= 15, "too long:\n{text}");
    assert!(lines.iter().any(|l| l.contains(why)), "{text}");
    assert!(
        !text.contains("metadata log") && !text.contains("cache:"),
        "{text}"
    );

    // Header and one row per member, every column on the same character positions.
    let header = lines
        .iter()
        .position(|l| l.trim_start().starts_with("ID "))
        .expect("a header");
    let table = &lines[header..header + 1 + status.members.len()];
    let want = cells(table[0]);
    assert_eq!(want.len(), 7);
    for row in &table[1..] {
        let got = cells(row);
        assert!(got.len() >= 7, "{row:?} in\n{text}");
        for col in 0..5 {
            assert_eq!(
                got[col].0, want[col].0,
                "column {col} of {row:?} in\n{text}"
            );
        }
        for col in 5..7 {
            assert_eq!(
                got[col].1, want[col].1,
                "column {col} of {row:?} in\n{text}"
            );
        }
    }
    // Detail for members that are away follows the table, never between rows.
    let after = &lines[header + table.len()..];
    assert!(after.iter().any(|l| l.contains("cannot reach")), "{text}");
    assert!(table.iter().all(|l| !l.contains("cannot reach")), "{text}");
    for m in &status.members {
        assert!(
            m.release
                .as_deref()
                .is_some_and(|r| table.iter().any(|l| l.contains(r))),
            "release of {} missing:\n{text}",
            m.addr
        );
    }

    let verbose = render(&status, true);
    for needle in [
        "storage:",
        "cache:",
        "scrub:",
        "metadata log:",
        "redundancy:",
    ] {
        assert!(verbose.contains(needle), "{needle} missing:\n{verbose}");
    }
}

#[test]
fn nine_members_with_one_away_fit_in_fifteen_lines() {
    let c = Cluster::start(9);
    let token = c.control(0).create("home", 3).unwrap();
    c.control(0).set_offer(BIG).unwrap();
    for i in 1..9 {
        c.control(i).join(&token, Some(0)).unwrap();
        c.control(i).set_offer(BIG).unwrap();
    }
    c.sync();
    c.crash(5);
    for _ in 0..4 {
        c.advance(HELLO_INTERVAL);
        c.sync();
    }
    let text = render(&c.control(0).status().unwrap(), false);
    assert!(text.lines().count() <= 15, "{text}");
    assert!(text.starts_with("DEGRADED: "), "{text}");
}
