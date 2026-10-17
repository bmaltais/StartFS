//! Membership flows: creating, joining, leaving and removing members, and
//! minting or revoking join tokens.

use super::{DOWN_AFTER, Daemon};
use crate::control::{FilesystemInfo, Verdict};
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

    pub(super) fn join(&self, token: &str, offer_bytes: Option<u64>) -> Result<(), Error> {
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
        self.fs.set_node(reply.id);
        self.membership.lock().unwrap().install(reply, offer)?;
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
        membership.mint_token(self.clock.now() + ttl.as_nanos() as u64)
    }

    /// Start a filesystem with this node as its first member; returns the first join token.
    pub(super) fn create_filesystem(&self, name: String, copies: u32) -> Result<String, Error> {
        let mut membership = self.membership.lock().unwrap();
        if membership.filesystem().is_some() {
            return Err(Error::AlreadyInFilesystem);
        }
        if name.is_empty() || copies == 0 {
            return Err(Error::BadFilesystemSpec);
        }
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
            },
            endpoint,
            self.addr(),
            self.config.offer_bytes,
        )?;
        let token = self.mint(&mut membership)?;
        self.fs.set_replicating();
        Ok(token)
    }

    /// Mint another join token and tell the other members about it.
    pub(super) fn new_token(&self) -> Result<String, Error> {
        let token = self.mint(&mut self.membership.lock().unwrap())?;
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
                .is_some_and(|x| {
                    self.heard
                        .lock()
                        .unwrap()
                        .get(&x.endpoint)
                        .is_some_and(|t| {
                            self.clock.now().saturating_sub(*t) < DOWN_AFTER.as_nanos() as u64
                        })
                });
            if up {
                return Err(Error::MemberOnline(member));
            }
            m.remove(member)?;
        }
        self.sync_peers();
        self.announce_members();
        Ok(())
    }

    /// Change this node's space offer; `None` leaves it as it is.
    pub(super) fn configure(
        &self,
        offer_bytes: Option<u64>,
        copies: Option<u32>,
    ) -> Result<(), Error> {
        if copies == Some(0) {
            return Err(Error::BadFilesystemSpec);
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
        self.fs.set_staging_limit(super::staging_limit(
            &self.config,
            membership.my_offer(self.config.offer_bytes),
        ));
        Ok(())
    }
}
