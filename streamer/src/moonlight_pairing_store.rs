//! Durable, permission-restricted storage for Moonlight host identity and paired clients.
//! Records are written atomically beneath a caller-selected local state directory.
//! This is local access control, not encrypted-at-rest key storage.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::moonlight_pairing::PairedClient;

const SERVER_FILE: &str = "server.identity";
const CLIENT_DIR: &str = "clients";
const SERVER_MAGIC: &str = "APHELION-MOONLIGHT-SERVER 1";
const CLIENT_MAGIC: &str = "APHELION-MOONLIGHT-CLIENT 1";
const MAX_CERTIFICATE_BYTES: usize = 16 * 1024;
const MAX_PRIVATE_KEY_BYTES: usize = 16 * 1024;
const MAX_IDENTITY_BYTES: usize = 128;
const MAX_RECORD_BYTES: usize = 70 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum PairingStoreError {
    Io(io::Error),
    InvalidRecord(&'static str),
    InsecurePermissions(&'static str),
    PathIsNotDirectory,
}
impl std::fmt::Display for PairingStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "Moonlight pairing store I/O failed: {error}"),
            Self::InvalidRecord(reason) => write!(f, "invalid Moonlight pairing store record: {reason}"),
            Self::InsecurePermissions(path) => write!(f, "Moonlight pairing store path has unsafe permissions: {path}"),
            Self::PathIsNotDirectory => f.write_str("Moonlight pairing store root is not a directory"),
        }
    }
}
impl std::error::Error for PairingStoreError {}
impl From<io::Error> for PairingStoreError {
    fn from(error: io::Error) -> Self { Self::Io(error) }
}

/// Server certificate plus zeroizing private-key bytes loaded from the local store.
pub struct StoredServerIdentity {
    pub certificate_der: Vec<u8>,
    private_key_pkcs8_der: Zeroizing<Vec<u8>>,
}
impl StoredServerIdentity {
    pub fn private_key_pkcs8_der(&self) -> &[u8] { self.private_key_pkcs8_der.as_slice() }
}

/// Single-process owner for Aphelion's local Moonlight pairing records.
pub struct MoonlightPairingStore {
    root: PathBuf,
}
impl MoonlightPairingStore {
    /// Create or open a private state directory. Existing symlink roots are rejected.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, PairingStoreError> {
        let root = root.into();
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(PairingStoreError::InsecurePermissions("root symlink")),
            Ok(metadata) if !metadata.is_dir() => return Err(PairingStoreError::PathIsNotDirectory),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(&root)?,
            Err(error) => return Err(error.into()),
        }
        set_private_directory(&root)?;
        let clients = root.join(CLIENT_DIR);
        match fs::symlink_metadata(&clients) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(PairingStoreError::InsecurePermissions("client directory symlink")),
            Ok(metadata) if !metadata.is_dir() => return Err(PairingStoreError::PathIsNotDirectory),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&clients)?,
            Err(error) => return Err(error.into()),
        }
        set_private_directory(&clients)?;
        Ok(Self { root })
    }

    /// Replace the server's certificate/private-key pair as one atomic record.
    pub fn save_server_identity(&self, certificate_der: &[u8], private_key_pkcs8_der: &[u8]) -> Result<(), PairingStoreError> {
        if certificate_der.is_empty() || certificate_der.len() > MAX_CERTIFICATE_BYTES {
            return Err(PairingStoreError::InvalidRecord("server certificate size"));
        }
        if private_key_pkcs8_der.is_empty() || private_key_pkcs8_der.len() > MAX_PRIVATE_KEY_BYTES {
            return Err(PairingStoreError::InvalidRecord("server private key size"));
        }
        let record = Zeroizing::new(format!("{SERVER_MAGIC}\n{}\n{}\n", hex(certificate_der), hex(private_key_pkcs8_der)));
        atomic_write(&self.root.join(SERVER_FILE), record.as_bytes())
    }

    pub fn load_server_identity(&self) -> Result<Option<StoredServerIdentity>, PairingStoreError> {
        let path = self.root.join(SERVER_FILE);
        let Some(bytes) = read_private_file(&path, MAX_RECORD_BYTES)? else { return Ok(None); };
        let bytes = Zeroizing::new(bytes);
        let record = std::str::from_utf8(&bytes).map_err(|_| PairingStoreError::InvalidRecord("server record is not UTF-8"))?;
        let mut lines = record.lines();
        if lines.next() != Some(SERVER_MAGIC) { return Err(PairingStoreError::InvalidRecord("server record version")); }
        let certificate_der = decode_hex(lines.next().ok_or(PairingStoreError::InvalidRecord("missing server certificate"))?, MAX_CERTIFICATE_BYTES)?;
        let private_key = Zeroizing::new(decode_hex(lines.next().ok_or(PairingStoreError::InvalidRecord("missing server private key"))?, MAX_PRIVATE_KEY_BYTES)?);
        if lines.next().is_some() || certificate_der.is_empty() || private_key.is_empty() {
            return Err(PairingStoreError::InvalidRecord("server record fields"));
        }
        Ok(Some(StoredServerIdentity { certificate_der, private_key_pkcs8_der: private_key }))
    }

    /// Persist only a certificate returned after the pairing transaction verified proof.
    pub fn authorize_client(&self, client: &PairedClient) -> Result<(), PairingStoreError> {
        validate_identity(&client.client_id)?;
        validate_identity(&client.client_name)?;
        if client.certificate_der.is_empty() || client.certificate_der.len() > MAX_CERTIFICATE_BYTES {
            return Err(PairingStoreError::InvalidRecord("client certificate size"));
        }
        let id = hex(&Sha256::digest(client.client_id.as_bytes()));
        let record = format!("{CLIENT_MAGIC}\n{}\n{}\n{}\n",
            hex(client.client_id.as_bytes()), hex(client.client_name.as_bytes()), hex(&client.certificate_der));
        atomic_write(&self.root.join(CLIENT_DIR).join(format!("{id}.client")), record.as_bytes())
    }

    pub fn load_clients(&self) -> Result<Vec<PairedClient>, PairingStoreError> {
        let mut clients = Vec::new();
        for entry in fs::read_dir(self.root.join(CLIENT_DIR))? {
            let entry = entry?;
            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("client") { continue; }
            let path = entry.path();
            let Some(bytes) = read_private_file(&path, MAX_RECORD_BYTES)? else { continue; };
            let record = std::str::from_utf8(&bytes).map_err(|_| PairingStoreError::InvalidRecord("client record is not UTF-8"))?;
            let mut lines = record.lines();
            if lines.next() != Some(CLIENT_MAGIC) { return Err(PairingStoreError::InvalidRecord("client record version")); }
            let client_id = decode_string(lines.next().ok_or(PairingStoreError::InvalidRecord("missing client id"))?, MAX_IDENTITY_BYTES)?;
            let client_name = decode_string(lines.next().ok_or(PairingStoreError::InvalidRecord("missing client name"))?, MAX_IDENTITY_BYTES)?;
            let certificate_der = decode_hex(lines.next().ok_or(PairingStoreError::InvalidRecord("missing client certificate"))?, MAX_CERTIFICATE_BYTES)?;
            if lines.next().is_some() || certificate_der.is_empty() { return Err(PairingStoreError::InvalidRecord("client record fields")); }
            validate_identity(&client_id)?;
            validate_identity(&client_name)?;
            let expected = format!("{}.client", hex(&Sha256::digest(client_id.as_bytes())));
            if path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
                return Err(PairingStoreError::InvalidRecord("client identity does not match filename"));
            }
            clients.push(PairedClient { client_id, client_name, certificate_der });
        }
        clients.sort_by(|left, right| left.client_id.cmp(&right.client_id));
        Ok(clients)
    }

    pub fn remove_client(&self, client_id: &str) -> Result<bool, PairingStoreError> {
        validate_identity(client_id)?;
        let path = self.root.join(CLIENT_DIR).join(format!("{}.client", hex(&Sha256::digest(client_id.as_bytes()))));
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(PairingStoreError::InsecurePermissions("client record is not a regular file")),
            Ok(_) => { fs::remove_file(path)?; File::open(self.root.join(CLIENT_DIR))?.sync_all()?; Ok(true) }
        }
    }
}

fn validate_identity(value: &str) -> Result<(), PairingStoreError> {
    if value.is_empty() || value.len() > MAX_IDENTITY_BYTES || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err(PairingStoreError::InvalidRecord("invalid client identity"));
    }
    Ok(())
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes { encoded.push(DIGITS[(byte >> 4) as usize] as char); encoded.push(DIGITS[(byte & 0x0f) as usize] as char); }
    encoded
}
fn decode_string(value: &str, maximum: usize) -> Result<String, PairingStoreError> {
    let bytes = decode_hex(value, maximum)?;
    String::from_utf8(bytes).map_err(|_| PairingStoreError::InvalidRecord("identity is not UTF-8"))
}
fn decode_hex(value: &str, maximum: usize) -> Result<Vec<u8>, PairingStoreError> {
    if value.len() % 2 != 0 || value.len() / 2 > maximum || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PairingStoreError::InvalidRecord("invalid hexadecimal field"));
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        bytes.push((nibble(pair[0]).ok_or(PairingStoreError::InvalidRecord("invalid hexadecimal field"))? << 4)
            | nibble(pair[1]).ok_or(PairingStoreError::InvalidRecord("invalid hexadecimal field"))?);
    }
    Ok(bytes)
}
fn nibble(byte: u8) -> Option<u8> {
    match byte { b'0'..=b'9' => Some(byte - b'0'), b'a'..=b'f' => Some(byte - b'a' + 10), b'A'..=b'F' => Some(byte - b'A' + 10), _ => None }
}

fn read_private_file(path: &Path, maximum: usize) -> Result<Option<Vec<u8>>, PairingStoreError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PairingStoreError::InsecurePermissions("record is not a regular file"));
    }
    if metadata.len() as usize > maximum { return Err(PairingStoreError::InvalidRecord("record exceeds size limit")); }
    check_private_file_permissions(&metadata)?;
    let mut file = File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((maximum + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > maximum { return Err(PairingStoreError::InvalidRecord("record exceeds size limit")); }
    Ok(Some(bytes))
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), PairingStoreError> {
    let parent = path.parent().ok_or(PairingStoreError::InvalidRecord("record has no parent directory"))?;
    for _ in 0..8 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".aphelion-{}-{sequence}.tmp", std::process::id()));
        match open_private_new(&temporary) {
            Ok(mut file) => {
                let result = (|| -> io::Result<()> { file.write_all(bytes)?; file.sync_all()?; fs::rename(&temporary, path)?; File::open(parent)?.sync_all()?; Ok(()) })();
                if result.is_err() { let _ = fs::remove_file(&temporary); }
                result?;
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(PairingStoreError::Io(io::Error::new(io::ErrorKind::AlreadyExists, "could not allocate unique pairing store temporary file")))
}
fn open_private_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_create_private_mode(&mut options);
    options.open(path)
}
#[cfg(unix)]
fn set_create_private_mode(options: &mut OpenOptions) { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
#[cfg(not(unix))]
fn set_create_private_mode(_: &mut OpenOptions) {}
#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), PairingStoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_private_directory(_: &Path) -> Result<(), PairingStoreError> { Ok(()) }
#[cfg(unix)]
fn check_private_file_permissions(metadata: &fs::Metadata) -> Result<(), PairingStoreError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 { return Err(PairingStoreError::InsecurePermissions("record permissions")); }
    Ok(())
}
#[cfg(not(unix))]
fn check_private_file_permissions(_: &fs::Metadata) -> Result<(), PairingStoreError> { Ok(()) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            Self(std::env::temp_dir().join(format!("aphelion-pairing-store-{}-{suffix}", std::process::id())))
        }
    }
    impl Drop for TestDir { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    #[test]
    fn atomically_persists_server_identity_and_verified_client_records() {
        let dir = TestDir::new();
        let store = MoonlightPairingStore::open(&dir.0).unwrap();
        store.save_server_identity(&[1, 2, 3], &[4, 5, 6]).unwrap();
        let server = store.load_server_identity().unwrap().unwrap();
        assert_eq!(server.certificate_der, [1, 2, 3]);
        assert_eq!(server.private_key_pkcs8_der(), [4, 5, 6]);

        let phone = PairedClient { client_id: "phone-uuid".into(), client_name: "Phone".into(), certificate_der: vec![0x30, 0x01, 0x00] };
        let tablet = PairedClient { client_id: "tablet-uuid".into(), client_name: "Tablet".into(), certificate_der: vec![0x30, 0x02, 0x00] };
        store.authorize_client(&tablet).unwrap();
        store.authorize_client(&phone).unwrap();
        let loaded = store.load_clients().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].client_id, "phone-uuid");
        assert_eq!(loaded[0].client_name, "Phone");
        assert_eq!(loaded[0].certificate_der, vec![0x30, 0x01, 0x00]);
        assert_eq!(loaded[1].client_id, "tablet-uuid");
        assert_eq!(loaded[1].client_name, "Tablet");
        assert_eq!(loaded[1].certificate_der, vec![0x30, 0x02, 0x00]);
        assert!(store.remove_client("phone-uuid").unwrap());
        assert!(!store.remove_client("phone-uuid").unwrap());
        assert_eq!(store.load_clients().unwrap().len(), 1);
    }

    #[test]
    fn rejects_bad_values_and_corrupt_records() {
        let dir = TestDir::new();
        let store = MoonlightPairingStore::open(&dir.0).unwrap();
        assert!(matches!(store.save_server_identity(&[], &[1]), Err(PairingStoreError::InvalidRecord(_))));
        let invalid = PairedClient { client_id: "../escape".into(), client_name: "Phone".into(), certificate_der: vec![1] };
        assert!(matches!(store.authorize_client(&invalid), Err(PairingStoreError::InvalidRecord(_))));
        fs::write(dir.0.join(SERVER_FILE), b"corrupt").unwrap();
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.0.join(SERVER_FILE), fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(matches!(store.load_server_identity(), Err(PairingStoreError::InvalidRecord(_))));
    }

    #[cfg(unix)]
    #[test]
    fn creates_private_directory_and_record_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new();
        let store = MoonlightPairingStore::open(&dir.0).unwrap();
        store.save_server_identity(&[1], &[2]).unwrap();
        assert_eq!(fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(dir.0.join(SERVER_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
