//! Upgrades the cluster coordinates itself (ADR 0007).
//!
//! `update` gossips an intent: a concrete release tag. Every member stages the
//! release through the [`Installer`], which tells it the target's protocol. Same
//! protocol: a rolling upgrade, one member at a time, each announcing an upgrade
//! pause before it restarts. Different protocol: a lockstep upgrade, where the
//! issuer sends one go-signal once every member that is up has staged, and all
//! swap together. A member that was away learns which release to catch up to from
//! an upgrade notice (ADR 0006).

use super::standing::is_up;
use super::{Daemon, STANDIN_AFTER};
use crate::control::{UpdateReport, UpgradeStatus};
use crate::error::Error;
use crate::members::MemberId;
use crate::net::NodeId;
use crate::ops::Nanos;
use crate::protocol::{Message, UpgradeNotice};
use crate::upgrade::{
    Failure, InstallError, Installer, Intent, Progress, Release, Saved, SavedFile, UpgradeGossip,
    is_newer,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// How long a member that is about to restart asks the others to expect it back
/// before they take it for asleep: the asleep threshold itself.
pub(super) const UPGRADE_PAUSE: Duration = STANDIN_AFTER;

/// How long a lockstep stage is kept without hearing from the member that ordered it.
/// Past it the stage expires and nothing has restarted.
const STAGE_LEASE: Duration = Duration::from_secs(10 * 60);

/// How often a member whose mount is in use looks again whether it still is.
const MOUNT_RECHECK: Duration = Duration::from_secs(15);

/// How often a member whose turn it is looks again whether the data is safe.
const GATE_RECHECK: Duration = Duration::from_secs(10);

/// How often a member tells one that cannot hear it which release the cluster is on.
const NOTICE_INTERVAL: Duration = Duration::from_secs(30);

/// Where this node's own copy of the release stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    /// Nothing asked of the installer for the current intent.
    Idle,
    /// The installer is fetching and checking it.
    Fetching,
    /// Staged: the target's release, as the staged binary reports it.
    Ready(Release),
}

/// What a peer last said about upgrades, and when.
struct PeerUpgrade {
    gossip: UpgradeGossip,
    /// When the peer's upgrade pause ends, as of what it gossiped.
    pause_until: Option<Nanos>,
}

/// What this node last told the others, so a change is told at once.
#[derive(PartialEq)]
struct Told {
    intent: Option<Intent>,
    progress: Option<Progress>,
    paused: bool,
}

pub(super) struct Upgrades {
    file: SavedFile,
    saved: Saved,
    stage: Stage,
    /// The user said to restart even though the mount is in use.
    forced: bool,
    /// When this node's own upgrade pause ends.
    pause_until: Option<Nanos>,
    /// This node's own progress under the intent.
    progress: Option<Progress>,
    /// When the swap was handed to the installer.
    swapped_at: Option<Nanos>,
    /// Whether the mount was in use when last looked at, and when.
    mount: Option<(Nanos, bool)>,
    /// Whether the data was safe enough for the next restart when last looked at.
    gate: Option<(Nanos, bool)>,
    peers: HashMap<NodeId, PeerUpgrade>,
    /// When each peer was last sent a notice.
    notified: HashMap<NodeId, Nanos>,
    /// The protocol version peers were seen to speak when it was not ours.
    protocols: HashMap<NodeId, u8>,
    told: Option<Told>,
}

impl Upgrades {
    pub(super) fn open(dir: &Path) -> Self {
        let file = SavedFile::new(dir);
        let saved = file.load();
        Self {
            file,
            saved,
            stage: Stage::Idle,
            forced: false,
            pause_until: None,
            progress: None,
            swapped_at: None,
            mount: None,
            gate: None,
            peers: HashMap::new(),
            notified: HashMap::new(),
            protocols: HashMap::new(),
            told: None,
        }
    }

    fn save(&self) {
        self.file.save(&self.saved);
    }

    /// Take up an intent a peer gossiped when it is newer than ours; a go-signal
    /// for the intent we hold is taken up too.
    fn adopt(&mut self, theirs: Intent) {
        if theirs.rev <= self.saved.cancelled {
            return;
        }
        match &mut self.saved.intent {
            Some(ours) if ours.rev > theirs.rev => {}
            Some(ours) if ours.rev == theirs.rev && ours.tag == theirs.tag => {
                if theirs.go && !ours.go {
                    ours.go = true;
                    self.save();
                }
            }
            // Two members that ordered at once: every member picks the same one.
            Some(ours) if ours.rev == theirs.rev && ours.tag > theirs.tag => {}
            _ => self.replace_intent(theirs),
        }
    }

    fn replace_intent(&mut self, intent: Intent) {
        self.saved.intent = Some(intent);
        self.saved.failure = None;
        self.stage = Stage::Idle;
        self.forced = false;
        self.progress = None;
        self.save();
    }

    /// The revision a new intent of this node's takes.
    fn next_rev(&self) -> u64 {
        let held = self.saved.intent.as_ref().map_or(0, |i| i.rev);
        held.max(self.saved.cancelled) + 1
    }

    fn fail(&mut self, why: InstallError) {
        if let Some(intent) = &self.saved.intent {
            self.saved.failure = Some(Failure {
                rev: intent.rev,
                why,
            });
        }
        self.saved.swapping = None;
        self.save();
    }
}

/// A member other than this one, as the upgrade logic sees it.
struct Peer {
    id: MemberId,
    up: bool,
    /// When anything was last heard from it, when ever.
    heard: Option<Nanos>,
    gossip: Option<UpgradeGossip>,
    /// Its upgrade pause is on.
    paused: bool,
}

impl Peer {
    fn runs(&self, tag: &str) -> bool {
        self.gossip.as_ref().is_some_and(|g| g.running.tag() == tag)
    }

    fn progress(&self) -> Option<&Progress> {
        self.gossip.as_ref().and_then(|g| g.progress.as_ref())
    }
}

/// What a tick decided to do about the intent, once it knows where everyone stands.
enum Action {
    Nothing,
    /// Swap the release in; a rolling upgrade announces a pause first.
    Swap { pause: bool },
    /// Send the go-signal of a lockstep upgrade.
    Go,
    /// The issuer is gone and the stage expired.
    Cancel,
}

impl Daemon {
    /// Hand this daemon the means to fetch and swap releases, and settle what a
    /// restart left open: a swap that was handed over and left this node on another
    /// release was rolled back.
    pub fn set_installer(&self, installer: Arc<dyn Installer>) {
        *self.installer.lock().unwrap() = installer;
        let running = self.installer().current();
        let mut up = self.upgrades.lock().unwrap();
        let Some(swapped) = up.saved.swapping.clone() else {
            return;
        };
        if running.tag() == swapped {
            up.saved.swapping = None;
            up.save();
        } else {
            up.fail(format!(
                "the new daemon did not answer, so the previous release ({}) was restored",
                running.tag()
            ));
        }
    }

    pub(super) fn installer(&self) -> Arc<dyn Installer> {
        self.installer.lock().unwrap().clone()
    }

    /// Answer `startfs update`: order the cluster to a release, or with `force` let
    /// this node restart despite its mount being in use.
    pub(super) fn update(
        &self,
        release: Option<String>,
        force: bool,
    ) -> Result<UpdateReport, Error> {
        let me = self.membership.lock().unwrap().me();
        let me = me.ok_or(Error::NotInFilesystem)?;
        let installer = self.installer();
        let running = installer.current();
        let now = self.clock.now();
        let continuing = force
            && release.is_none()
            && self
                .upgrades
                .lock()
                .unwrap()
                .saved
                .intent
                .as_ref()
                .is_some_and(|i| i.tag != running.tag());
        let tag = match release {
            Some(tag) => {
                if !tag.starts_with('v') || !tag[1..].starts_with(|c: char| c.is_ascii_digit()) {
                    return Err(Error::BadRelease(tag));
                }
                tag
            }
            None if continuing => {
                let up = self.upgrades.lock().unwrap();
                up.saved.intent.as_ref().expect("checked above").tag.clone()
            }
            None => installer.latest_stable().map_err(Error::NoRelease)?,
        };
        let views = self.peer_views(now);
        let already = running.tag() == tag
            && views
                .iter()
                .filter(|p| p.up)
                .all(|p| p.runs(&tag));
        if already {
            return Ok(UpdateReport {
                release: tag,
                already_on_it: true,
            });
        }
        {
            let mut up = self.upgrades.lock().unwrap();
            if force {
                up.forced = true;
            }
            let same = up
                .saved
                .intent
                .as_ref()
                .is_some_and(|i| i.tag == tag && up.saved.failure.is_none());
            if !(continuing || same) {
                let rev = up.next_rev();
                up.replace_intent(Intent {
                    tag: tag.clone(),
                    rev,
                    issuer: Some(me),
                    go: false,
                });
            }
        }
        self.announce_upgrade();
        Ok(UpdateReport {
            release: tag,
            already_on_it: false,
        })
    }

    /// The members other than this one, with what they said about upgrades.
    fn peer_views(&self, now: Nanos) -> Vec<Peer> {
        let (members, me) = {
            let m = self.membership.lock().unwrap();
            (m.members(), m.me())
        };
        let heard = self.heard.lock().unwrap().clone();
        let up = self.upgrades.lock().unwrap();
        members
            .into_iter()
            .filter(|m| Some(m.id) != me)
            .map(|m| {
                let state = up.peers.get(&m.endpoint);
                Peer {
                    id: m.id,
                    up: is_up(heard.get(&m.endpoint).copied(), now),
                    heard: heard.get(&m.endpoint).copied(),
                    gossip: state.map(|s| s.gossip.clone()),
                    paused: state.is_some_and(|s| s.pause_until.is_some_and(|t| t > now)),
                }
            })
            .collect()
    }

    /// The members whose upgrade pause is on: expected back soon, so they are
    /// sent nothing new, get no stand-in copies and leave the verdict as it was.
    pub(super) fn paused_members(&self, now: Nanos) -> HashSet<MemberId> {
        let members = self.membership.lock().unwrap().members();
        let up = self.upgrades.lock().unwrap();
        members
            .into_iter()
            .filter(|m| {
                up.peers
                    .get(&m.endpoint)
                    .is_some_and(|p| p.pause_until.is_some_and(|t| t > now))
            })
            .map(|m| m.id)
            .collect()
    }

    /// What this node tells the others about upgrades.
    pub(super) fn upgrade_gossip(&self) -> UpgradeGossip {
        let now = self.clock.now();
        let running = self.installer().current();
        let up = self.upgrades.lock().unwrap();
        UpgradeGossip {
            running,
            intent: up.saved.intent.clone(),
            progress: up.progress.clone(),
            pause_secs: up
                .pause_until
                .and_then(|t| t.checked_sub(now))
                .filter(|left| *left > 0)
                .map(|left| left.div_ceil(NANOS_PER_SEC)),
        }
    }

    /// Tell every member how this node stands now.
    pub(super) fn announce_upgrade(&self) {
        let Some(transport) = self.transport() else {
            return;
        };
        let peers = self.membership.lock().unwrap().peers();
        let msg = Message::Upgrade(self.upgrade_gossip()).encode();
        for p in peers {
            transport.send(p, msg.clone());
        }
    }

    pub(super) fn on_upgrade_gossip(&self, from: NodeId, gossip: UpgradeGossip) {
        if !self.membership.lock().unwrap().is_member_endpoint(from) {
            return;
        }
        let now = self.clock.now();
        let mut up = self.upgrades.lock().unwrap();
        if let Some(intent) = gossip.intent.clone() {
            up.adopt(intent);
        }
        let pause_until = gossip.pause_secs.map(|s| now + s * NANOS_PER_SEC);
        up.peers.insert(
            from,
            PeerUpgrade {
                gossip,
                pause_until,
            },
        );
    }

    /// A member says the cluster is on a release this node cannot hear it speak
    /// for: take it up, if it is later than the one this node runs.
    pub(super) fn on_upgrade_notice(&self, from: NodeId, notice: UpgradeNotice) {
        if !self.membership.lock().unwrap().is_member_endpoint(from) {
            return;
        }
        let running = self.installer().current();
        if notice.protocol == running.protocol || !is_newer(&notice.release, &running.tag()) {
            return;
        }
        let mut up = self.upgrades.lock().unwrap();
        match &mut up.saved.intent {
            Some(held) if held.tag == notice.release => {
                if !held.go {
                    held.go = true;
                    up.save();
                }
            }
            _ => {
                let rev = up.next_rev();
                up.replace_intent(Intent {
                    tag: notice.release,
                    rev,
                    issuer: None,
                    go: true,
                });
            }
        }
    }

    /// A member sent a message in another protocol version. A member that is behind
    /// is told which release the cluster is on, when this node knows.
    pub(super) fn on_version_mismatch(&self, from: NodeId, theirs: u8) {
        if !self.membership.lock().unwrap().is_member_endpoint(from) {
            return;
        }
        let running = self.installer().current();
        let ours = running.protocol;
        let now = self.clock.now();
        let mut up = self.upgrades.lock().unwrap();
        up.protocols.insert(from, theirs);
        let settled = up
            .saved
            .intent
            .as_ref()
            .is_some_and(|i| i.tag == running.tag());
        let due = up
            .notified
            .get(&from)
            .is_none_or(|t| now.saturating_sub(*t) >= NOTICE_INTERVAL.as_nanos() as u64);
        if theirs >= ours || !settled || !due {
            return;
        }
        up.notified.insert(from, now);
        drop(up);
        if let Some(transport) = self.transport() {
            let notice = UpgradeNotice {
                release: running.tag(),
                protocol: running.protocol,
            };
            transport.send(from, Message::UpgradeNotice(notice).encode());
        }
    }

    /// The notice to hand to a member refused for its protocol: the release this
    /// node runs, once the cluster's upgrade to it has settled.
    fn current_notice(&self) -> Option<Vec<u8>> {
        let running = self.installer().current();
        let up = self.upgrades.lock().unwrap();
        up.saved
            .intent
            .as_ref()
            .is_some_and(|i| i.tag == running.tag())
            .then(|| {
                Message::UpgradeNotice(UpgradeNotice {
                    release: running.tag(),
                    protocol: running.protocol,
                })
                .encode()
            })
    }

    /// One tick of upgrade work: stage the release, then, as the mode says, wait
    /// for this member's turn or for the go-signal, and swap. Whether anything happened.
    pub(super) fn drive_upgrade(&self) -> bool {
        let Some(me) = self.membership.lock().unwrap().me() else {
            return false;
        };
        if let Some(transport) = self.transport() {
            transport.set_notice(self.current_notice());
        }
        let installer = self.installer();
        let running = installer.current();
        let now = self.clock.now();
        if self.upgrades.lock().unwrap().saved.intent.is_none() {
            return self.tell_upgrade_changes();
        }
        let views = self.peer_views(now);
        let mut busy = false;
        let action = {
            let mut up = self.upgrades.lock().unwrap();
            let action = self.plan_upgrade(&mut up, &*installer, &running, &views, me, now);
            // The pause or the swap not taking hold ends in a failure of its own.
            if let (Some(at), Some(intent)) = (up.swapped_at, up.saved.intent.clone())
                && running.tag() != intent.tag
                && now.saturating_sub(at) >= UPGRADE_PAUSE.as_nanos() as u64
            {
                up.swapped_at = None;
                up.pause_until = None;
                up.fail("the daemon was not restarted on the new release".into());
                up.progress = Some(Progress::Failed(
                    "the daemon was not restarted on the new release".into(),
                ));
                busy = true;
            }
            action
        };
        match action {
            Action::Nothing => {}
            Action::Cancel => {
                let mut up = self.upgrades.lock().unwrap();
                if let Some(intent) = up.saved.intent.take() {
                    up.saved.cancelled = up.saved.cancelled.max(intent.rev);
                }
                up.stage = Stage::Idle;
                up.progress = None;
                up.forced = false;
                up.save();
                drop(up);
                installer.discard();
                busy = true;
            }
            Action::Go => {
                let mut up = self.upgrades.lock().unwrap();
                if let Some(intent) = up.saved.intent.as_mut() {
                    intent.go = true;
                }
                up.save();
                busy = true;
            }
            Action::Swap { pause } => {
                if !pause || self.gate_open(now) {
                    self.swap(&*installer, pause, now);
                    busy = true;
                }
            }
        }
        busy | self.tell_upgrade_changes()
    }

    /// Stage the release and decide what to do about it. Holds the upgrade state.
    fn plan_upgrade(
        &self,
        up: &mut Upgrades,
        installer: &dyn Installer,
        running: &Release,
        views: &[Peer],
        me: MemberId,
        now: Nanos,
    ) -> Action {
        let Some(intent) = up.saved.intent.clone() else {
            up.progress = None;
            return Action::Nothing;
        };
        if running.tag() == intent.tag {
            // Done here: drop what was kept, and what the restart left open.
            if up.stage != Stage::Idle {
                installer.discard();
                up.stage = Stage::Idle;
            }
            if up.saved.swapping.is_some() {
                up.saved.swapping = None;
                up.save();
            }
            up.forced = false;
            up.pause_until = None;
            up.swapped_at = None;
            up.progress = None;
            return Action::Nothing;
        }
        if let Some(failure) = up.saved.failure.clone().filter(|f| f.rev == intent.rev) {
            up.progress = Some(Progress::Failed(failure.why));
            return Action::Nothing;
        }
        if up.swapped_at.is_some() {
            up.progress = Some(Progress::Restarting);
            return Action::Nothing;
        }
        if up.stage == Stage::Idle {
            installer.begin_stage(&intent.tag);
            up.stage = Stage::Fetching;
        }
        if up.stage == Stage::Fetching {
            match installer.staged() {
                None => {
                    up.progress = Some(Progress::Staging);
                    return Action::Nothing;
                }
                Some(Ok(staged)) if staged.tag() == intent.tag => up.stage = Stage::Ready(staged),
                Some(Ok(staged)) => {
                    up.fail(format!("staged {} instead of {}", staged.tag(), intent.tag));
                    return Action::Nothing;
                }
                Some(Err(why)) => {
                    up.fail(why);
                    return Action::Nothing;
                }
            }
        }
        let Stage::Ready(staged) = up.stage.clone() else {
            return Action::Nothing;
        };
        let lockstep = staged.protocol != running.protocol;
        // A mount in use holds the restart until the user frees it or forces it.
        let waiting = !up.forced && {
            let recheck = up.mount.is_none_or(|(at, _)| {
                now.saturating_sub(at) >= MOUNT_RECHECK.as_nanos() as u64
            });
            if recheck {
                up.mount = Some((now, installer.mount_in_use()));
            }
            up.mount.is_some_and(|(_, busy)| busy)
        };
        up.progress = Some(if waiting {
            Progress::WaitingOnMount
        } else {
            Progress::Staged
        });
        if waiting {
            return Action::Nothing;
        }
        if !lockstep {
            return if rolling_turn(views, me, &intent, up.pause_until, now) {
                Action::Swap { pause: true }
            } else {
                Action::Nothing
            };
        }
        if intent.go {
            return Action::Swap { pause: false };
        }
        if intent.issuer == Some(me) {
            let ready = views.iter().filter(|p| p.up).all(|p| {
                p.runs(&intent.tag)
                    || p.gossip.as_ref().is_some_and(|g| {
                        g.intent.as_ref().is_some_and(|i| i.rev == intent.rev)
                            && g.progress == Some(Progress::Staged)
                    })
            });
            return if ready { Action::Go } else { Action::Nothing };
        }
        let lapsed = intent.issuer.is_some_and(|issuer| {
            views.iter().find(|p| p.id == issuer).is_none_or(|p| {
                !p.up
                    && now.saturating_sub(p.heard.unwrap_or(self.started))
                        >= STAGE_LEASE.as_nanos() as u64
            })
        });
        if lapsed {
            Action::Cancel
        } else {
            Action::Nothing
        }
    }

    /// Whether the data is safe enough for the next member to restart: nothing is
    /// short of copies, whoever is absent. Looked at only now and then.
    fn gate_open(&self, now: Nanos) -> bool {
        {
            let up = self.upgrades.lock().unwrap();
            if let Some((at, open)) = up.gate
                && now.saturating_sub(at) < GATE_RECHECK.as_nanos() as u64
            {
                return open;
            }
        }
        let open = self.status_with_gate().is_ok_and(|(_, open)| open);
        self.upgrades.lock().unwrap().gate = Some((now, open));
        open
    }

    /// Hand the staged release to the installer, announcing the pause first when
    /// the others are to expect this member back.
    fn swap(&self, installer: &dyn Installer, pause: bool, now: Nanos) {
        let tag = {
            let mut up = self.upgrades.lock().unwrap();
            let Some(intent) = up.saved.intent.clone() else {
                return;
            };
            up.saved.swapping = Some(intent.tag.clone());
            up.save();
            up.swapped_at = Some(now);
            up.progress = Some(Progress::Restarting);
            if pause {
                up.pause_until = Some(now + UPGRADE_PAUSE.as_nanos() as u64);
            }
            intent.tag
        };
        self.announce_upgrade();
        if let Err(why) = installer.swap() {
            let mut up = self.upgrades.lock().unwrap();
            up.swapped_at = None;
            up.pause_until = None;
            up.fail(format!("cannot swap in {tag}: {why}"));
            up.progress = Some(Progress::Failed(why));
        }
    }

    /// Tell the others when this node's part of the upgrade changed.
    fn tell_upgrade_changes(&self) -> bool {
        let now = self.clock.now();
        let told = {
            let up = self.upgrades.lock().unwrap();
            Told {
                intent: up.saved.intent.clone(),
                progress: up.progress.clone(),
                paused: up.pause_until.is_some_and(|t| t > now),
            }
        };
        let changed = {
            let mut up = self.upgrades.lock().unwrap();
            let changed = up.told.as_ref() != Some(&told);
            up.told = Some(told);
            changed
        };
        if changed {
            self.announce_upgrade();
        }
        changed
    }

    /// The upgrade as `status` reports it, from this node's view.
    pub(super) fn upgrade_status(&self, now: Nanos) -> Option<UpgradeStatus> {
        let running = self.installer().current();
        let views = self.peer_views(now);
        let up = self.upgrades.lock().unwrap();
        let intent = up.saved.intent.clone()?;
        let lockstep = match &up.stage {
            Stage::Ready(staged) => Some(staged.protocol != running.protocol),
            _ if intent.tag == running.tag() => None,
            _ => None,
        };
        let complete = running.tag() == intent.tag
            && views.iter().filter(|p| p.up).all(|p| p.runs(&intent.tag));
        let split = views
            .iter()
            .filter(|p| self.protocol_of(&up, p.id).is_some_and(|n| n != running.protocol))
            .map(|p| p.id)
            .collect();
        Some(UpgradeStatus {
            release: intent.tag,
            lockstep,
            go: intent.go,
            complete,
            split,
        })
    }

    /// The release, protocol and upgrade progress of a member, as far as this node knows.
    pub(super) fn member_upgrade(
        &self,
        endpoint: NodeId,
        is_self: bool,
    ) -> (Option<String>, Option<u8>, Option<Progress>) {
        let up = self.upgrades.lock().unwrap();
        if is_self {
            let running = self.installer().current();
            return (Some(running.tag()), Some(running.protocol), up.progress.clone());
        }
        match up.peers.get(&endpoint) {
            Some(p) => (
                Some(p.gossip.running.tag()),
                Some(p.gossip.running.protocol),
                p.gossip.progress.clone(),
            ),
            None => (None, up.protocols.get(&endpoint).copied(), None),
        }
    }

    fn protocol_of(&self, up: &Upgrades, member: MemberId) -> Option<u8> {
        let endpoint = self
            .membership
            .lock()
            .unwrap()
            .members()
            .into_iter()
            .find(|m| m.id == member)?
            .endpoint;
        up.protocols
            .get(&endpoint)
            .copied()
            .or_else(|| up.peers.get(&endpoint).map(|p| p.gossip.running.protocol))
    }
}

/// Whether this member is the next to restart in a rolling upgrade: nobody is
/// paused or still away from a restart, and this is the lowest-numbered member that
/// is up and still to do. Members that failed, wait on a mount or are absent do not
/// hold it up.
fn rolling_turn(
    views: &[Peer],
    me: MemberId,
    intent: &Intent,
    own_pause: Option<Nanos>,
    now: Nanos,
) -> bool {
    if own_pause.is_some_and(|t| t > now) || views.iter().any(|p| p.paused) {
        return false;
    }
    // A member that went down to restart and is not back holds the upgrade up: that
    // is not an absent member, whose restart this upgrade never began.
    if views
        .iter()
        .any(|p| !p.runs(&intent.tag) && p.progress() == Some(&Progress::Restarting))
    {
        return false;
    }
    let next = views
        .iter()
        .filter(|p| p.up && !p.runs(&intent.tag))
        .filter(|p| {
            !matches!(
                p.progress(),
                Some(Progress::Failed(_) | Progress::WaitingOnMount)
            )
        })
        .map(|p| p.id)
        .chain(std::iter::once(me))
        .min();
    next == Some(me)
}
