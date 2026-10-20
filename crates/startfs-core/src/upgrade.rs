//! Upgrades the cluster coordinates itself (ADR 0007): what members gossip about
//! them, what a daemon keeps across the restart that ends one, and the
//! [`Installer`] seam through which it fetches and swaps a release.

use crate::members::MemberId;
use crate::protocol::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A release a daemon can run: its version, without the leading `v`, and the
/// protocol version it speaks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    pub protocol: u8,
}

impl Release {
    /// The tag the release is published under, as `v1.10.0`.
    pub fn tag(&self) -> String {
        format!("v{}", self.version)
    }
}

/// Whether the release tag `candidate` is later than `than`. Numbers compare as
/// numbers (`v1.10.0` is later than `v1.9.0`) and a pre-release is earlier than its
/// release. A tag that is not a version is later than nothing.
pub fn is_newer(candidate: &str, than: &str) -> bool {
    fn key(tag: &str) -> Option<(Vec<u64>, bool)> {
        let version = tag.strip_prefix('v').unwrap_or(tag);
        let (numbers, pre) = match version.split_once('-') {
            Some((numbers, _)) => (numbers, true),
            None => (version, false),
        };
        let numbers = numbers
            .split('.')
            .map(|n| n.parse().ok())
            .collect::<Option<Vec<u64>>>()?;
        Some((numbers, !pre))
    }
    matches!((key(candidate), key(than)), (Some(a), Some(b)) if a > b)
}

/// The release the cluster is moving to, as members gossip it. The issuer resolves
/// it once to a concrete tag, so a release published mid-way cannot split the cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub tag: String,
    /// Bumped by each `update`; the highest wins when members disagree.
    pub rev: u64,
    /// The member that ordered it. `None` for an intent a member made for itself
    /// from an upgrade notice.
    pub issuer: Option<MemberId>,
    /// The go-signal of a lockstep upgrade, sent by the issuer once every member
    /// that is up has staged the release.
    pub go: bool,
}

/// Where a member is in carrying out the intent. Absent when it has nothing to do:
/// it already runs the release, or there is no intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Progress {
    /// Fetching and checking the release.
    Staging,
    /// The release is staged and the member may restart on the word.
    Staged,
    /// Staged, but the mount is in use: the member waits for the user.
    WaitingOnMount,
    /// Swapped in; the member is about to restart.
    Restarting,
    /// Could not stage or swap the release.
    Failed(String),
}

/// What a member gossips about upgrades: always the release it runs, and the intent
/// and its own progress while an upgrade is under way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeGossip {
    pub running: Release,
    pub intent: Option<Intent>,
    pub progress: Option<Progress>,
    /// While the member's upgrade pause lasts, the seconds left on it: a duration,
    /// since members' clocks do not agree.
    pub pause_secs: Option<u64>,
}

/// Why a release could not be staged or swapped in, in words for the user.
pub type InstallError = String;

/// How a daemon fetches and swaps a release. The daemon has no download path of its
/// own: the real one runs the installer's fetch, checksum, swap and rollback logic,
/// and the harness stands one in.
pub trait Installer: Send + Sync {
    /// The release this daemon runs.
    fn current(&self) -> Release;
    /// The tag of the latest stable release (never a pre-release).
    fn latest_stable(&self) -> Result<String, InstallError>;
    /// Start fetching and checking `tag`, keeping it next to the running binary
    /// without swapping it. Never waits; a release already staged is not fetched again.
    fn begin_stage(&self, tag: &str);
    /// How the last [`begin_stage`](Self::begin_stage) went: `None` while it is under way.
    /// The staged release's protocol comes from running the staged binary.
    fn staged(&self) -> Option<Result<Release, InstallError>>;
    /// Drop what was staged.
    fn discard(&self);
    /// Whether processes are using the mount, which a restart would take from them.
    fn mount_in_use(&self) -> bool;
    /// Swap the staged release in and restart the daemon on it. Returns once the swap
    /// has been handed off; the process is replaced, so on success the caller is
    /// about to disappear. The installer rolls the node back by itself if the new
    /// daemon does not answer.
    fn swap(&self) -> Result<(), InstallError>;
}

/// An installer for a daemon that cannot upgrade itself, such as one embedded in a test.
pub struct NoInstaller;

impl Installer for NoInstaller {
    fn current(&self) -> Release {
        Release {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
        }
    }

    fn latest_stable(&self) -> Result<String, InstallError> {
        Err("this daemon cannot look up releases".into())
    }

    fn begin_stage(&self, _tag: &str) {}

    fn staged(&self) -> Option<Result<Release, InstallError>> {
        Some(Err("this daemon cannot upgrade itself".into()))
    }

    fn discard(&self) {}

    fn mount_in_use(&self) -> bool {
        false
    }

    fn swap(&self) -> Result<(), InstallError> {
        Err("this daemon cannot upgrade itself".into())
    }
}

/// What a daemon remembers about upgrades across restarts, so the restart that ends
/// one does not forget what the cluster was moving to.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Saved {
    /// The release the cluster is moving to, or is on.
    pub intent: Option<Intent>,
    /// The highest intent revision that was abandoned: nothing at or below it is taken up.
    #[serde(default)]
    pub cancelled: u64,
    /// The release this node handed to the installer to swap in. Still set after a
    /// restart on another release means the swap was rolled back.
    #[serde(default)]
    pub swapping: Option<String>,
    /// A release this node could not take, and why; not retried for the same intent.
    #[serde(default)]
    pub failure: Option<Failure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Failure {
    pub rev: u64,
    pub why: String,
}

pub(crate) struct SavedFile {
    path: PathBuf,
}

impl SavedFile {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("upgrade.json"),
        }
    }

    pub fn load(&self) -> Saved {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// A failure is reported and the in-memory state stays authoritative until the next save.
    pub fn save(&self, saved: &Saved) {
        let tmp = self.path.with_extension("tmp");
        let text = serde_json::to_string_pretty(saved).expect("serialises");
        if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &self.path)) {
            eprintln!("startfs: cannot save the upgrade state: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_tags_compare_as_versions() {
        assert!(is_newer("v1.10.0", "v1.9.0"));
        assert!(!is_newer("v1.9.0", "v1.10.0"));
        assert!(!is_newer("v1.9.0", "v1.9.0"));
        assert!(is_newer("v2.0.0", "v2.0.0-rc.1"));
        assert!(!is_newer("v2.0.0-rc.1", "v2.0.0"));
        assert!(!is_newer("latest", "v1.0.0"));
    }
}
