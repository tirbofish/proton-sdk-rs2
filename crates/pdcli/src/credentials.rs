use std::path::{Path, PathBuf};
use std::sync::Arc;

use platform_dirs::AppDirs;
use proton_drive_sdk::cache::encrypted::EncryptedCacheRepository;
use proton_drive_sdk::cache::sqlite::SqliteCacheRepository;
use proton_sdk_rs2::{cache::CacheRepository, ser::StoredCredentials, session::ProtonAPISession};
use rand::Rng;

const APP_NAME: &str = "pdcli";
const CRED_FILE: &str = "cred.ron";
const CACHE_KEY_FILE: &str = "cache.key";
const KEYRING_SERVICE: &str = "pdcli";
const KEYRING_SESSION: &str = "session";
const KEYRING_CACHE_KEY: &str = "cache-master-key";

fn cred_dir() -> PathBuf {
    let dir = AppDirs::new(Some(APP_NAME), false)
        .expect("failed to resolve platform config directory")
        .config_dir;
    let _ = std::fs::create_dir_all(&dir);
    restrict_permissions(&dir, 0o700);
    dir
}

fn cred_path() -> PathBuf {
    cred_dir().join(CRED_FILE)
}

fn keyring_get(user: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, user)
        .ok()?
        .get_password()
        .ok()
}

fn keyring_set(user: &str, value: &str) -> bool {
    keyring::Entry::new(KEYRING_SERVICE, user)
        .and_then(|entry| entry.set_password(value))
        .is_ok()
}

fn keyring_delete(user: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, user) {
        let _ = entry.delete_credential();
    }
}

pub fn load() -> Option<StoredCredentials> {
    if let Some(data) = keyring_get(KEYRING_SESSION) {
        return parse_credentials(&data);
    }

    let path = cred_path();
    let data = std::fs::read_to_string(&path).ok()?;
    let cred = parse_credentials(&data)?;
    if keyring_set(KEYRING_SESSION, &data) {
        let _ = std::fs::remove_file(&path);
        tracing::info!("migrated credentials from file to keyring");
    } else {
        restrict_permissions(&path, 0o600);
    }
    Some(cred)
}

fn parse_credentials(data: &str) -> Option<StoredCredentials> {
    match ron::from_str(data) {
        Ok(cred) => Some(cred),
        Err(e) => {
            tracing::warn!(error = %e, "corrupt stored credentials");
            None
        }
    }
}

pub fn save(cred: &StoredCredentials) -> anyhow::Result<()> {
    let data = ron::ser::to_string(cred)?;
    if keyring_set(KEYRING_SESSION, &data) {
        let _ = std::fs::remove_file(cred_path());
        return Ok(());
    }

    let dir = cred_dir();
    let path = dir.join(CRED_FILE);
    write_private_file(&path, data.as_bytes())?;
    tracing::debug!(path = %path.display(), "saved credentials to locked-down file (keyring unavailable)");
    Ok(())
}

pub fn save_session_tokens_on_refresh(session: &ProtonAPISession) {
    let mut refreshed = session.token_credential.subscribe_tokens_refreshed();
    let session_id = session.session_id.raw().clone();
    let username = session.username.clone();
    let user_id = session.user_id.raw().clone();
    let scopes = session.scopes.clone();
    let is_waiting_for_second_factor_code = session.is_waiting_for_second_factor_code;
    let password_mode = session.password_mode;

    tokio::spawn(async move {
        while let Ok((access_token, refresh_token)) = refreshed.recv().await {
            let cred = StoredCredentials::new(
                session_id.clone(),
                username.clone(),
                user_id.clone(),
                access_token,
                refresh_token,
                scopes.clone(),
                is_waiting_for_second_factor_code,
                password_mode,
            );
            if let Err(e) = save(&cred) {
                tracing::warn!(error = %e, "failed to persist refreshed session tokens");
            }
        }
    });
}

pub fn remove() {
    keyring_delete(KEYRING_SESSION);
    let path = cred_path();
    if std::fs::remove_file(&path).is_ok() {
        tracing::debug!(path = %path.display(), "removed credentials");
    }
}

pub fn open_session_caches() -> anyhow::Result<(
    Arc<dyn CacheRepository>,
    Arc<dyn CacheRepository>,
)> {
    let dir = cred_dir();
    let cache_db_path = dir.join("cache.db");
    let secret_db_path = dir.join("secret.db");
    scrub_plaintext_passphrases(&cache_db_path);

    let entity: Arc<dyn CacheRepository> = Arc::new(SqliteCacheRepository::open_file(
        &cache_db_path,
        Some(10_000),
    )?);
    restrict_permissions(&cache_db_path, 0o600);

    let sqlite = SqliteCacheRepository::open_file(&secret_db_path, Some(5_000))?;
    restrict_permissions(&secret_db_path, 0o600);
    let secret: Arc<dyn CacheRepository> = Arc::new(EncryptedCacheRepository::new(
        Arc::new(sqlite),
        cache_master_key()?,
    ));
    Ok((entity, secret))
}

fn scrub_plaintext_passphrases(cache_db: &Path) {
    if !cache_db.exists() {
        return;
    }
    let Ok(conn) = rusqlite::Connection::open(cache_db) else {
        return;
    };
    let _ = conn.execute(
        "DELETE FROM Entries WHERE Key LIKE 'account:passphrase:%'",
        [],
    );
    let _ = conn.execute(
        "DELETE FROM Tags WHERE Key LIKE 'account:passphrase:%'",
        [],
    );
}

pub(crate) fn cache_master_key() -> anyhow::Result<Vec<u8>> {
    if let Some(encoded) = keyring_get(KEYRING_CACHE_KEY) {
        let _ = std::fs::remove_file(cred_dir().join(CACHE_KEY_FILE));
        return decode_key(&encoded);
    }

    let path = cred_dir().join(CACHE_KEY_FILE);
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            restrict_permissions(&path, 0o600);
            if keyring_set(KEYRING_CACHE_KEY, &encode_key(&bytes)) {
                let _ = std::fs::remove_file(&path);
            }
            return Ok(bytes);
        }
    }

    let mut key = vec![0u8; 32];
    rand::rng().fill_bytes(&mut key);
    let encoded = encode_key(&key);
    if !keyring_set(KEYRING_CACHE_KEY, &encoded) {
        write_private_file(&path, &key)?;
    }
    Ok(key)
}

fn encode_key(key: &[u8]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_key(encoded: &str) -> anyhow::Result<Vec<u8>> {
    if encoded.len() != 64 {
        anyhow::bail!("invalid cache master key");
    }
    (0..32)
        .map(|i| u8::from_str_radix(&encoded[i * 2..i * 2 + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!(e))
}

fn write_private_file(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    std::fs::write(path, data)?;
    restrict_permissions(path, 0o600);
    Ok(())
}

pub(crate) fn restrict_permissions(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}
