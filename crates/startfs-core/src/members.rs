//! Who belongs to the filesystem: the member list, join tokens and admission.
//!
//! A token reads `startfs1-<filesystem id>.<token id>@<host>:<port>`: the
//! address is the minting member's, which is the member that admits the
//! joiner. Tokens expire and can be revoked. Token state is gossiped like the
//! member list, so any member can admit with any token and a revocation made
//! anywhere holds everywhere. Member ids are assigned at admission and never
//! reused.
//!
//! Ids are unique without any coordination: each member owns a range of the id
//! space and, to admit a node, takes the node's id from its own range and hands
//! it a slice of that range for the nodes it admits in turn. Ranges never
//! overlap, so two members admitting at the same moment cannot assign the same id.

use crate::control::FilesystemInfo;
use crate::error::Error;
use crate::net::NodeId;
use crate::ops::Nanos;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A member's id, assigned at admission. It tags the member's log entries and
/// inode numbers; on the wire and on disk it is a bare number.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct MemberId(pub u32);

/// Ids stay below this so that `(id + 1) << 40` always fits an inode number.
const ID_LIMIT: u32 = (1 << 24) - 1;

/// A share of the id space that one member may hand out: `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IdRange {
    pub start: u32,
    pub end: u32,
}

impl IdRange {
    /// A new member's id and the range it may hand out in turn. The id comes
    /// off the bottom, so a lone admitter hands out 1, 2, 3 and so on; the new
    /// member's range is a slice off the top. `None` when the range is used up.
    fn carve(&mut self) -> Option<(MemberId, IdRange)> {
        let free = self.end.saturating_sub(self.start);
        if free == 0 {
            return None;
        }
        let id = MemberId(self.start);
        self.start += 1;
        let give = (free - 1) / 16;
        self.end -= give;
        Some((
            id,
            IdRange {
                start: self.end,
                end: self.end + give,
            },
        ))
    }
}

impl std::fmt::Display for MemberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Assigned at admission; tags the member's log entries and inode numbers.
    pub id: MemberId,
    /// Where the member is reached on the transport.
    pub endpoint: NodeId,
    /// The `host:port` the member advertises.
    pub addr: String,
    /// Space the member offers to the filesystem, in bytes.
    pub offer_bytes: u64,
    /// The share of new copies the member takes, in the units of the offer, when
    /// that should differ from what it offers; 0 means the offer.
    #[serde(default)]
    pub weight_bytes: u64,
    /// Where the member lives (home, cloud), as it declared; empty means it is
    /// its own site. Placement spreads a chunk's copies across sites.
    #[serde(default)]
    pub site: String,
    /// Bumped by the member each time it changes its own record; the highest wins.
    pub rev: u64,
    /// The member is handing its data to the others and about to go.
    #[serde(default)]
    pub leaving: bool,
    /// The member has gone (left, or was removed): kept so stale gossip cannot revive it.
    #[serde(default)]
    pub removed: bool,
}

impl Member {
    /// What placement weighs the member by: its weight, or its offer when it
    /// has none.
    pub fn share(&self) -> u64 {
        if self.weight_bytes > 0 {
            self.weight_bytes
        } else {
            self.offer_bytes
        }
    }
}

/// What an admitting member tells a new member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub fs: FilesystemInfo,
    pub id: MemberId,
    /// The ids the new member may hand out to others.
    pub ids: IdRange,
    pub members: Vec<Member>,
}

/// A new node's request to the member named in its token: admit me.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub fs_id: String,
    pub token_id: String,
    /// The joiner's advertised `host:port`.
    pub addr: String,
    pub offer_bytes: u64,
    /// The joiner's declared site; empty means none.
    #[serde(default)]
    pub site: String,
}

/// A join token as every member knows it. `expires` never changes and `revoked`
/// only ever turns on, so merging two copies is order-independent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRec {
    pub id: String,
    pub expires: Nanos,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub fs_id: String,
    pub token_id: String,
    pub addr: String,
}

impl Token {
    pub fn parse(text: &str) -> Result<Token, Error> {
        let bad = || Error::BadToken;
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
    me: MemberId,
    /// Ids this member may still hand out; never overlaps another member's.
    #[serde(default)]
    ids: IdRange,
    members: BTreeMap<MemberId, Member>,
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

    fn save(&self) -> Result<(), Error> {
        let tmp = self.path.with_extension("tmp");
        let text = serde_json::to_string_pretty(self).expect("serialises");
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, &self.path))
            .map_err(Error::SaveMembership)
    }

    /// Save where the caller cannot return an error: a failure is reported,
    /// never silent, and the in-memory state stays authoritative until the next save.
    fn save_or_report(&self) {
        if let Err(e) = self.save() {
            eprintln!("startfs: {e}");
        }
    }

    pub fn filesystem(&self) -> Option<&FilesystemInfo> {
        self.fs.as_ref()
    }

    /// This node's member id, once it belongs to a filesystem.
    pub fn me(&self) -> Option<MemberId> {
        self.fs.as_ref().map(|_| self.me)
    }

    /// The current members.
    pub fn members(&self) -> Vec<Member> {
        self.members
            .values()
            .filter(|m| !m.removed)
            .cloned()
            .collect()
    }

    /// Every record, including those of members that have gone: what gossip carries.
    pub fn records(&self) -> Vec<Member> {
        self.members.values().cloned().collect()
    }

    /// Whether this node is handing its data over before leaving.
    pub fn is_leaving(&self) -> bool {
        self.members.get(&self.me).is_some_and(|m| m.leaving)
    }

    pub fn my_offer(&self, default: u64) -> u64 {
        self.offer.unwrap_or(default)
    }

    /// Endpoints of the other members.
    pub fn peers(&self) -> Vec<NodeId> {
        self.members
            .values()
            .filter(|m| m.id != self.me && !m.removed)
            .map(|m| m.endpoint)
            .collect()
    }

    pub fn is_member_endpoint(&self, endpoint: NodeId) -> bool {
        self.fs.is_some()
            && self
                .members
                .values()
                .any(|m| m.id != self.me && !m.removed && m.endpoint == endpoint)
    }

    /// Start a filesystem with this node as its first member.
    pub fn create(
        &mut self,
        fs: FilesystemInfo,
        endpoint: NodeId,
        addr: String,
        default_offer: u64,
    ) -> Result<(), Error> {
        self.fs = Some(fs);
        self.me = MemberId(0);
        self.ids = IdRange {
            start: 1,
            end: ID_LIMIT,
        };
        let offer_bytes = self.my_offer(default_offer);
        self.members.insert(
            MemberId(0),
            Member {
                id: MemberId(0),
                endpoint,
                addr,
                offer_bytes,
                weight_bytes: 0,
                site: String::new(),
                rev: 1,
                leaving: false,
                removed: false,
            },
        );
        self.save()
    }

    /// Mint a join token valid until `expires`; returns the token text.
    pub fn mint_token(&mut self, expires: Nanos) -> Result<String, Error> {
        let (Some(fs), Some(me)) = (&self.fs, self.members.get(&self.me)) else {
            return Err(Error::NotInFilesystem);
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

    pub fn revoke_token(&mut self, text: &str) -> Result<(), Error> {
        let token = Token::parse(text)?;
        let rec = self
            .tokens
            .iter_mut()
            .find(|t| t.id == token.token_id)
            .ok_or(Error::NoSuchToken)?;
        rec.revoked = true;
        self.save()
    }

    /// Every token, for gossip.
    pub fn token_records(&self) -> Vec<TokenRec> {
        self.tokens.clone()
    }

    /// Fold in a peer's tokens: unknown ones are added, a revocation sticks.
    /// Returns whether anything changed.
    pub fn merge_tokens(&mut self, theirs: Vec<TokenRec>) -> bool {
        let mut changed = false;
        for t in theirs {
            match self.tokens.iter_mut().find(|m| m.id == t.id) {
                Some(mine) if t.revoked && !mine.revoked => {
                    mine.revoked = true;
                    changed = true;
                }
                Some(_) => {}
                None => {
                    self.tokens.push(t);
                    changed = true;
                }
            }
        }
        if changed {
            self.save_or_report();
        }
        changed
    }

    /// Admit a new member presenting a token.
    pub fn admit(
        &mut self,
        now: Nanos,
        endpoint: NodeId,
        request: JoinRequest,
    ) -> Result<Admission, Error> {
        let fs = self.fs.clone().ok_or(Error::NotAdmitting)?;
        let denied = || Error::TokenNotRecognised;
        if fs.id != request.fs_id {
            return Err(denied());
        }
        let rec = self
            .tokens
            .iter()
            .find(|t| t.id == request.token_id)
            .ok_or_else(denied)?;
        if rec.revoked {
            return Err(Error::TokenRevoked);
        }
        if now >= rec.expires {
            return Err(Error::TokenExpired);
        }
        // The same machine joining again (rebuilt) replaces its old record.
        for old in self.members.values_mut() {
            if old.endpoint == endpoint && !old.removed {
                old.removed = true;
                old.rev += 1;
            }
        }
        let mut ids = self.ids;
        let (id, handed) = ids.carve().ok_or(Error::OutOfMemberIds)?;
        self.members.insert(
            id,
            Member {
                id,
                endpoint,
                addr: request.addr,
                offer_bytes: request.offer_bytes,
                weight_bytes: 0,
                site: request.site,
                rev: 1,
                leaving: false,
                removed: false,
            },
        );
        self.ids = ids;
        self.save()?;
        Ok(Admission {
            fs,
            id,
            ids: handed,
            members: self.records(),
        })
    }

    /// Become a member, as told by the admitting member.
    pub fn install(&mut self, adm: Admission, offer: u64) -> Result<(), Error> {
        self.fs = Some(adm.fs);
        self.me = adm.id;
        self.ids = adm.ids;
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
                if m.removed {
                    // The others removed us: we no longer belong to the filesystem.
                    self.reset();
                    return true;
                }
                continue; // nobody else speaks for us
            }
            if self
                .members
                .get(&m.id)
                .is_none_or(|mine| (mine.rev, mine.removed) < (m.rev, m.removed))
            {
                self.members.insert(m.id, m);
                changed = true;
            }
        }
        if changed {
            self.save_or_report();
        }
        changed
    }

    /// Keep our own record's address, endpoint and offer current.
    pub fn refresh_self(&mut self, addr: &str, endpoint: NodeId, default_offer: u64) {
        let offer = self.my_offer(default_offer);
        let me = self.me;
        if let Some(m) = self.members.get_mut(&me)
            && (m.addr != addr || m.endpoint != endpoint || m.offer_bytes != offer)
        {
            m.addr = addr.to_string();
            m.endpoint = endpoint;
            m.offer_bytes = offer;
            m.rev += 1;
            self.save_or_report();
        }
    }

    /// Start handing this node's data to the others.
    pub fn begin_leave(&mut self) -> Result<(), Error> {
        let me = self.me;
        let Some(m) = self.members.get_mut(&me) else {
            return Err(Error::NotAMember);
        };
        if !m.leaving {
            m.leaving = true;
            m.rev += 1;
        }
        self.save()
    }

    /// Take a member that is gone for good out of the filesystem.
    pub fn remove(&mut self, id: MemberId) -> Result<(), Error> {
        if id == self.me {
            return Err(Error::RemoveSelf);
        }
        let m = self
            .members
            .get_mut(&id)
            .filter(|m| !m.removed)
            .ok_or(Error::NoMember(id))?;
        m.removed = true;
        m.rev += 1;
        self.save()
    }

    /// Our record, marked gone, for the others; then this node belongs to nothing.
    pub fn finish_leave(&mut self) -> Option<Vec<Member>> {
        let me = self.me;
        let mine = self.members.get_mut(&me)?;
        mine.removed = true;
        mine.rev += 1;
        let records = self.records();
        self.reset();
        Some(records)
    }

    fn reset(&mut self) {
        *self = Membership {
            path: std::mem::take(&mut self.path),
            ..Default::default()
        };
        self.save_or_report();
    }

    /// Set the number of copies and bump its revision so the others take it.
    pub fn set_copies(&mut self, copies: u32) -> Result<(), Error> {
        let fs = self.fs.as_mut().ok_or(Error::NotInFilesystem)?;
        if copies == 0 {
            return Err(Error::BadFilesystemSpec);
        }
        if fs.copies == copies {
            return Ok(());
        }
        fs.copies = copies;
        fs.copies_rev += 1;
        self.save()
    }

    /// The copies setting as gossiped: `(copies, revision)`.
    pub fn copies_setting(&self) -> Option<(u32, u64)> {
        self.fs.as_ref().map(|fs| (fs.copies, fs.copies_rev))
    }

    /// Fold in a peer's copies setting; the higher revision wins, and equal
    /// revisions (concurrent changes) go to the higher count so every member
    /// settles on the same value. Returns whether anything changed.
    pub fn merge_copies(&mut self, copies: u32, rev: u64) -> bool {
        let Some(fs) = self.fs.as_mut() else {
            return false;
        };
        if copies == 0 || (fs.copies_rev, fs.copies) >= (rev, copies) {
            return false;
        }
        fs.copies = copies;
        fs.copies_rev = rev;
        self.save_or_report();
        true
    }

    /// Change how much of the placement this node takes; 0 follows the offer.
    pub fn set_weight(&mut self, bytes: u64) -> Result<(), Error> {
        let me = self.me;
        if let Some(m) = self.members.get_mut(&me) {
            m.weight_bytes = bytes;
            m.rev += 1;
        }
        self.save()
    }

    /// Declare where this node lives; empty clears it.
    pub fn set_site(&mut self, site: &str) -> Result<(), Error> {
        let me = self.me;
        if let Some(m) = self.members.get_mut(&me)
            && m.site != site
        {
            m.site = site.to_string();
            m.rev += 1;
        }
        self.save()
    }

    pub fn set_offer(&mut self, bytes: u64) -> Result<(), Error> {
        self.offer = Some(bytes);
        let me = self.me;
        if let Some(m) = self.members.get_mut(&me) {
            m.offer_bytes = bytes;
            m.rev += 1;
        }
        self.save()
    }
}
