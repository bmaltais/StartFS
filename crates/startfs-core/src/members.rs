//! Who belongs to the filesystem: the member list, join tokens and admission.
//!
//! A token reads `startfs1-<filesystem id>.<token id>@<host>:<port>`: the
//! address is the minting member's, which is the member that admits the
//! joiner. Tokens expire and can be revoked; both are checked by the admitting
//! member. Member ids are assigned at admission and never reused.

use crate::control::FilesystemInfo;
use crate::net::NodeId;
use crate::ops::Nanos;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Assigned at admission; tags the member's log entries and inode numbers.
    pub id: u32,
    /// Where the member is reached on the transport.
    pub endpoint: NodeId,
    /// The `host:port` the member advertises.
    pub addr: String,
    /// Space the member offers to the filesystem, in bytes.
    pub offer_bytes: u64,
    /// Bumped by the member each time it changes its own record; the highest wins.
    pub rev: u64,
}

/// What an admitting member tells a new member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub fs: FilesystemInfo,
    pub id: u32,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenRec {
    id: String,
    expires: Nanos,
    revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub fs_id: String,
    pub token_id: String,
    pub addr: String,
}

impl Token {
    pub fn parse(text: &str) -> Result<Token, String> {
        let bad = || "not a StartFS join token".to_string();
        let rest = text.trim().strip_prefix("startfs1-").ok_or_else(bad)?;
        let (ids, addr) = rest.split_once('@').ok_or_else(bad)?;
        let (fs_id, token_id) = ids.split_once('.').ok_or_else(bad)?;
        if fs_id.is_empty() || token_id.is_empty() || addr.is_empty() {
            return Err(bad());
        }
        Ok(Token {
            fs_id: fs_id.into(),
            token_id: token_id.into(),
            addr: addr.into(),
        })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Membership {
    fs: Option<FilesystemInfo>,
    me: u32,
    members: BTreeMap<u32, Member>,
    tokens: Vec<TokenRec>,
    /// Set by `join` or `config`; otherwise the config file's offer applies.
    offer: Option<u64>,
    #[serde(skip)]
    path: PathBuf,
}

fn random_hex(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    getrandom::fill(&mut raw).expect("os randomness");
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

impl Membership {
    pub fn load(path: &Path) -> Self {
        let mut m: Membership = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        m.path = path.to_path_buf();
        m
    }

    fn save(&self) -> Result<(), String> {
        let tmp = self.path.with_extension("tmp");
        let text = serde_json::to_string_pretty(self).expect("serialises");
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, &self.path))
            .map_err(|e| format!("cannot save membership: {e}"))
    }

    pub fn filesystem(&self) -> Option<&FilesystemInfo> {
        self.fs.as_ref()
    }

    /// This node's member id, once it belongs to a filesystem.
    pub fn me(&self) -> Option<u32> {
        self.fs.as_ref().map(|_| self.me)
    }

    pub fn members(&self) -> Vec<Member> {
        self.members.values().cloned().collect()
    }

    pub fn my_offer(&self, default: u64) -> u64 {
        self.offer.unwrap_or(default)
    }

    /// Endpoints of the other members.
    pub fn peers(&self) -> Vec<NodeId> {
        self.members
            .values()
            .filter(|m| m.id != self.me)
            .map(|m| m.endpoint)
            .collect()
    }

    pub fn is_member_endpoint(&self, endpoint: NodeId) -> bool {
        self.fs.is_some()
            && self
                .members
                .values()
                .any(|m| m.id != self.me && m.endpoint == endpoint)
    }

    /// Start a filesystem with this node as its first member.
    pub fn create(
        &mut self,
        fs: FilesystemInfo,
        endpoint: NodeId,
        addr: String,
        default_offer: u64,
    ) -> Result<(), String> {
        self.fs = Some(fs);
        self.me = 0;
        let offer_bytes = self.my_offer(default_offer);
        self.members.insert(
            0,
            Member {
                id: 0,
                endpoint,
                addr,
                offer_bytes,
                rev: 1,
            },
        );
        self.save()
    }

    /// Mint a join token valid until `expires`; returns the token text.
    pub fn mint_token(&mut self, expires: Nanos) -> Result<String, String> {
        let (Some(fs), Some(me)) = (&self.fs, self.members.get(&self.me)) else {
            return Err("this node does not belong to a filesystem yet".into());
        };
        let id = random_hex(8);
        let text = format!("startfs1-{}.{id}@{}", fs.id, me.addr);
        self.tokens.push(TokenRec {
            id,
            expires,
            revoked: false,
        });
        self.save()?;
        Ok(text)
    }

    pub fn revoke_token(&mut self, text: &str) -> Result<(), String> {
        let token = Token::parse(text)?;
        let rec = self
            .tokens
            .iter_mut()
            .find(|t| t.id == token.token_id)
            .ok_or("this node did not issue that token")?;
        rec.revoked = true;
        self.save()
    }

    /// Admit a new member presenting a token.
    pub fn admit(
        &mut self,
        now: Nanos,
        fs_id: &str,
        token_id: &str,
        endpoint: NodeId,
        addr: String,
        offer_bytes: u64,
    ) -> Result<Admission, String> {
        let fs = self.fs.clone().ok_or("this node is not in a filesystem")?;
        let denied = || "token not recognised".to_string();
        if fs.id != fs_id {
            return Err(denied());
        }
        let rec = self
            .tokens
            .iter()
            .find(|t| t.id == token_id)
            .ok_or_else(denied)?;
        if rec.revoked {
            return Err("token has been revoked".into());
        }
        if now >= rec.expires {
            return Err("token has expired".into());
        }
        let id = self.members.keys().next_back().map_or(0, |m| m + 1);
        self.members.insert(
            id,
            Member {
                id,
                endpoint,
                addr,
                offer_bytes,
                rev: 1,
            },
        );
        self.save()?;
        Ok(Admission {
            fs,
            id,
            members: self.members(),
        })
    }

    /// Become a member, as told by the admitting member.
    pub fn install(&mut self, adm: Admission, offer: u64) -> Result<(), String> {
        self.fs = Some(adm.fs);
        self.me = adm.id;
        self.members = adm.members.into_iter().map(|m| (m.id, m)).collect();
        self.offer = Some(offer);
        self.save()
    }

    /// Fold in a peer's view of the member list; the higher `rev` wins.
    /// Returns whether anything changed.
    pub fn merge(&mut self, theirs: Vec<Member>) -> bool {
        let mut changed = false;
        for m in theirs {
            if m.id == self.me {
                continue; // nobody else speaks for us
            }
            if self.members.get(&m.id).is_none_or(|mine| mine.rev < m.rev) {
                self.members.insert(m.id, m);
                changed = true;
            }
        }
        if changed {
            let _ = self.save();
        }
        changed
    }

    /// Keep our own record's endpoint and offer current.
    pub fn refresh_self(&mut self, endpoint: NodeId, default_offer: u64) {
        let offer = self.my_offer(default_offer);
        let me = self.me;
        if let Some(m) = self.members.get_mut(&me)
            && (m.endpoint != endpoint || m.offer_bytes != offer)
        {
            m.endpoint = endpoint;
            m.offer_bytes = offer;
            m.rev += 1;
            let _ = self.save();
        }
    }

    pub fn set_offer(&mut self, bytes: u64) -> Result<(), String> {
        self.offer = Some(bytes);
        let me = self.me;
        if let Some(m) = self.members.get_mut(&me) {
            m.offer_bytes = bytes;
            m.rev += 1;
        }
        self.save()
    }
}
