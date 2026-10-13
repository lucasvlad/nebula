//! Device identity: the long-term static X25519 keypair.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use nebula_core::DeviceId;

/// Noise protocol name for sessions (ADR 0004). Also used for key generation.
pub(crate) const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const KEY_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity file I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("noise key generation failed: {0}")]
    Noise(#[from] snow::Error),
    #[error("identity file {0} is corrupt")]
    Corrupt(PathBuf),
}

/// This device's long-term static X25519 keypair. Generated on first run,
/// never leaves the device.
///
/// Stored as 64 raw bytes (private then public). On Unix the file is created
/// with mode 0600. On Windows it is created with default permissions, which
/// for files under the user's profile inherit a per-user ACL.
///
/// On load the stored public key is not checked against the private key,
/// because snow does not expose public-key derivation; only the length is
/// validated.
pub struct Identity {
    private: [u8; KEY_LEN],
    public: [u8; KEY_LEN],
}

impl Identity {
    pub fn generate() -> Result<Self, IdentityError> {
        let keypair = snow::Builder::new(NOISE_PARAMS.parse()?).generate_keypair()?;
        let private = keypair.private.try_into().map_err(|_| snow::Error::Dh)?;
        let public = keypair.public.try_into().map_err(|_| snow::Error::Dh)?;
        Ok(Self { private, public })
    }

    /// Load the identity at `path`, or generate and persist a new one.
    pub fn load_or_create(path: &Path) -> Result<Self, IdentityError> {
        match fs::read(path) {
            Ok(bytes) => {
                let identity = Self::from_bytes(&bytes)
                    .ok_or_else(|| IdentityError::Corrupt(path.to_path_buf()))?;
                #[cfg(unix)]
                tighten_permissions(path)?;
                Ok(identity)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate()?;
                identity.write(path)?;
                Ok(identity)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn public_key(&self) -> &[u8; KEY_LEN] {
        &self.public
    }

    pub fn private_key(&self) -> &[u8; KEY_LEN] {
        &self.private
    }

    pub fn device_id(&self) -> DeviceId {
        DeviceId::from_public_key(&self.public)
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 2 * KEY_LEN {
            return None;
        }
        let (private, public) = bytes.split_at(KEY_LEN);
        Some(Self {
            private: private.try_into().ok()?,
            public: public.try_into().ok()?,
        })
    }

    fn write(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp = PathBuf::from(tmp_name);
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut file = create_private(&tmp)?;
        file.write_all(&self.private)?;
        file.write_all(&self.public)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({})", self.device_id().short())
    }
}

#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Like ssh, refuse to leave a key readable by group/other.
#[cfg(unix)]
fn tighten_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_is_distinct_and_device_id_matches() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        assert_ne!(a.public_key(), b.public_key());
        assert_ne!(a.private_key(), b.private_key());
        assert_eq!(a.device_id(), DeviceId::from_public_key(a.public_key()));
    }

    #[test]
    fn load_or_create_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        let a = Identity::load_or_create(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap().len(), 64);
        assert!(!dir.path().join("identity.key.tmp").exists());
        let b = Identity::load_or_create(&path).unwrap();
        assert_eq!(a.public_key(), b.public_key());
        assert_eq!(a.private_key(), b.private_key());
        assert_eq!(a.device_id(), b.device_id());
    }

    #[test]
    fn creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/identity.key");
        Identity::load_or_create(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn wrong_length_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        fs::write(&path, [0u8; 10]).unwrap();
        assert!(matches!(
            Identity::load_or_create(&path),
            Err(IdentityError::Corrupt(p)) if p == path
        ));
    }

    #[test]
    fn stale_tmp_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        fs::write(dir.path().join("identity.key.tmp"), [1u8; 3]).unwrap();
        Identity::load_or_create(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap().len(), 64);
    }

    #[cfg(unix)]
    #[test]
    fn unix_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.key");
        Identity::load_or_create(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        Identity::load_or_create(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn debug_hides_private_key() {
        let id = Identity::generate().unwrap();
        let hex: String = id
            .private_key()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let out = format!("{id:?}");
        assert!(!out.contains(&hex));
        assert!(out.contains(&id.device_id().short()));
    }
}
