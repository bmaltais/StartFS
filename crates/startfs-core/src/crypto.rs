//! Encryption at rest: the filesystem key, the recovery keys that unlock it, and the
//! ciphers for chunks and metadata (ADR 0010).
//!
//! A filesystem has one random **filesystem key**, made at `create`. It is never typed or
//! shown. Any number of **recovery keys** each *wrap* it: a recovery key is a random 256-bit
//! secret, shown once, and the wrap (the filesystem key sealed under it) is public state that
//! every member holds and gossips. A running member can therefore mint a new recovery key
//! without re-encrypting a byte, and a rebuilt or new machine turns any recovery key into the
//! filesystem key by fetching a wrap from a peer.

use aes::Aes256;
use aes::cipher::KeyInit as AesKeyInit;
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Write;
use std::path::Path;
use xts_mode::{Xts128, get_tweak_default};

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
/// Bytes an encrypted chunk grows by.
pub const SEAL_OVERHEAD: usize = NONCE_LEN + TAG_LEN;
/// The unit the metadata database is encrypted in (the smallest write a disk keeps whole).
pub const SECTOR: usize = 512;
/// Where a node keeps its filesystem key, inside its data directory.
pub const KEY_FILE: &str = "fs.key";
const RECOVERY_PREFIX: &str = "sfrk1";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("that is not a recovery key")]
    BadRecoveryKey,
    #[error("the recovery key does not open this filesystem")]
    WrongRecoveryKey,
    #[error("data could not be decrypted: wrong key or damaged")]
    Open,
}

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("os randomness");
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The key every chunk and the metadata database are encrypted under.
#[derive(Clone, PartialEq, Eq)]
pub struct FsKey([u8; 32]);

impl fmt::Debug for FsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FsKey(..)")
    }
}

impl FsKey {
    pub fn generate() -> Self {
        Self(random())
    }

    /// The cipher chunks are sealed with.
    pub fn chunk_cipher(&self) -> ChunkCipher {
        let sub = blake3::derive_key("startfs 1 chunk encryption", &self.0);
        ChunkCipher(XChaCha20Poly1305::new(&Key::from(sub)))
    }

    /// The cipher the metadata database is encrypted with.
    pub fn sector_cipher(&self) -> SectorCipher {
        let mut sub = [0u8; 64];
        blake3::Hasher::new_derive_key("startfs 1 metadata encryption")
            .update(&self.0)
            .finalize_xof()
            .fill(&mut sub);
        let a = Aes256::new_from_slice(&sub[..32]).expect("32-byte key");
        let b = Aes256::new_from_slice(&sub[32..]).expect("32-byte key");
        SectorCipher(Xts128::new(a, b))
    }

    /// Keep the key in `dir` for the daemon to find when it restarts on its own. The file is
    /// private to the user and written whole or not at all.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = dir.join(format!("{KEY_FILE}.tmp"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(hex(&self.0).as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, dir.join(KEY_FILE))?;
        std::fs::File::open(dir)?.sync_all()
    }

    /// The key kept in `dir`, if any.
    pub fn load(dir: &Path) -> std::io::Result<Option<Self>> {
        let text = match std::fs::read_to_string(dir.join(KEY_FILE)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "damaged key file");
        let bytes = unhex(text.trim()).ok_or_else(bad)?;
        Ok(Some(Self(bytes.try_into().map_err(|_| bad())?)))
    }
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.is_ascii() || text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// Seals and opens chunk bodies.
pub struct ChunkCipher(XChaCha20Poly1305);

impl ChunkCipher {
    /// `nonce || ciphertext || tag`; `aad` is authenticated, not hidden.
    pub fn seal(&self, aad: &[u8], plain: &[u8]) -> Vec<u8> {
        let nonce: [u8; NONCE_LEN] = random();
        let mut out = nonce.to_vec();
        let sealed = self
            .0
            .encrypt(&XNonce::from(nonce), Payload { msg: plain, aad })
            .expect("encryption of a buffer does not fail");
        out.extend_from_slice(&sealed);
        out
    }

    pub fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if sealed.len() < SEAL_OVERHEAD {
            return Err(CryptoError::Open);
        }
        let (nonce, body) = sealed.split_at(NONCE_LEN);
        self.0
            .decrypt(
                &XNonce::try_from(nonce).expect("nonce length"),
                Payload { msg: body, aad },
            )
            .map_err(|_| CryptoError::Open)
    }
}

/// Encrypts a file in [`SECTOR`]-byte units, each tweaked by its position (XTS), so a
/// sector can be rewritten in place. It hides content, not that a sector changed, and does
/// not detect tampering (the database's own checksums notice damage).
pub struct SectorCipher(Xts128<Aes256>);

impl SectorCipher {
    /// Encrypt `data`, which starts at the beginning of sector `first`.
    pub fn encrypt(&self, data: &mut [u8], first: u64) {
        self.0
            .encrypt_area(data, SECTOR, u128::from(first), get_tweak_default);
    }

    pub fn decrypt(&self, data: &mut [u8], first: u64) {
        self.0
            .decrypt_area(data, SECTOR, u128::from(first), get_tweak_default);
    }
}

/// A secret that unlocks the filesystem key, shown to the person and typed back by them.
#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryKey([u8; 32]);

impl fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryKey(..)")
    }
}

impl fmt::Display for RecoveryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let h = hex(&self.0);
        write!(f, "{RECOVERY_PREFIX}")?;
        for group in h.as_bytes().chunks(8) {
            write!(f, "-{}", std::str::from_utf8(group).expect("hex is ascii"))?;
        }
        Ok(())
    }
}

impl RecoveryKey {
    pub fn generate() -> Self {
        Self(random())
    }

    /// Read a key as [`Display`](fmt::Display) writes it; dashes, spaces and case are free.
    pub fn parse(text: &str) -> Result<Self, CryptoError> {
        let lower = text.trim().to_ascii_lowercase();
        let body = lower
            .strip_prefix(RECOVERY_PREFIX)
            .ok_or(CryptoError::BadRecoveryKey)?;
        let digits: String = body.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
        let bytes = unhex(&digits).ok_or(CryptoError::BadRecoveryKey)?;
        Ok(Self(
            bytes.try_into().map_err(|_| CryptoError::BadRecoveryKey)?,
        ))
    }

    fn wrapping_cipher(&self) -> XChaCha20Poly1305 {
        let sub = blake3::derive_key("startfs 1 recovery wrap", &self.0);
        XChaCha20Poly1305::new(&Key::from(sub))
    }

    /// What a node sends to show it holds this key, without sending the key.
    pub fn proof(&self) -> [u8; 32] {
        blake3::derive_key("startfs 1 recovery proof", &self.0)
    }

    /// The filesystem key sealed under this recovery key.
    pub fn wrap(&self, key: &FsKey) -> KeyWrap {
        let nonce: [u8; NONCE_LEN] = random();
        let mut wrapped = nonce.to_vec();
        wrapped.extend_from_slice(
            &self
                .wrapping_cipher()
                .encrypt(&XNonce::from(nonce), key.0.as_slice())
                .expect("encryption of a buffer does not fail"),
        );
        let check = *blake3::hash(&self.proof()).as_bytes();
        KeyWrap {
            id: hex(&check[..8]),
            check,
            wrapped,
        }
    }
}

/// A filesystem key sealed under one recovery key. It reveals nothing without that key, so
/// every member keeps and gossips it and hands it to anyone who asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyWrap {
    /// Names the recovery key without revealing it.
    pub id: String,
    /// Hash of the recovery key's [`proof`](RecoveryKey::proof): the holder is whoever can
    /// present the proof.
    pub check: [u8; 32],
    pub wrapped: Vec<u8>,
}

impl KeyWrap {
    /// Whether `proof` comes from the recovery key this wrap is under.
    pub fn accepts(&self, proof: &[u8; 32]) -> bool {
        blake3::hash(proof).as_bytes() == &self.check
    }

    pub fn unwrap(&self, recovery: &RecoveryKey) -> Result<FsKey, CryptoError> {
        if self.wrapped.len() < NONCE_LEN {
            return Err(CryptoError::WrongRecoveryKey);
        }
        let (nonce, body) = self.wrapped.split_at(NONCE_LEN);
        let bytes = recovery
            .wrapping_cipher()
            .decrypt(&XNonce::try_from(nonce).expect("nonce length"), body)
            .map_err(|_| CryptoError::WrongRecoveryKey)?;
        Ok(FsKey(
            bytes
                .try_into()
                .map_err(|_| CryptoError::WrongRecoveryKey)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recovery_key_round_trips_through_its_text() {
        let key = RecoveryKey::generate();
        let text = key.to_string();
        assert!(text.starts_with("sfrk1-"));
        assert_eq!(RecoveryKey::parse(&text.to_uppercase()).unwrap(), key);
        assert_eq!(
            RecoveryKey::parse("sfrk1-00"),
            Err(CryptoError::BadRecoveryKey)
        );
        assert_eq!(RecoveryKey::parse("nope"), Err(CryptoError::BadRecoveryKey));
    }

    #[test]
    fn a_wrap_opens_only_with_its_own_recovery_key() {
        let fs = FsKey::generate();
        let (a, b) = (RecoveryKey::generate(), RecoveryKey::generate());
        let wrap = a.wrap(&fs);
        assert!(wrap.unwrap(&a).unwrap() == fs);
        assert_eq!(wrap.unwrap(&b).unwrap_err(), CryptoError::WrongRecoveryKey);
        assert!(wrap.accepts(&a.proof()));
        assert!(!wrap.accepts(&b.proof()));
        assert!(!b.wrap(&fs).accepts(&a.proof()));
    }

    #[test]
    fn a_chunk_opens_with_its_key_and_its_header_only() {
        let cipher = FsKey::generate().chunk_cipher();
        let sealed = cipher.seal(b"head", b"secret");
        assert_eq!(cipher.open(b"head", &sealed).unwrap(), b"secret");
        assert!(cipher.open(b"other", &sealed).is_err());
        assert!(
            FsKey::generate()
                .chunk_cipher()
                .open(b"head", &sealed)
                .is_err()
        );
        assert!(!sealed.windows(6).any(|w| w == b"secret"));
    }

    #[test]
    fn sectors_round_trip_and_differ_by_position() {
        let cipher = FsKey::generate().sector_cipher();
        let plain = vec![7u8; SECTOR * 2];
        let mut a = plain.clone();
        cipher.encrypt(&mut a, 0);
        assert_ne!(a, plain);
        assert_ne!(a[..SECTOR], a[SECTOR..]);
        cipher.decrypt(&mut a, 0);
        assert_eq!(a, plain);
    }

    #[test]
    fn the_key_file_is_private_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("sfkey-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(FsKey::load(&dir).unwrap().is_none());
        let key = FsKey::generate();
        key.save(&dir).unwrap();
        assert!(FsKey::load(&dir).unwrap().unwrap() == key);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
