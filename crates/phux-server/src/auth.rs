//! Structured bearer-credential authentication for remote consumers.
//!
//! The on-disk store contains only SHA-256 verifiers, never bearer secrets.
//! Credentials carry stable identity and authorization metadata for the
//! authority boundary described by ADR-0092; scope enforcement is deliberately
//! owned by the follow-up authorization work, not this module.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Default persisted path for the remote-consumer credential store.
#[must_use]
pub fn default_token_store_path() -> PathBuf {
    crate::telemetry::state_dir().join("remote-tokens")
}

/// Length in bytes of a bearer secret minted from the OS CSPRNG.
pub const TOKEN_LEN: usize = 32;
const STORE_VERSION: u32 = 1;
const VERIFIER_PREFIX: &str = "sha256:";

/// The initial scope of an ordinary terminal pairing. Work-plane access is
/// intentionally absent: ADR-0092 says existing terminal pairing is not
/// implicitly work authorization.
pub const TERMINAL_CONTROL_SCOPE: &str = "terminal.control";

/// Errors from loading or changing credentials.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The credential file could not be read or written.
    #[error("credential store io: {0}")]
    Io(#[from] io::Error),
    /// The OS random source failed while minting a credential.
    #[error("os random source unavailable: {0}")]
    Random(#[from] getrandom::Error),
    /// The structured store could not be decoded or violates its invariants.
    #[error("malformed credential store: {0}")]
    Malformed(String),
    /// Anonymous token lines require an explicit one-time conversion.
    #[error("legacy token store requires explicit migration")]
    LegacyMigrationRequired,
    /// A requested credential does not exist.
    #[error("credential {0} not found")]
    CredentialNotFound(String),
}

#[derive(Clone, Serialize, Deserialize)]
struct CredentialFile {
    version: u32,
    credentials: Vec<CredentialRecord>,
}

impl Default for CredentialFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            credentials: Vec::new(),
        }
    }
}

/// One version of a credential. Rotation retains the prior generation for a
/// bounded overlap, so records are keyed by `(id, generation)` rather than id.
#[derive(Clone, Serialize, Deserialize)]
struct CredentialRecord {
    id: String,
    verifier: String,
    principal: String,
    scopes: Vec<String>,
    issued_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    generation: u64,
}

/// Identity and policy metadata captured when a connection is established.
///
/// This value is a snapshot. Revocation and expiry apply to the next
/// authentication attempt; an established session is not re-authorized and
/// keeps this attestation until its transport closes (ADR-0031).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedCredential {
    /// Stable credential identifier shared by its rotated generations.
    pub id: String,
    /// Authority principal represented by this credential.
    pub principal: String,
    /// Declared authorization scopes; enforcement belongs to the caller.
    pub scopes: Vec<String>,
    /// Time this generation was issued.
    pub issued_at: DateTime<Utc>,
    /// Optional time after which new authentication fails.
    pub expires_at: Option<DateTime<Utc>>,
    /// Monotonic generation within the credential identifier.
    pub generation: u64,
}

/// A newly minted bearer secret and its non-secret identity.
pub struct MintedCredential {
    /// Stable identifier of the credential.
    pub id: String,
    /// Newly minted generation number.
    pub generation: u64,
    secret: String,
}

impl MintedCredential {
    /// The bearer secret, exposed only for one-time delivery to the consumer.
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl std::fmt::Debug for MintedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedCredential")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

/// A parsed snapshot of the current structured credential store.
#[derive(Clone)]
pub struct TokenStore {
    file: CredentialFile,
}

impl std::fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenStore")
            .field("credentials", &self.file.credentials.len())
            .finish()
    }
}

impl TokenStore {
    /// Load a versioned store. Missing files are empty; legacy token lines are
    /// rejected until [`migrate_legacy_store`] is called explicitly.
    pub fn load(path: &Path) -> Result<Self, AuthError> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err.into()),
        };
        if raw.trim().is_empty() {
            return Ok(Self {
                file: CredentialFile::default(),
            });
        }
        if !raw.trim_start().starts_with('{') {
            return Err(AuthError::LegacyMigrationRequired);
        }
        let file: CredentialFile =
            serde_json::from_str(&raw).map_err(|error| AuthError::Malformed(error.to_string()))?;
        validate_file(&file)?;
        Ok(Self { file })
    }

    /// Number of credential generations in this snapshot.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.file.credentials.len()
    }

    /// Whether this snapshot has no credentials.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.file.credentials.is_empty()
    }

    /// Authenticate a bearer secret at the current wall-clock time.
    #[must_use]
    pub fn authenticate(&self, presented: &[u8]) -> Option<AuthenticatedCredential> {
        self.authenticate_at(presented, Utc::now())
    }

    fn authenticate_at(
        &self,
        presented: &[u8],
        now: DateTime<Utc>,
    ) -> Option<AuthenticatedCredential> {
        if presented.len() != TOKEN_LEN {
            return None;
        }
        let candidate = Sha256::digest(presented);
        let mut matched = None;
        for record in &self.file.credentials {
            let verifier = decode_verifier(&record.verifier).ok();
            let active = record.revoked_at.is_none()
                && record.expires_at.is_none_or(|expiry| now < expiry)
                && record.issued_at <= now;
            let is_match = verifier
                .as_ref()
                .is_some_and(|verifier| bool::from(verifier.ct_eq(candidate.as_slice())));
            if active && is_match {
                matched = Some(AuthenticatedCredential {
                    id: record.id.clone(),
                    principal: record.principal.clone(),
                    scopes: record.scopes.clone(),
                    issued_at: record.issued_at,
                    expires_at: record.expires_at,
                    generation: record.generation,
                });
            }
        }
        matched
    }

    /// Compatibility predicate for transport callers that need only admission.
    #[must_use]
    pub fn verify(&self, presented: &[u8]) -> bool {
        self.authenticate(presented).is_some()
    }
}

fn validate_file(file: &CredentialFile) -> Result<(), AuthError> {
    if file.version != STORE_VERSION {
        return Err(AuthError::Malformed(format!(
            "unsupported version {} (expected {STORE_VERSION})",
            file.version
        )));
    }
    let mut keys = std::collections::HashSet::new();
    for record in &file.credentials {
        if record.id.is_empty() || record.principal.is_empty() || record.generation == 0 {
            return Err(AuthError::Malformed(
                "credential id, principal, and generation must be present".to_owned(),
            ));
        }
        decode_verifier(&record.verifier)?;
        if !keys.insert((&record.id, record.generation)) {
            return Err(AuthError::Malformed(format!(
                "duplicate credential generation {}:{}",
                record.id, record.generation
            )));
        }
    }
    Ok(())
}

fn verifier(secret: &[u8]) -> String {
    format!("{VERIFIER_PREFIX}{}", hex::encode(Sha256::digest(secret)))
}

fn decode_verifier(encoded: &str) -> Result<[u8; 32], AuthError> {
    let hex = encoded
        .strip_prefix(VERIFIER_PREFIX)
        .ok_or_else(|| AuthError::Malformed("credential verifier must use sha256".to_owned()))?;
    let bytes = hex::decode(hex)
        .map_err(|_| AuthError::Malformed("credential verifier is not hex".to_owned()))?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| AuthError::Malformed("credential verifier has wrong length".to_owned()))
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
struct Stamp {
    mtime: Option<std::time::SystemTime>,
    len: u64,
    dev: u64,
    ino: u64,
}

impl Stamp {
    fn probe(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(path).ok()?;
        Some(Self {
            mtime: meta.modified().ok(),
            len: meta.len(),
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

struct Cached {
    stamp: Option<Stamp>,
    store: TokenStore,
    reloads: u64,
}

/// A last-known-good snapshot that re-reads after each atomic file generation.
pub struct ReloadingTokenStore {
    path: PathBuf,
    cached: std::sync::Mutex<Cached>,
}

#[allow(
    clippy::missing_fields_in_debug,
    reason = "the cached field contains credential verifiers and is deliberately redacted"
)]
impl std::fmt::Debug for ReloadingTokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReloadingTokenStore")
            .field("path", &self.path)
            .field("credentials", &self.len())
            .finish()
    }
}

impl ReloadingTokenStore {
    /// Wrap an already-loaded snapshot and its current file generation.
    #[must_use]
    pub fn new(path: PathBuf, initial: TokenStore) -> Self {
        let stamp = Stamp::probe(&path);
        Self {
            path,
            cached: std::sync::Mutex::new(Cached {
                stamp,
                store: initial,
                reloads: 0,
            }),
        }
    }

    /// Load the current snapshot and begin tracking its file generation.
    pub fn load(path: PathBuf) -> Result<Self, AuthError> {
        let store = TokenStore::load(&path)?;
        Ok(Self::new(path, store))
    }

    /// Path of the tracked credential store.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn with_current<T>(&self, f: impl FnOnce(&TokenStore) -> T) -> T {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stamp = Stamp::probe(&self.path);
        if stamp.is_none() || stamp != cached.stamp {
            match TokenStore::load(&self.path) {
                Ok(store) => {
                    cached.stamp = stamp;
                    cached.store = store;
                    cached.reloads = cached.reloads.saturating_add(1);
                }
                Err(error) => tracing::warn!(
                    path = %self.path.display(), %error,
                    "credential store unreadable; keeping last known-good generation"
                ),
            }
        }
        f(&cached.store)
    }

    /// Authenticate against the current readable generation.
    #[must_use]
    pub fn authenticate(&self, presented: &[u8]) -> Option<AuthenticatedCredential> {
        self.with_current(|store| store.authenticate(presented))
    }

    /// Whether a bearer secret authenticates against the current generation.
    #[must_use]
    pub fn verify(&self, presented: &[u8]) -> bool {
        self.authenticate(presented).is_some()
    }

    /// Number of credential generations in the current snapshot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.with_current(TokenStore::len)
    }

    /// Whether the current snapshot contains no credentials.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.with_current(TokenStore::is_empty)
    }

    #[cfg(test)]
    fn reloads(&self) -> u64 {
        self.cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reloads
    }
}

/// Mint a generation-one credential with terminal-only authority.
pub fn mint_token(path: &Path) -> Result<String, AuthError> {
    Ok(mint_credential(path, None, &[TERMINAL_CONTROL_SCOPE.to_owned()], None)?.secret)
}

/// Mint a structured credential. `principal = None` creates a stable principal
/// from the generated credential id.
pub fn mint_credential(
    path: &Path,
    principal: Option<&str>,
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
) -> Result<MintedCredential, AuthError> {
    let mut file = load_file_for_update(path)?;
    let (id, secret) = random_identity_and_secret()?;
    let principal = principal.map_or_else(|| format!("remote-consumer:{id}"), str::to_owned);
    file.credentials.push(CredentialRecord {
        id: id.clone(),
        verifier: verifier(&secret),
        principal,
        scopes: scopes.to_vec(),
        issued_at: Utc::now(),
        expires_at,
        revoked_at: None,
        generation: 1,
    });
    atomic_write(path, &file)?;
    Ok(MintedCredential {
        id,
        generation: 1,
        secret: hex::encode(secret),
    })
}

/// Rotate a credential with a bounded overlap.
///
/// The atomic replacement means interruption exposes either the old complete
/// store or the new complete store, never half a rotation.
pub fn rotate_credential(
    path: &Path,
    id: &str,
    overlap: Duration,
) -> Result<MintedCredential, AuthError> {
    rotate_credential_at(path, id, overlap, Utc::now())
}

fn rotate_credential_at(
    path: &Path,
    id: &str,
    overlap: Duration,
    now: DateTime<Utc>,
) -> Result<MintedCredential, AuthError> {
    let mut file = load_file_for_update(path)?;
    let latest = file
        .credentials
        .iter()
        .filter(|record| record.id == id)
        .max_by_key(|record| record.generation)
        .cloned()
        .ok_or_else(|| AuthError::CredentialNotFound(id.to_owned()))?;
    let overlap_until = now + overlap.max(Duration::zero());
    for record in file.credentials.iter_mut().filter(|record| record.id == id) {
        if record.revoked_at.is_none() {
            record.expires_at = Some(
                record
                    .expires_at
                    .map_or(overlap_until, |expiry| expiry.min(overlap_until)),
            );
        }
    }
    let mut secret = [0u8; TOKEN_LEN];
    getrandom::getrandom(&mut secret)?;
    let generation = latest.generation.saturating_add(1);
    file.credentials.push(CredentialRecord {
        id: id.to_owned(),
        verifier: verifier(&secret),
        principal: latest.principal,
        scopes: latest.scopes,
        issued_at: now,
        expires_at: None,
        revoked_at: None,
        generation,
    });
    atomic_write(path, &file)?;
    Ok(MintedCredential {
        id: id.to_owned(),
        generation,
        secret: hex::encode(secret),
    })
}

/// Revoke every generation of a credential for future connection attempts.
pub fn revoke_credential(path: &Path, id: &str) -> Result<(), AuthError> {
    let mut file = load_file_for_update(path)?;
    let now = Utc::now();
    let mut found = false;
    for record in file.credentials.iter_mut().filter(|record| record.id == id) {
        record.revoked_at = Some(now);
        found = true;
    }
    if !found {
        return Err(AuthError::CredentialNotFound(id.to_owned()));
    }
    atomic_write(path, &file)
}

/// Explicitly convert anonymous token lines to generation-one structured
/// records. The old bearer values are read once and replaced by verifiers.
pub fn migrate_legacy_store(path: &Path) -> Result<usize, AuthError> {
    let raw = fs::read_to_string(path)?;
    if raw.trim_start().starts_with('{') {
        return Err(AuthError::Malformed(
            "credential store is already structured".to_owned(),
        ));
    }
    let now = Utc::now();
    let mut file = CredentialFile::default();
    for line in raw.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let secret = decode_secret(line)?;
        let (id, _) = random_identity_and_secret()?;
        file.credentials.push(CredentialRecord {
            id: id.clone(),
            verifier: verifier(&secret),
            principal: format!("legacy-remote-consumer:{id}"),
            scopes: vec![TERMINAL_CONTROL_SCOPE.to_owned()],
            issued_at: now,
            expires_at: None,
            revoked_at: None,
            generation: 1,
        });
    }
    let count = file.credentials.len();
    atomic_write(path, &file)?;
    Ok(count)
}

fn load_file_for_update(path: &Path) -> Result<CredentialFile, AuthError> {
    Ok(TokenStore::load(path)?.file)
}

fn random_identity_and_secret() -> Result<(String, [u8; TOKEN_LEN]), AuthError> {
    let mut id = [0u8; 16];
    let mut secret = [0u8; TOKEN_LEN];
    getrandom::getrandom(&mut id)?;
    getrandom::getrandom(&mut secret)?;
    Ok((hex::encode(id), secret))
}

fn decode_secret(encoded: &str) -> Result<[u8; TOKEN_LEN], AuthError> {
    let bytes = hex::decode(encoded)
        .map_err(|_| AuthError::Malformed("legacy token is not hex".to_owned()))?;
    <[u8; TOKEN_LEN]>::try_from(bytes.as_slice())
        .map_err(|_| AuthError::Malformed("legacy token has wrong length".to_owned()))
}

fn atomic_write(path: &Path, file: &CredentialFile) -> Result<(), AuthError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut suffix = [0u8; 8];
    getrandom::getrandom(&mut suffix)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("credentials"),
        hex::encode(suffix)
    ));
    let result = (|| {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        serde_json::to_writer_pretty(&mut output, file)
            .map_err(|error| AuthError::Malformed(error.to_string()))?;
        output.write_all(b"\n")?;
        output.sync_all()?;
        fs::rename(&tmp, path)?;
        FileSync::sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

struct FileSync;

impl FileSync {
    fn sync_directory(path: &Path) -> io::Result<()> {
        fs::File::open(path)?.sync_all()
    }
}

#[cfg(test)]
pub(crate) fn write_test_credential(path: &Path, secret: &[u8; TOKEN_LEN]) {
    let file = CredentialFile {
        version: STORE_VERSION,
        credentials: vec![CredentialRecord {
            id: "test-credential".to_owned(),
            verifier: verifier(secret),
            principal: "test-principal".to_owned(),
            scopes: vec![TERMINAL_CONTROL_SCOPE.to_owned()],
            issued_at: Utc::now() - Duration::seconds(1),
            expires_at: None,
            revoked_at: None,
            generation: 1,
        }],
    };
    atomic_write(path, &file).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn secret(minted: &MintedCredential) -> Vec<u8> {
        hex::decode(minted.secret()).unwrap()
    }

    #[test]
    fn structured_mint_persists_only_a_redacted_verifier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let minted = mint_credential(
            &path,
            Some("device:cockpit"),
            &[TERMINAL_CONTROL_SCOPE.to_owned()],
            None,
        )
        .unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"version\": 1"));
        assert!(raw.contains("device:cockpit"));
        assert!(raw.contains("sha256:"));
        assert!(!raw.contains(minted.secret()));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let auth = TokenStore::load(&path)
            .unwrap()
            .authenticate(&secret(&minted))
            .unwrap();
        assert_eq!(auth.id, minted.id);
        assert_eq!(auth.principal, "device:cockpit");
        assert_eq!(auth.scopes, [TERMINAL_CONTROL_SCOPE]);
        assert_eq!(auth.generation, 1);
    }

    #[test]
    fn legacy_store_requires_and_survives_explicit_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let token = "ab".repeat(TOKEN_LEN);
        fs::write(&path, format!("# old\n{token}\n")).unwrap();
        assert!(matches!(
            TokenStore::load(&path),
            Err(AuthError::LegacyMigrationRequired)
        ));
        assert_eq!(migrate_legacy_store(&path).unwrap(), 1);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(&token));
        let store = TokenStore::load(&path).unwrap();
        assert!(store.verify(&hex::decode(token).unwrap()));
    }

    #[test]
    fn expiry_and_revocation_fail_closed() {
        let now = Utc::now();
        let token = [0x44; TOKEN_LEN];
        let base = CredentialRecord {
            id: "cred".to_owned(),
            verifier: verifier(&token),
            principal: "device:test".to_owned(),
            scopes: vec![TERMINAL_CONTROL_SCOPE.to_owned()],
            issued_at: now - Duration::minutes(1),
            expires_at: Some(now + Duration::seconds(1)),
            revoked_at: None,
            generation: 1,
        };
        let store = TokenStore {
            file: CredentialFile {
                version: 1,
                credentials: vec![base.clone()],
            },
        };
        assert!(store.authenticate_at(&token, now).is_some());
        assert!(
            store
                .authenticate_at(&token, now + Duration::seconds(1))
                .is_none()
        );
        let mut revoked = base;
        revoked.expires_at = None;
        revoked.revoked_at = Some(now);
        let store = TokenStore {
            file: CredentialFile {
                version: 1,
                credentials: vec![revoked],
            },
        };
        assert!(store.authenticate_at(&token, now).is_none());
    }

    #[test]
    fn rotation_has_bounded_ab_overlap_and_preserves_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let first = mint_credential(
            &path,
            Some("device:a"),
            &[TERMINAL_CONTROL_SCOPE.to_owned()],
            None,
        )
        .unwrap();
        let first_secret = secret(&first);
        let now = Utc::now() + Duration::seconds(1);
        let second = rotate_credential_at(&path, &first.id, Duration::minutes(5), now).unwrap();
        let second_secret = secret(&second);
        let store = TokenStore::load(&path).unwrap();
        assert_eq!(
            store
                .authenticate_at(&first_secret, now)
                .unwrap()
                .generation,
            1
        );
        let current = store.authenticate_at(&second_secret, now).unwrap();
        assert_eq!(current.id, first.id);
        assert_eq!(current.principal, "device:a");
        assert_eq!(current.generation, 2);
        assert!(
            store
                .authenticate_at(&first_secret, now + Duration::minutes(5))
                .is_none()
        );
        assert!(
            store
                .authenticate_at(&second_secret, now + Duration::minutes(5))
                .is_some()
        );
    }

    #[test]
    fn interrupted_rotation_temp_file_cannot_replace_last_good_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let first =
            mint_credential(&path, None, &[TERMINAL_CONTROL_SCOPE.to_owned()], None).unwrap();
        fs::write(
            dir.path().join(".credentials.interrupted.tmp"),
            b"{\"version\":1",
        )
        .unwrap();
        let store = TokenStore::load(&path).unwrap();
        assert!(store.verify(&secret(&first)));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn reloads_new_generation_and_keeps_last_good_on_malformed_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let first =
            mint_credential(&path, None, &[TERMINAL_CONTROL_SCOPE.to_owned()], None).unwrap();
        let store = ReloadingTokenStore::load(path.clone()).unwrap();
        let second =
            mint_credential(&path, None, &[TERMINAL_CONTROL_SCOPE.to_owned()], None).unwrap();
        assert!(store.verify(&secret(&second)));
        assert_eq!(store.reloads(), 1);
        fs::write(&path, "{broken").unwrap();
        assert!(store.verify(&secret(&first)));
        assert_eq!(store.reloads(), 1, "failed reads do not commit a stamp");
    }

    #[test]
    fn unchanged_store_is_statted_without_re_reading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let minted =
            mint_credential(&path, None, &[TERMINAL_CONTROL_SCOPE.to_owned()], None).unwrap();
        let bearer = secret(&minted);
        let store = ReloadingTokenStore::load(path).unwrap();
        for _ in 0..8 {
            assert!(store.verify(&bearer));
        }
        assert_eq!(store.reloads(), 0);
    }

    #[test]
    fn deleting_store_revokes_all_on_next_authentication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let minted =
            mint_credential(&path, None, &[TERMINAL_CONTROL_SCOPE.to_owned()], None).unwrap();
        let bearer = secret(&minted);
        let store = ReloadingTokenStore::load(path.clone()).unwrap();
        assert!(store.verify(&bearer));
        fs::remove_file(path).unwrap();
        assert!(!store.verify(&bearer));
        assert!(store.is_empty());
    }

    #[test]
    fn revocation_affects_new_authentication_not_established_attestation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let minted = mint_credential(
            &path,
            Some("device:a"),
            &[TERMINAL_CONTROL_SCOPE.to_owned()],
            None,
        )
        .unwrap();
        let bearer = secret(&minted);
        let live = ReloadingTokenStore::load(path.clone()).unwrap();
        let established = live.authenticate(&bearer).unwrap();
        revoke_credential(&path, &minted.id).unwrap();
        assert!(
            live.authenticate(&bearer).is_none(),
            "next handshake is revoked"
        );
        assert_eq!(
            established.principal, "device:a",
            "established session retains its captured attestation"
        );
    }

    #[test]
    fn debug_output_never_contains_bearer_or_verifier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let minted =
            mint_credential(&path, None, &[TERMINAL_CONTROL_SCOPE.to_owned()], None).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let verifier = raw
            .split("sha256:")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let output = format!(
            "{minted:?} {:?} {:?}",
            TokenStore::load(&path).unwrap(),
            ReloadingTokenStore::load(path).unwrap()
        );
        assert!(!output.contains(minted.secret()));
        assert!(!output.contains(verifier));
        assert!(output.contains("[REDACTED]"));
    }
}
