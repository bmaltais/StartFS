//! Membership flows: creating, joining, leaving and removing members, and
//! minting or revoking join tokens.

use super::Daemon;
use crate::control::{FilesystemInfo, Verdict};
use crate::crypto::{FsKey, RecoveryKey};
use crate::error::Error;
use crate::members::{JoinRequest, MemberId, Membership, Token};
use crate::net::{NodeId, Transport};
use crate::protocol::Message;

impl Daemon {
    /// A leaving node goes once its data is on the others and they have its log.
    pub(super) fn finish_leaving(&self, transport: &dyn Transport) -> bool {
        let (leaving, peers) = {
            let m = self.membership.lock().unwrap();
            (m.is_leaving(), m.peers())
        };
        if !leaving
            || !self.fs.held().unwrap_or_default().is_empty()
            || !self.fs.staged().unwrap_or_default().is_empty()
        {
            return false;
        }
        let gone: Vec<NodeId> = self
            .with_view(|v| {
                v.members
                    .iter()
                    .filter(|m| v.lost.contains(&m.id))
                    .map(|m| m.endpoint)
                    .collect()
            })
            .unwrap_or_default();
        let synced = self
            .syncer
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|s| s.peers_have_our_log(&gone));
        if !synced {
            return false;
        }
        let Some(records) = self.membership.lock().unwrap().finish_leave() else {
            return false;
        };
        let msg = Message::Members(records).encode();
        for p in peers {
            transport.send(p, msg.clone());
        }
        true
    }

    /// Whether this node is a storage-only member: it never mounts the filesystem.
    pub fn is_storage_only(&self) -> bool {
        self.membership.lock().unwrap().is_storage_only()
    }

    pub(super) fn join(
        &self,
        token: &str,
        offer_bytes: Option<u64>,
        site: String,
        storage_only: bool,
        recovery_key: Option<String>,
    ) -> Result<(), Error> {
        let recovery = recovery_key
            .as_deref()
            .map(RecoveryKey::parse)
            .transpose()?;
        if storage_only && offer_bytes.unwrap_or(self.config.offer_bytes) == 0 {
            return Err(Error::StorageOnlyWithoutOffer);
        }
        if storage_only && self.mountpoint.lock().unwrap().is_some() {
            return Err(Error::MountedCannotBeStorageOnly);
        }
        let token = Token::parse(token)?;
        if self.membership.lock().unwrap().filesystem().is_some() {
            return Err(Error::AlreadyInFilesystem);
        }
        if !self.fs.frontier()?.is_empty() {
            return Err(Error::NotFresh);
        }
        let transport = self.transport().ok_or(Error::NoNetwork)?;
        let bootstrap = transport
            .resolve(&token.addr)
            .ok_or_else(|| Error::Unreachable(token.addr.clone()))?;
        let offer = offer_bytes.unwrap_or(self.config.offer_bytes);
        *self.join_reply.lock().unwrap() = None;
        transport.send(
            bootstrap,
            Message::JoinRequest(JoinRequest {
                fs_id: token.fs_id,
                token_id: token.token_id,
                addr: self.addr(),
                offer_bytes: offer,
                site: site.trim().to_string(),
                storage_only,
                key_proof: recovery.as_ref().map(RecoveryKey::proof),
            })
            .encode(),
        );
        let reply = self
            .wait_for(|| self.join_reply.lock().unwrap().take())
            .ok_or_else(|| match transport.last_error(bootstrap) {
                Some(why) => Error::NoAnswerBecause {
                    addr: token.addr.clone(),
                    why,
                },
                None => Error::NoAnswer {
                    addr: token.addr.clone(),
                },
            })??;
        // Open the filesystem key before anything is kept: a key that does not open it
        // leaves this node as it was.
        let fs_key = if reply.fs.encrypted {
            let recovery = recovery.as_ref().ok_or(Error::RecoveryKeyNeeded)?;
            let wrap = reply
                .key_wraps
                .iter()
                .find(|w| w.accepts(&recovery.proof()))
                .ok_or(Error::RecoveryKeyWrong)?;
            Some(wrap.unwrap(recovery)?)
        } else {
            None
        };
        if let Some(key) = fs_key {
            self.fs.set_encryption(key)?;
        }
        self.fs.set_node(reply.id);
        {
            let mut membership = self.membership.lock().unwrap();
            membership.install(reply, offer)?;
            self.fs.set_storage_only(membership.is_storage_only());
        }
        self.fs.set_replicating();
        Ok(())
    }

    /// Tell every member about a change to the member list right away.
    pub(super) fn announce_members(&self) {
        let Some(transport) = self.transport() else {
            return;
        };
        let (peers, records) = {
            let m = self.membership.lock().unwrap();
            (m.peers(), m.records())
        };
        let msg = Message::Members(records).encode();
        for p in peers {
            transport.send(p, msg.clone());
        }
    }

    /// Tell every member about a new or revoked token right away.
    pub(super) fn announce_tokens(&self) {
        let Some(transport) = self.transport() else {
            return;
        };
        let (peers, tokens) = {
            let m = self.membership.lock().unwrap();
            (m.peers(), m.token_records())
        };
        let msg = Message::Tokens(tokens).encode();
        for p in peers {
            transport.send(p, msg.clone());
        }
    }

    fn mint(&self, membership: &mut Membership) -> Result<String, Error> {
        let ttl = std::time::Duration::from_secs(self.config.token_ttl_secs);
        self.mint_for(membership, ttl)
    }

    fn mint_for(
        &self,
        membership: &mut Membership,
        ttl: std::time::Duration,
    ) -> Result<String, Error> {
        membership.mint_token(self.clock.now() + ttl.as_nanos() as u64)
    }

    /// Start a filesystem with this node as its first member; returns the first join token.
    pub(super) fn create_filesystem(
        &self,
        name: String,
        copies: u32,
        encrypt: bool,
    ) -> Result<(String, Option<String>), Error> {
        let mut membership = self.membership.lock().unwrap();
        if membership.filesystem().is_some() {
            return Err(Error::AlreadyInFilesystem);
        }
        if name.is_empty() || copies == 0 {
            return Err(Error::BadFilesystemSpec);
        }
        if encrypt && !self.fs.frontier()?.is_empty() {
            return Err(Error::EncryptNeedsFresh);
        }
        // Keys first: if the node cannot start encrypting, nothing else has changed.
        let keys = if encrypt {
            let (key, recovery) = (FsKey::generate(), RecoveryKey::generate());
            self.fs.set_encryption(key.clone())?;
            Some((key, recovery))
        } else {
            None
        };
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).expect("os randomness");
        let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let endpoint = self.transport().map_or(NodeId(0), |t| t.local());
        membership.create(
            FilesystemInfo {
                id,
                name,
                copies,
                copies_rev: 0,
                repair_grace_secs: self.config.repair_grace_secs,
                gc_retention_secs: self.config.gc_retention_secs,
                standin_days: self.config.standin_days,
                keep_versions_secs: self.config.keep_versions_secs,
                keep_versions_rev: 0,
                encrypted: encrypt,
            },
            endpoint,
            self.addr(),
            self.config.offer_bytes,
        )?;
        let recovery = keys.map(|(key, recovery)| {
            membership.add_key_wrap(recovery.wrap(&key))?;
            Ok::<_, Error>(recovery.to_string())
        });
        let recovery = recovery.transpose()?;
        let token = self.mint(&mut membership)?;
        self.fs.set_replicating();
        Ok((token, recovery))
    }

    /// Make another recovery key for the filesystem, from the key this running member holds,
    /// and tell the others its wrap. Nothing is re-encrypted; the older keys keep working.
    pub(super) fn new_recovery_key(&self) -> Result<String, Error> {
        let key = self.fs.encryption_key().ok_or_else(|| {
            if self
                .membership
                .lock()
                .unwrap()
                .filesystem()
                .is_some_and(|f| f.encrypted)
            {
                Error::NoKeyHere
            } else {
                Error::NotEncrypted
            }
        })?;
        let recovery = RecoveryKey::generate();
        self.membership
            .lock()
            .unwrap()
            .add_key_wrap(recovery.wrap(&key))?;
        self.announce_key_wraps();
        Ok(recovery.to_string())
    }

    fn announce_key_wraps(&self) {
        let Some(transport) = self.transport() else {
            return;
        };
        let (peers, wraps) = {
            let m = self.membership.lock().unwrap();
            (m.peers(), m.key_wraps())
        };
        let msg = Message::KeyWraps(wraps).encode();
        for p in peers {
            transport.send(p, msg.clone());
        }
    }

    /// Mint another join token and tell the other members about it.
    pub(super) fn new_token(&self) -> Result<String, Error> {
        self.new_token_for(std::time::Duration::from_secs(self.config.token_ttl_secs))
    }

    /// Like `new_token`, but valid for `ttl` instead of the configured time.
    pub(super) fn new_token_for(&self, ttl: std::time::Duration) -> Result<String, Error> {
        let token = self.mint_for(&mut self.membership.lock().unwrap(), ttl)?;
        self.announce_tokens();
        Ok(token)
    }

    pub(super) fn revoke_token(&self, token: &str) -> Result<(), Error> {
        self.membership.lock().unwrap().revoke_token(token)?;
        self.announce_tokens();
        Ok(())
    }

    pub(super) fn begin_leave(&self) -> Result<(), Error> {
        self.membership.lock().unwrap().begin_leave()?;
        self.announce_members();
        Ok(())
    }

    /// Remove a member that is gone for good; one still online must leave itself.
    pub(super) fn remove_member(&self, member: MemberId) -> Result<(), Error> {
        {
            let mut m = self.membership.lock().unwrap();
            let up = m
                .members()
                .into_iter()
                .find(|x| x.id == member)
                .is_some_and(|x| self.liveness.is_up(x.endpoint, self.clock.now()));
            if up {
                return Err(Error::MemberOnline(member));
            }
            m.remove(member)?;
        }
        self.sync_peers();
        self.announce_members();
        Ok(())
    }

    /// Change this node's space offer, weight, site, role or the copies; `None` leaves each as it is.
    pub(super) fn configure(
        &self,
        offer_bytes: Option<u64>,
        weight_bytes: Option<u64>,
        copies: Option<u32>,
        activity: Option<bool>,
        site: Option<String>,
        storage_only: Option<bool>,
    ) -> Result<(), Error> {
        if copies == Some(0) {
            return Err(Error::BadFilesystemSpec);
        }
        {
            // A storage-only member has to offer space, and cannot have the filesystem
            // mounted. Judged before anything is changed.
            let membership = self.membership.lock().unwrap();
            let storage = storage_only.unwrap_or_else(|| membership.is_storage_only());
            let offer = offer_bytes.unwrap_or_else(|| membership.my_offer(self.config.offer_bytes));
            if storage && offer == 0 {
                return Err(Error::StorageOnlyWithoutOffer);
            }
            if storage_only == Some(true)
                && !membership.is_storage_only()
                && self.mountpoint.lock().unwrap().is_some()
            {
                return Err(Error::MountedCannotBeStorageOnly);
            }
        }
        if let Some(on) = activity {
            self.set_activity(on);
        }
        // Lowering drops copies, so only do it when none of them is still being made
        // or out of reach. Judged before taking the lock (status takes it too), so
        // remember what it was judged against and refuse if that moved meanwhile.
        let judged = self.membership.lock().unwrap().copies_setting();
        let lowering = copies.zip(judged).is_some_and(|(n, (now, _))| n < now);
        // Best effort is as healthy as safe: it only means fewer nodes than copies wanted.
        if lowering
            && !matches!(
                self.status()?.verdict,
                Some(Verdict::Safe | Verdict::BestEffort)
            )
        {
            return Err(Error::NotHealthyToLowerCopies);
        }
        let mut membership = self.membership.lock().unwrap();
        if lowering && membership.copies_setting() != judged {
            return Err(Error::NotHealthyToLowerCopies);
        }
        if let Some(copies) = copies {
            membership.set_copies(copies)?;
        }
        if let Some(bytes) = offer_bytes {
            membership.set_offer(bytes)?;
        }
        if let Some(bytes) = weight_bytes {
            membership.set_weight(bytes)?;
        }
        if let Some(site) = site {
            membership.set_site(site.trim())?;
        }
        if let Some(on) = storage_only {
            membership.set_storage_only(on)?;
            self.fs.set_storage_only(on);
        }
        self.refresh_staging_limit();
        Ok(())
    }
}
