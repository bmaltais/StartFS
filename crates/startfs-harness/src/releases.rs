//! Releases for the nodes to upgrade to, and the installer that stands in for the
//! real one: it stages from a registry the test fills, and a swap takes the node
//! down and brings it back on the new release.

use startfs_core::protocol::PROTOCOL_VERSION;
use startfs_core::upgrade::{InstallError, Installer, Release, is_newer};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

struct Published {
    tag: String,
    protocol: u8,
    prerelease: bool,
}

/// The releases a test published, shared by every node's installer.
#[derive(Default)]
pub(crate) struct Releases {
    published: Mutex<Vec<Published>>,
}

impl Releases {
    pub(crate) fn publish(&self, tag: &str, protocol: u8, prerelease: bool) {
        self.published.lock().unwrap().push(Published {
            tag: tag.to_string(),
            protocol,
            prerelease,
        });
    }

    fn find(&self, tag: &str) -> Option<Release> {
        let published = self.published.lock().unwrap();
        let p = published.iter().find(|p| p.tag == tag)?;
        Some(Release {
            version: p.tag.trim_start_matches('v').to_string(),
            protocol: p.protocol,
        })
    }

    fn latest_stable(&self) -> Option<String> {
        let published = self.published.lock().unwrap();
        published
            .iter()
            .filter(|p| !p.prerelease)
            .map(|p| p.tag.clone())
            .reduce(|a, b| if is_newer(&b, &a) { b } else { a })
    }
}

/// One node's installer. It outlives the node's restarts, as the binary on disk does.
pub(crate) struct NodeRelease {
    releases: Arc<Releases>,
    running: Mutex<Release>,
    /// The protocol the node's transport speaks: the running release's.
    wire: Arc<AtomicU8>,
    staged: Mutex<Option<Release>>,
    /// How the last attempt to stage went.
    last: Mutex<Option<Result<Release, InstallError>>>,
    /// Releases this node cannot fetch, with the reason.
    broken: Mutex<Vec<(String, String)>>,
    mount_in_use: AtomicBool,
    /// A swap was handed over and the node has not been taken down for it yet.
    swap: Mutex<Option<Release>>,
    /// The next swap fails to start the new daemon: the node comes back on the old release.
    swap_fails: AtomicBool,
}

impl NodeRelease {
    pub(crate) fn new(releases: Arc<Releases>) -> Self {
        Self {
            releases,
            running: Mutex::new(Release {
                version: env!("CARGO_PKG_VERSION").to_string(),
                protocol: PROTOCOL_VERSION,
            }),
            wire: Arc::new(AtomicU8::new(PROTOCOL_VERSION)),
            staged: Mutex::new(None),
            last: Mutex::new(None),
            broken: Mutex::new(Vec::new()),
            mount_in_use: AtomicBool::new(false),
            swap: Mutex::new(None),
            swap_fails: AtomicBool::new(false),
        }
    }

    /// The protocol the node's transport speaks, shared with it.
    pub(crate) fn wire(&self) -> Arc<AtomicU8> {
        self.wire.clone()
    }

    pub(crate) fn tag(&self) -> String {
        self.running.lock().unwrap().tag()
    }

    pub(crate) fn staged_tag(&self) -> Option<String> {
        self.staged.lock().unwrap().as_ref().map(Release::tag)
    }

    pub(crate) fn set_protocol(&self, protocol: u8) {
        self.running.lock().unwrap().protocol = protocol;
        self.wire.store(protocol, Ordering::SeqCst);
    }

    pub(crate) fn set_version(&self, version: &str) {
        self.running.lock().unwrap().version = version.to_string();
    }

    pub(crate) fn set_mount_in_use(&self, in_use: bool) {
        self.mount_in_use.store(in_use, Ordering::SeqCst);
    }

    pub(crate) fn break_release(&self, tag: &str, why: &str) {
        self.broken
            .lock()
            .unwrap()
            .push((tag.to_string(), why.to_string()));
    }

    pub(crate) fn fail_next_swap(&self) {
        self.swap_fails.store(true, Ordering::SeqCst);
    }

    /// Whether a swap was handed over and the node is still to be taken down for it.
    pub(crate) fn swap_requested(&self) -> bool {
        self.swap.lock().unwrap().is_some()
    }

    /// The node is down for the swap: what it was handed is now what it runs, or,
    /// when the swap fails, what it ran before.
    pub(crate) fn finish_swap(&self) {
        let Some(next) = self.swap.lock().unwrap().take() else {
            return;
        };
        if self.swap_fails.swap(false, Ordering::SeqCst) {
            return;
        }
        self.wire.store(next.protocol, Ordering::SeqCst);
        *self.running.lock().unwrap() = next;
        *self.staged.lock().unwrap() = None;
        *self.last.lock().unwrap() = None;
    }
}

impl Installer for NodeRelease {
    fn current(&self) -> Release {
        self.running.lock().unwrap().clone()
    }

    fn latest_stable(&self) -> Result<String, InstallError> {
        self.releases
            .latest_stable()
            .ok_or_else(|| "no release is published".to_string())
    }

    fn begin_stage(&self, tag: &str) {
        let broken = self
            .broken
            .lock()
            .unwrap()
            .iter()
            .find(|(t, _)| t == tag)
            .map(|(_, why)| why.clone());
        let result = match (broken, self.releases.find(tag)) {
            (Some(why), _) => Err(why),
            (None, None) => Err(format!("no release {tag}")),
            (None, Some(release)) => Ok(release),
        };
        if let Ok(release) = &result {
            *self.staged.lock().unwrap() = Some(release.clone());
        }
        *self.last.lock().unwrap() = Some(result);
    }

    fn staged(&self) -> Option<Result<Release, InstallError>> {
        self.last.lock().unwrap().clone()
    }

    fn discard(&self) {
        *self.staged.lock().unwrap() = None;
        *self.last.lock().unwrap() = None;
    }

    fn mount_in_use(&self) -> Option<bool> {
        Some(self.mount_in_use.load(Ordering::SeqCst))
    }

    fn swap(&self) -> Result<(), InstallError> {
        let staged = self
            .staged
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "nothing is staged".to_string())?;
        *self.swap.lock().unwrap() = Some(staged);
        Ok(())
    }
}
