//! What can go wrong in the daemon, the membership and the control interface.
//!
//! The `Display` text is what the user sees: the CLI prints it, and control
//! replies carry it to the client.

use crate::control::Response;
use crate::members::MemberId;
use crate::ops::FsError;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a StartFS join token")]
    BadToken,
    #[error("this node does not belong to a filesystem yet")]
    NotInFilesystem,
    #[error("this node does not belong to a filesystem")]
    NotAMember,
    #[error("this node is not in a filesystem")]
    NotAdmitting,
    #[error("this node has no member ids left to give a new node")]
    OutOfMemberIds,
    #[error("this node already belongs to a filesystem")]
    AlreadyInFilesystem,
    #[error("need a name and at least 1 copy")]
    BadFilesystemSpec,
    #[error(
        "cannot lower the number of copies until every file has all its copies and every member is reachable (see `startfs status`)"
    )]
    NotHealthyToLowerCopies,
    #[error("a mark's name is 1 to 64 characters of letters, digits, '-', '_' and '.'")]
    BadMarkName,
    #[error("there is a mark called {0} already")]
    MarkExists(String),
    #[error("there is no mark called {0}")]
    NoSuchMark(String),
    #[error("there are too many marks: delete one first")]
    TooManyMarks,
    #[error("{0} is not a release tag (expected something like v1.10.0)")]
    BadRelease(String),
    #[error("cannot find the latest release: {0}")]
    NoRelease(String),
    #[error(
        "this node already holds files, which cannot be encrypted in place; create the filesystem on a fresh node, or with --no-encrypt"
    )]
    EncryptNeedsFresh,
    #[error("this filesystem is encrypted: joining needs its recovery key (--key)")]
    RecoveryKeyNeeded,
    #[error("the recovery key does not open this filesystem")]
    RecoveryKeyWrong,
    #[error("this filesystem is not encrypted")]
    NotEncrypted,
    #[error("this node does not hold the key to this encrypted filesystem")]
    NoKeyHere,
    #[error("storage-only members of an encrypted filesystem are not supported yet")]
    StorageOnlyEncrypted,
    #[error(transparent)]
    Crypto(#[from] crate::crypto::CryptoError),
    #[error("cannot keep the filesystem key: {0}")]
    SaveKey(std::io::Error),
    #[error("no such token")]
    NoSuchToken,
    #[error("token not recognised")]
    TokenNotRecognised,
    #[error("token has been revoked")]
    TokenRevoked,
    #[error("token has expired")]
    TokenExpired,
    #[error("this node cannot remove itself; use `leave`")]
    RemoveSelf,
    #[error("no member {0}")]
    NoMember(MemberId),
    #[error("member {0} is still online; it can leave with `startfs leave`")]
    MemberOnline(MemberId),
    #[error("this node holds files of its own; join needs a fresh node")]
    NotFresh,
    #[error("this node has the filesystem mounted; unmount it before making it storage-only")]
    MountedCannotBeStorageOnly,
    #[error("a storage-only member must offer space to store (its offer is 0)")]
    StorageOnlyWithoutOffer,
    #[error("the daemon has no network")]
    NoNetwork,
    #[error("cannot reach {0}")]
    Unreachable(String),
    #[error("no answer from {addr}")]
    NoAnswer { addr: String },
    #[error("no answer from {addr}: {why}")]
    NoAnswerBecause { addr: String, why: String },
    #[error("cannot save membership: {0}")]
    SaveMembership(std::io::Error),
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error(transparent)]
    Config(#[from] toml::de::Error),
    #[error("cannot reach daemon at {}: {source}", socket.display())]
    DaemonUnreachable {
        socket: PathBuf,
        source: std::io::Error,
    },
    #[error("bad response: {0}")]
    BadResponse(serde_json::Error),
    #[error("unexpected response: {0:?}")]
    UnexpectedResponse(Box<Response>),
    /// A refusal that came over the wire as text: a peer's join reply or the
    /// daemon's control reply. Those formats carry plain strings.
    #[error("{0}")]
    Remote(String),
}
