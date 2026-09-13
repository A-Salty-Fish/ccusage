use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// Environment variable that overrides the token discovered on disk.
pub(crate) const TOKEN_ENV: &str = "CCUSAGE_CURSOR_TOKEN";
/// Optional explicit cache file path, used by tests.
pub(crate) const CACHE_ENV: &str = "CCUSAGE_CURSOR_CACHE";

const TOKEN_KEY: &str = "cursorAuth/accessToken";
const REFRESH_TOKEN_KEY: &str = "cursorAuth/refreshToken";
const REFRESH_URL: &str = "https://api2.cursor.sh/oauth/token";
/// Public Cursor desktop OAuth client id (also used by cstats and similar tools).
const OAUTH_CLIENT_ID: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CliAuthFile {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Resolve the Cursor access token used to query the usage API.
///
/// The explicit `CCUSAGE_CURSOR_TOKEN` override wins so CI and headless setups
/// never need a local login. Otherwise the Agent CLI `auth.json` is preferred
/// over leftover desktop `state.vscdb` rows, which stay stale after the app is
/// uninstalled. The token is only ever sent in the request `Authorization`
/// header; it is never logged or printed.
pub(crate) fn access_token() -> Option<String> {
    if let Ok(token) = env::var(TOKEN_ENV) {
        let token = token.trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    if let Some(token) = cli_auth_file().and_then(|auth| nonempty(auth.access_token)) {
        return Some(token);
    }
    for db in state_db_paths() {
        if let Some(token) = read_item_from_db(&db, TOKEN_KEY) {
            return Some(token);
        }
    }
    None
}

pub(crate) fn refresh_token() -> Option<String> {
    if env::var(TOKEN_ENV).is_ok_and(|value| !value.trim().is_empty()) {
        return None;
    }
    if let Some(token) = cli_auth_file().and_then(|auth| nonempty(auth.refresh_token)) {
        return Some(token);
    }
    for db in state_db_paths() {
        if let Some(token) = read_item_from_db(&db, REFRESH_TOKEN_KEY) {
            return Some(token);
        }
    }
    None
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn cli_auth_file() -> Option<CliAuthFile> {
    for path in auth_json_paths() {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(auth) = serde_json::from_str::<CliAuthFile>(&text) {
            return Some(auth);
        }
    }
    None
}

/// Agent CLI credential file written by `FileCredentialManager`.
fn auth_json_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(appdata) = env::var_os("APPDATA").filter(|path| !path.is_empty()) {
        let appdata = PathBuf::from(appdata);
        paths.push(appdata.join("Cursor").join("auth.json"));
        paths.push(appdata.join("cursor").join("auth.json"));
    }
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
        paths.push(PathBuf::from(xdg).join("cursor").join("auth.json"));
    }
    if let Some(home) = crate::home::home_dir() {
        paths.push(home.join(".config").join("cursor").join("auth.json"));
        paths.push(
            home.join("Library")
                .join("Application Support")
                .join("Cursor")
                .join("auth.json"),
        );
    }
    paths.retain(|path| path.is_file());
    paths.sort();
    paths.dedup();
    paths
}

/// Prefer a still-valid access token; otherwise exchange the refresh token.
pub(crate) fn resolved_access_token() -> Option<String> {
    let access = access_token();
    if let Some(token) = access.as_ref()
        && !token_needs_refresh(token)
    {
        return Some(token.clone());
    }
    if let Some(refresh) = refresh_token()
        && let Some(refreshed) = exchange_refresh_token(&refresh)
    {
        return Some(refreshed);
    }
    access
}

fn token_needs_refresh(token: &str) -> bool {
    jwt_exp_unix(token).is_none_or(|exp| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        exp <= now + 300
    })
}

fn jwt_exp_unix(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = decode_base64url(payload)?;
    let value = serde_json::from_slice::<serde_json::Value>(&decoded).ok()?;
    value.get("exp")?.as_i64()
}

fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    let mut normalized = input.replace('-', "+").replace('_', "/");
    while normalized.len() % 4 != 0 {
        normalized.push('=');
    }
    let mut output = Vec::new();
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut buf = 0u32;
    let mut bits = 0u32;
    for byte in normalized.bytes() {
        if byte == b'=' {
            break;
        }
        let value = table.iter().position(|&candidate| candidate == byte)? as u32;
        buf = (buf << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(output)
}

fn exchange_refresh_token(refresh: &str) -> Option<String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut response = agent
        .post(REFRESH_URL)
        .header("Content-Type", "application/json")
        .send(
            serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": OAUTH_CLIENT_ID,
                "refresh_token": refresh,
            })
            .to_string(),
        )
        .ok()?;
    if response.status().as_u16() != 200 {
        return None;
    }
    let text = response.body_mut().read_to_string().ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

pub(crate) fn cache_path() -> Option<PathBuf> {
    if let Ok(path) = env::var(CACHE_ENV) {
        let path = path.trim();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    cache_dir().map(|dir| dir.join("ccusage").join("cursor").join("events-v1.json"))
}

fn cache_dir() -> Option<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::home::home_dir().map(|home| home.join(".cache")))
}

fn read_item_from_db(db: &Path, key: &str) -> Option<String> {
    let connection =
        sqlite::Connection::open_with_flags(db, sqlite::OpenFlags::new().with_read_only()).ok()?;
    let mut statement = connection
        .prepare("SELECT value FROM ItemTable WHERE key = ?")
        .ok()?;
    statement.bind((1, key)).ok()?;
    if let Ok(sqlite::State::Row) = statement.next() {
        let value = statement.read::<String, _>(0).ok()?;
        let token = value.trim().trim_matches('"');
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    None
}

/// Candidate `state.vscdb` locations across the supported desktop platforms.
fn state_db_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = crate::home::home_dir() {
        paths.push(home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"));
        paths.push(home.join(".config/Cursor/User/globalStorage/state.vscdb"));
        paths.push(home.join("AppData/Roaming/Cursor/User/globalStorage/state.vscdb"));
    }
    if let Some(appdata) = env::var_os("APPDATA")
        && !appdata.is_empty()
    {
        paths.push(PathBuf::from(appdata).join("Cursor/User/globalStorage/state.vscdb"));
    }
    paths.retain(|path| path.is_file());
    paths.sort();
    paths.dedup();
    paths
}

pub(crate) fn has_local_credentials() -> bool {
    access_token().is_some() || refresh_token().is_some()
}

pub(crate) fn has_cache_file() -> bool {
    cache_path().is_some_and(|path| path.is_file())
}

/// Exists so callers can report a useful error without printing the token.
pub(crate) fn missing_token_message() -> String {
    format!(
        "No Cursor access token found. Run `agent login` (cursor-agent CLI), or set {TOKEN_ENV}."
    )
}

pub(crate) fn cli_token_error() -> crate::CliError {
    crate::cli_error(missing_token_message())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    use ccusage_test_support::{EnvVarsGuard, fs_fixture};

    #[test]
    fn token_env_wins_over_missing_db() {
        let fixture = fs_fixture!({});
        let _guard = EnvVarsGuard::set_many([
            (TOKEN_ENV, Some(OsString::from("env-token"))),
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            (
                "USERPROFILE",
                Some(fixture.root().as_os_str().to_os_string()),
            ),
            (
                "APPDATA",
                Some(fixture.path("empty-appdata").into_os_string()),
            ),
        ]);
        assert_eq!(access_token().as_deref(), Some("env-token"));
    }

    #[test]
    fn reads_access_token_from_cli_auth_json() {
        let fixture = fs_fixture!({
            "Cursor/auth.json": r#"{"accessToken":"cli-token","refreshToken":"cli-refresh"}"#,
        });
        let _guard = EnvVarsGuard::set_many([
            (TOKEN_ENV, None),
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            (
                "USERPROFILE",
                Some(fixture.root().as_os_str().to_os_string()),
            ),
            ("APPDATA", Some(fixture.root().as_os_str().to_os_string())),
        ]);
        assert_eq!(access_token().as_deref(), Some("cli-token"));
        assert_eq!(refresh_token().as_deref(), Some("cli-refresh"));
    }

    #[test]
    fn cli_auth_json_wins_over_stale_state_vscdb() {
        let fixture = fs_fixture!({
            "Cursor/auth.json": r#"{"accessToken":"cli-token"}"#,
        });
        let db_path = fixture.path("Cursor/User/globalStorage/state.vscdb");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = sqlite::open(&db_path).unwrap();
        db.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        db.execute(
            "INSERT INTO ItemTable (key, value) VALUES ('cursorAuth/accessToken', '\"stale-db\"');",
        )
        .unwrap();
        drop(db);
        let _guard = EnvVarsGuard::set_many([
            (TOKEN_ENV, None),
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            (
                "USERPROFILE",
                Some(fixture.root().as_os_str().to_os_string()),
            ),
            ("APPDATA", Some(fixture.root().as_os_str().to_os_string())),
        ]);
        assert_eq!(access_token().as_deref(), Some("cli-token"));
    }

    #[test]
    fn reads_access_token_from_state_vscdb() {
        let fixture = fs_fixture!({});
        let db_path = fixture.path("AppData/Roaming/Cursor/User/globalStorage/state.vscdb");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = sqlite::open(&db_path).unwrap();
        db.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);")
            .unwrap();
        db.execute(
            "INSERT INTO ItemTable (key, value) VALUES ('cursorAuth/accessToken', '\"db-token\"');",
        )
        .unwrap();
        drop(db);

        let _guard = EnvVarsGuard::set_many([
            (TOKEN_ENV, None),
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            (
                "USERPROFILE",
                Some(fixture.root().as_os_str().to_os_string()),
            ),
            (
                "APPDATA",
                Some(fixture.path("AppData/Roaming").into_os_string()),
            ),
        ]);
        assert_eq!(access_token().as_deref(), Some("db-token"));
    }

    #[test]
    fn jwt_exp_reads_unix_seconds() {
        // {"exp":2000000000} in base64url, with dummy header/signature.
        let token = "eyJhbGciOiJub25lIn0.eyJleHAiOjIwMDAwMDAwMDB9.sig";
        assert_eq!(jwt_exp_unix(token), Some(2_000_000_000));
        assert!(!token_needs_refresh(token));
    }

    #[test]
    fn cache_env_overrides_xdg() {
        let fixture = fs_fixture!({});
        let cache = fixture.path("custom-cache.json");
        let _guard = EnvVarsGuard::set_many([
            (CACHE_ENV, Some(cache.clone().into_os_string())),
            (
                "XDG_CACHE_HOME",
                Some(fixture.path("xdg-cache").into_os_string()),
            ),
        ]);
        assert_eq!(cache_path().as_deref(), Some(cache.as_path()));
    }
}
