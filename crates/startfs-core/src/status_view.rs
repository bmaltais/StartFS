//! `startfs status` as text: a short summary by default, everything with `verbose`.

use crate::control::{FilesystemInfo, MemberStatus, NodeState, ScrubState, Status, Tier, Verdict};
use crate::upgrade::Progress;

/// The state of a node or member as one word.
pub fn state_word(state: NodeState) -> &'static str {
    match state {
        NodeState::Online => "up",
        NodeState::Down => "down",
        NodeState::Lost => "lost",
        NodeState::Leaving => "leaving",
        NodeState::Upgrading => "upgrading",
        NodeState::Unknown => "unknown",
    }
}

/// Pads `text` with spaces to `width` characters (not bytes, so `µs` lines up).
fn pad(text: &str, width: usize, right: bool) -> String {
    let fill = " ".repeat(width.saturating_sub(text.chars().count()));
    if right {
        format!("{fill}{text}")
    } else {
        format!("{text}{fill}")
    }
}

/// The members table: a header and one row per member, every column sized to
/// its longest entry. The marker for this node ends the row.
fn members_table(members: &[MemberStatus]) -> String {
    let mut rows: Vec<[String; 7]> =
        vec![["ID", "NAME", "STATE", "RELEASE", "SITE", "OFFERED", "RTT"].map(String::from)];
    for m in members {
        let mut state = state_word(m.state).to_string();
        if m.storage_only {
            state = format!("{state}, storage-only");
        }
        if let Some(load) = &m.behind {
            state = format!("{state}, {}", load.behind_text());
        }
        if let Some(step) = &m.upgrade {
            state = format!("{state}, {}", upgrade_step_text(step));
        }
        let dash = |s: &str| {
            if s.is_empty() {
                "-".to_string()
            } else {
                s.to_string()
            }
        };
        rows.push([
            m.id.to_string(),
            m.addr.clone(),
            state,
            dash(m.release.as_deref().unwrap_or("")),
            dash(&m.site),
            human(m.offer_bytes),
            m.rtt_micros.map_or_else(|| "-".to_string(), rtt_text),
        ]);
    }
    let width = |col: usize| {
        rows.iter()
            .map(|r| r[col].chars().count())
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..7).map(width).collect();
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        let mut line = String::from(" ");
        for (c, cell) in r.iter().enumerate() {
            line.push(' ');
            // Offered space and round-trip time are numbers: right-aligned.
            line.push_str(&pad(cell, widths[c], c >= 5));
            line.push(' ');
        }
        if i > 0 && members[i - 1].is_self {
            line.push_str(" (this node)");
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Detail for a member that is not plainly up, after the table: why it cannot
/// be reached, when it was last seen, how long stand-in copies are kept; with
/// `verbose`, also the protocol and weight of every member.
fn member_notes(members: &[MemberStatus], verbose: bool) -> String {
    let mut out = String::new();
    for m in members {
        let mut notes: Vec<String> = Vec::new();
        if let Some(why) = &m.problem {
            notes.push(format!("cannot reach: {why}"));
        }
        match &m.upgrade {
            Some(Progress::Failed(why)) => notes.push(format!("upgrade failed: {why}")),
            Some(Progress::WaitingOnMount) => notes.push(
                "processes are using its mount: free it, or run `startfs update --force`"
                    .to_string(),
            ),
            _ => {}
        }
        if let Some(seen) = m.last_seen {
            notes.push(format!(
                "last seen {}{} ago",
                if seen.exact { "" } else { "over " },
                time_left(seen.secs_ago)
            ));
        }
        if let Some(left) = m.standin_secs_left {
            notes.push(format!(
                "extra copies kept for {} in case it sleeps again",
                time_left(left)
            ));
        }
        if verbose {
            if m.weight_bytes > 0 {
                notes.push(format!("weight {}", human(m.weight_bytes)));
            }
            if let Some(p) = m.protocol {
                notes.push(format!("protocol {p}"));
            }
        }
        if !notes.is_empty() {
            out.push_str(&format!("  {} ({}): {}\n", m.addr, m.id, notes.join("; ")));
        }
    }
    out
}

/// What a member has got to in the upgrade, for the members table.
fn upgrade_step_text(step: &Progress) -> &'static str {
    match step {
        Progress::Staging => "staging a release",
        Progress::Staged => "release staged",
        Progress::WaitingOnMount => "release staged, waiting on its mount",
        Progress::Restarting => "restarting on a new release",
        Progress::Failed(_) => "upgrade failed",
    }
}

/// A round-trip time for the members table: `340µs`, `12ms`, `1.2s`.
pub fn rtt_text(micros: u64) -> String {
    match micros {
        0..1_000 => format!("{micros}µs"),
        1_000..100_000 => format!("{:.1}ms", micros as f64 / 1_000.0),
        100_000..1_000_000 => format!("{}ms", micros / 1_000),
        _ => format!("{:.1}s", micros as f64 / 1_000_000.0),
    }
}

/// "2d 4h", "5h 10m" or "12m": the two largest units of `secs`.
pub fn time_left(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// Chunks short of copies at `tier`.
fn short_chunks(s: &Status, tier: Tier) -> u32 {
    s.unreplicated
        .iter()
        .filter(|f| f.tier() == tier)
        .map(|f| f.chunks)
        .sum()
}

/// "not fully replicated: N chunks (a at risk, b degraded)", or nothing.
fn unreplicated_line(s: &Status) -> Option<String> {
    if s.unreplicated.is_empty() {
        return None;
    }
    let n: u32 = s.unreplicated.iter().map(|f| f.chunks).sum();
    let tiers: Vec<String> = [(Tier::AtRisk, "at risk"), (Tier::Degraded, "degraded")]
        .into_iter()
        .filter(|(t, _)| short_chunks(s, *t) > 0)
        .map(|(t, name)| format!("{} {name}", short_chunks(s, t)))
        .collect();
    Some(format!(
        "not fully replicated: {n} chunk{} ({})\n",
        plural(u64::from(n), "", "s"),
        tiers.join(", ")
    ))
}

/// `a / b` as "1.4x", or nothing when there is no `b` to compare with.
pub fn ratio(a: u64, b: u64) -> Option<String> {
    (b > 0).then(|| format!("{:.1}x", a as f64 / b as f64))
}

/// Where the space went: the files' size, what dedup and compression leave
/// of it, and this node's share of the replicas.
fn efficiency_lines(s: &Status, copies: u32) -> String {
    let e = &s.efficiency;
    let versions = match (&s.filesystem, e.version_bytes) {
        (Some(f), bytes) if bytes > 0 => format!(
            "  plus {} that only earlier versions use, kept {} after they were replaced (`startfs history PATH`)\n",
            human(bytes),
            time_left(f.keep_versions_secs)
        ),
        _ => String::new(),
    };
    if e.logical_bytes == 0 {
        return format!("storage: no files yet\n{versions}");
    }
    let mut out = format!(
        "storage: {} in files, {} after dedup",
        human(e.logical_bytes),
        human(e.referenced_bytes + e.inline_bytes)
    );
    if let Some(r) = ratio(e.logical_bytes, e.referenced_bytes + e.inline_bytes) {
        out.push_str(&format!(" ({r})"));
    }
    out.push('\n');
    out.push_str(&format!(
        "  this node: {} on disk for {} of chunks",
        human(e.local_disk_bytes),
        human(e.local_plain_bytes)
    ));
    if let Some(r) = ratio(e.local_plain_bytes, e.local_disk_bytes) {
        out.push_str(&format!(" (compression {r})"));
    }
    out.push_str(&format!(
        ", {} compressed and {} stored as is\n",
        e.zstd_chunks, e.raw_chunks
    ));
    if e.zstd_chunks > 0 && e.local_plain_bytes > 0 {
        out.push_str(&format!(
            "  compression saved {} ({:.0}% of this node's chunk bytes were compressed)\n",
            human(e.local_plain_bytes.saturating_sub(e.local_disk_bytes)),
            100.0 * e.zstd_plain_bytes as f64 / e.local_plain_bytes as f64
        ));
    }
    let expected = e.referenced_bytes.saturating_mul(u64::from(copies));
    if expected > 0 {
        out.push_str(&format!(
            "  this node holds {:.1}% of the {} expected across the cluster ({} cop{})\n",
            100.0 * e.local_plain_bytes as f64 / expected as f64,
            human(expected),
            copies,
            plural(u64::from(copies), "y", "ies")
        ));
    }
    if s.gc.pending_bytes > 0 {
        out.push_str(&format!(
            "  plus {} on this node that no file uses, waiting for garbage collection\n",
            human(s.gc.pending_bytes)
        ));
    }
    out.push_str(&versions);
    out
}

/// The upgrade under way, or the release the cluster last moved to, and a split cluster.
/// Unless `verbose`, only what needs attention: an upgrade in progress or a split.
fn upgrade_lines(
    u: &crate::control::UpgradeStatus,
    members: &[MemberStatus],
    verbose: bool,
) -> String {
    let mut out = String::new();
    if !u.complete {
        let how = match (u.lockstep, u.go) {
            (Some(true), false) => {
                " (the protocol changes: every member swaps together once all have staged it)"
            }
            (Some(true), true) => " (the protocol changes: every member swaps together, now)",
            (Some(false), _) => " (one member at a time)",
            (None, _) => "",
        };
        out.push_str(&format!("upgrade: moving to {}{how}\n", u.release));
    } else if verbose {
        out.push_str(&format!(
            "release: {}, which every member that is up runs\n",
            u.release
        ));
    }
    if !u.split.is_empty() {
        let names: Vec<&str> = members
            .iter()
            .filter(|m| u.split.contains(&m.id))
            .map(|m| m.addr.as_str())
            .collect();
        out.push_str(&format!(
            "split: {} speak another protocol than this node and cannot sync with it until they run the same release\n",
            names.join(", ")
        ));
    }
    out
}

/// The names of the members with these ids.
fn names_of<'a>(s: &'a Status, ids: &[crate::members::MemberId]) -> Vec<&'a str> {
    s.members
        .iter()
        .filter(|m| ids.contains(&m.id))
        .map(|m| m.addr.as_str())
        .collect()
}

/// The first line: the verdict and what it comes from.
fn headline(s: &Status, verdict: Verdict, fs: &FilesystemInfo) -> String {
    let mut causes: Vec<String> = Vec::new();
    let away: Vec<&MemberStatus> = s
        .members
        .iter()
        .filter(|m| m.state != NodeState::Online)
        .collect();
    match away.as_slice() {
        [] => {}
        [m] => causes.push(format!("{} is {}", m.addr, state_word(m.state))),
        many if many.len() <= 3 => {
            let states: Vec<&str> = many.iter().map(|m| state_word(m.state)).collect();
            let names: Vec<&str> = many.iter().map(|m| m.addr.as_str()).collect();
            if states.windows(2).all(|w| w[0] == w[1]) {
                causes.push(format!("{} are {}", names.join(", "), states[0]));
            } else {
                causes.push(format!("{} are not up", names.join(", ")));
            }
        }
        many => causes.push(format!("{} members are not up", many.len())),
    }
    let single = short_chunks(s, Tier::AtRisk);
    if verdict == Verdict::AtRisk {
        let n = single.max(s.stranded.iter().map(|x| x.chunks).sum());
        if n > 0 {
            causes.push(format!(
                "{n} chunk{} may have a single copy",
                plural(u64::from(n), "", "s")
            ));
        }
    } else if verdict == Verdict::Degraded {
        let n = short_chunks(s, Tier::Degraded);
        if n > 0 && away.is_empty() {
            causes.push(format!(
                "{n} chunk{} still being copied",
                plural(u64::from(n), "", "s")
            ));
        }
    }
    let label = verdict.describe().to_uppercase();
    let tail = match verdict {
        Verdict::Safe => {
            return format!(
                "{label}: all {} cop{} in place",
                fs.copies,
                plural(u64::from(fs.copies), "y", "ies")
            );
        }
        Verdict::BestEffort => {
            causes.push(format!(
                "only {} node{} can hold copies",
                s.copies_possible,
                plural(u64::from(s.copies_possible), "", "s")
            ));
            format!("{} of {} copies possible", s.copies_possible, fs.copies)
        }
        _ => format!(
            "{} of {} copies still possible",
            s.copies_possible, fs.copies
        ),
    };
    causes.push(tail);
    format!("{label}: {}", causes.join(", "))
}

/// One refresh of `startfs status --watch` for a terminal of `rows` x `cols`: the cursor goes
/// home and each line is overwritten in place, clipped to the width and to the height so the
/// screen never scrolls, with whatever the previous frame left on a line or below erased. So
/// a member that changes state between two refreshes redraws without flicker or leftovers.
pub fn watch_frame(text: &str, rows: usize, cols: usize) -> String {
    let mut out = String::from("\x1b[H");
    let lines: Vec<&str> = text.lines().take(rows.max(1)).collect();
    for (i, line) in lines.iter().enumerate() {
        out.extend(line.chars().take(cols.max(1)));
        out.push_str("\x1b[K");
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    out.push_str("\x1b[J");
    out
}

/// `startfs status` as text. `verbose` adds the diagnostic detail: storage and
/// dedup, compression, cache, scrub, metadata log, garbage collection.
pub fn render(s: &Status, verbose: bool) -> String {
    let stopping = if s.stopping {
        "stopping: the daemon is shutting down\n"
    } else {
        ""
    };
    let Some(fs) = &s.filesystem else {
        return format!(
            "{stopping}no filesystem on this node; run `startfs create --name NAME --copies N`\n"
        );
    };
    let mut out = String::from(stopping);
    if let Some(v) = s.verdict {
        out.push_str(&headline(s, v, fs));
        out.push('\n');
    }
    out.push_str(&format!(
        "filesystem: {} ({} copies), {} node{}, this node {}\n",
        fs.name,
        fs.copies,
        s.nodes,
        plural(u64::from(s.nodes), "", "s"),
        match s.node_state {
            NodeState::Online => "online",
            other => state_word(other),
        }
    ));
    out.push_str(&members_table(&s.members));
    out.push_str(&member_notes(&s.members, verbose));
    out.push_str(&format!(
        "usage: {} stored in {} chunks, {} offered, about {} usable\n",
        human(s.usage.stored_bytes),
        s.usage.chunks,
        human(s.usage.offer_bytes),
        human(s.usable_bytes)
    ));
    if verbose {
        out.push_str(&format!(
            "redundancy: {} of {} copies possible ({} node{})\n",
            s.copies_possible,
            fs.copies,
            s.nodes,
            plural(u64::from(s.nodes), "", "s")
        ));
        out.push_str(&efficiency_lines(s, fs.copies));
        out.push_str(&format!(
            "cache: {} used of {}\n",
            human(s.cache.used_bytes),
            human(s.cache.capacity_bytes)
        ));
    }
    if s.staging.chunks > 0 {
        out.push_str(&format!(
            "staging: {} in {} chunks waiting for replication",
            human(s.staging.bytes),
            s.staging.chunks
        ));
        if let Some((id, chunks)) = s.staging.waiting_on {
            let name = s
                .members
                .iter()
                .find(|m| m.id == id)
                .map_or_else(|| id.0.to_string(), |m| m.addr.clone());
            out.push_str(&format!(", most on {name} ({chunks} chunks)"));
        }
        out.push('\n');
    }
    if let Some(u) = &s.upgrade {
        out.push_str(&upgrade_lines(u, &s.members, verbose));
    }
    if let Some(r) = &s.repair {
        out.push_str(&format!(
            "repair: {} at risk, {} degraded chunks left to re-replicate\n",
            r.at_risk, r.degraded
        ));
    }
    let scrub = match s.scrub.state {
        ScrubState::Idle => "idle".to_string(),
        ScrubState::Running => format!("running, {} replicas to check", s.scrub.remaining),
        ScrubState::PausedOnBattery => "paused while on battery power".to_string(),
    };
    if verbose || s.scrub.state != ScrubState::Idle || s.scrub.healing > 0 {
        out.push_str(&format!("scrub: {scrub}"));
        if s.scrub.healing > 0 {
            out.push_str(&format!(
                ", {} corrupt replicas being re-fetched",
                s.scrub.healing
            ));
        }
        out.push('\n');
    }
    if verbose {
        out.push_str(&format!(
            "metadata log: {} entries, {} objects\n",
            s.log.entries, s.log.objects
        ));
        if s.gc.pending_chunks > 0 || s.gc.reclaimed_chunks > 0 {
            out.push_str(&format!(
                "garbage collection: {} chunks ({} bytes) pending, {} chunks ({} bytes) reclaimed\n",
                s.gc.pending_chunks, s.gc.pending_bytes, s.gc.reclaimed_chunks, s.gc.reclaimed_bytes
            ));
        }
    }
    if s.gc.purging {
        out.push_str("garbage collection: purge in progress (deleted data is freed as soon as the gate allows)\n");
    }
    if !s.gc.waiting_on.is_empty() {
        out.push_str(&format!(
            "garbage collection is waiting to catch up with {} before reclaiming\n",
            names_of(s, &s.gc.waiting_on).join(", ")
        ));
    }
    if let Some(t) = &s.tick_busy {
        out.push_str(&format!(
            "busy: the background tick worked {}% of the last minute (average {} ms); see `startfs activity`\n",
            t.percent, t.avg_ms
        ));
    }
    for w in &s.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    if !s.unheard.is_empty() {
        out.push_str(&format!(
            "partial view: no replication report from {}\n",
            names_of(s, &s.unheard).join(", ")
        ));
    }
    if let Some(f) = s.filesystem.as_ref().filter(|f| !f.dir_copies.is_empty()) {
        let dirs: Vec<String> = f
            .dir_copies
            .iter()
            .map(|(p, n)| format!("{p} {n}"))
            .collect();
        out.push_str(&format!("directory copies: {}\n", dirs.join(", ")));
    }
    if let Some(line) = unreplicated_line(s) {
        out.push_str(&line);
    }
    if s.rechecking > 0 {
        out.push_str(&format!(
            "re-checking: {} chunk{} held here, waiting to hear who else holds them\n",
            s.rechecking,
            plural(s.rechecking, "", "s")
        ));
    }
    for st in &s.stranded {
        let addr = s
            .members
            .iter()
            .find(|m| m.id == st.member)
            .map_or("a member", |m| m.addr.as_str());
        out.push_str(&format!(
            "at risk: {} chunk{} may exist only on {addr} (single copy in its report {} ago)\n",
            st.chunks,
            plural(u64::from(st.chunks), "", "s"),
            time_left(st.secs_ago)
        ));
    }
    if !s.conflicts.is_empty() {
        out.push_str(&format!(
            "conflicts: {} (delete a copy to resolve it)\n",
            s.conflicts.len()
        ));
        for c in &s.conflicts {
            out.push_str(&format!("  {}\n", c.path));
        }
    }
    out
}

/// A size in powers of 1024: `512 B`, `1.0 KiB`, `100.0 GiB`.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}
