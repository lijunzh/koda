//! OAuth sign-in for streamable HTTP MCP servers, as `codex mcp login`
//! signs in (`codex-rs/rmcp-client/src/perform_oauth_login.rs`).
//!
//! rmcp's `auth` feature does the protocol: it discovers the server's
//! authorization server (RFC 9728, RFC 8414), registers Koda as a client
//! where the server allows it (Dynamic Client Registration, or a Client ID
//! Metadata Document), runs the authorization-code flow with PKCE, and
//! refreshes the access token when it expires. Koda adds:
//!
//! - **The callback:** a one-shot listener on `127.0.0.1`, on a port the OS
//!   picks, for the browser's redirect.
//! - **Where tokens live:** one file per server,
//!   `~/.config/koda/mcp_oauth/<server>.json`, readable by the user only,
//!   as Codex falls back to `~/.codex/.credentials.json` without a keyring.
//!   Koda's API keys live in the same folder (`db/koda.db`).
//! - **Connecting:** a server with stored tokens and no bearer token is
//!   reached through rmcp's `AuthClient`, which adds the token to each
//!   request and refreshes it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rmcp::transport::auth::{
    AuthClient, AuthError, AuthorizationManager, AuthorizationRequest, CredentialStore, OAuthState,
    StoredCredentials,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How long the browser has to come back.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// The file that holds `server`'s tokens.
pub fn credentials_path(server: &str) -> Result<PathBuf> {
    let name: String = server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Ok(koda_config::config_dir()?
        .join("mcp_oauth")
        .join(format!("{name}.json")))
}

/// Whether Koda holds tokens for `server`.
pub fn has_credentials(server: &str) -> bool {
    credentials_path(server).is_ok_and(|path| path.is_file())
}

/// Forget `server`'s tokens. Returns whether there were any.
pub fn logout(server: &str) -> Result<bool> {
    let path = credentials_path(server)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("couldn't remove {}", path.display())),
    }
}

/// Tokens in a file only the user can read.
struct FileStore {
    path: PathBuf,
}

impl FileStore {
    fn new(server: &str) -> Result<Self> {
        Ok(Self {
            path: credentials_path(server)?,
        })
    }
}

fn store_error(e: impl std::fmt::Display) -> AuthError {
    AuthError::InternalError(format!("MCP credentials: {e}"))
}

#[async_trait::async_trait]
impl CredentialStore for FileStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(store_error),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(store_error(e)),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let text = serde_json::to_string_pretty(&credentials).map_err(store_error)?;
        write_private(&self.path, &text).map_err(store_error)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(store_error(e)),
        }
    }
}

/// Write `text` to `path` through a temporary file, user-only.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent folder"))?;
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
    }
    std::fs::rename(&tmp, path)
}

/// A sign-in under way: the page to open, and the wait for the browser.
pub struct Login {
    /// The authorization server's page, to open in a browser.
    pub authorization_url: String,
    done: tokio::task::JoinHandle<Result<()>>,
}

impl Login {
    /// Wait for the browser to come back and the tokens to be stored.
    pub async fn wait(self) -> Result<()> {
        self.done.await.map_err(|e| anyhow!("{e}"))?
    }
}

/// Start signing in to the MCP server `server` at `url`, asking for
/// `scopes`; with none, rmcp asks for what the server advertises. A private,
/// loopback or link-local URL is refused, as connecting to it is.
pub async fn login(server: &str, url: &str, scopes: &[String]) -> Result<Login> {
    if !koda_sandbox::url_guard::is_safe_url(url) {
        bail!(
            "{server}'s URL {url} is private, loopback or link-local, which Koda doesn't connect to"
        );
    }
    login_unchecked(server, url, scopes).await
}

/// [`login`] without the address check, for tests against a local server.
pub(crate) async fn login_unchecked(server: &str, url: &str, scopes: &[String]) -> Result<Login> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("couldn't listen for the sign-in's redirect")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let mut manager = AuthorizationManager::new(url)
        .await
        .with_context(|| format!("couldn't reach {url} to sign in"))?;
    manager.set_credential_store(FileStore::new(server)?);
    let mut state = OAuthState::Unauthorized(manager);
    state
        .start_authorization(
            AuthorizationRequest::new(&redirect_uri)
                .with_client_name("Koda")
                .with_scopes(scopes.iter().cloned()),
        )
        .await
        .map_err(|e| anyhow!("{server} can't start an OAuth sign-in: {e}"))?;
    let authorization_url = state
        .get_authorization_url()
        .await
        .map_err(|e| anyhow!("{e}"))?;

    let done = tokio::spawn(async move {
        let callback = tokio::time::timeout(LOGIN_TIMEOUT, callback(&listener, port))
            .await
            .map_err(|_| anyhow!("the sign-in wasn't finished within 5 minutes"))??;
        let OAuthState::Session(session) = &state else {
            bail!("the sign-in ended before the browser came back");
        };
        session
            .handle_callback_url(&callback)
            .await
            .map_err(|e| anyhow!("the sign-in failed: {e}"))?;
        state
            .complete_authorization()
            .await
            .map_err(|e| anyhow!("{e}"))?;
        Ok(())
    });
    Ok(Login {
        authorization_url,
        done,
    })
}

/// Wait for the browser's redirect to `/callback`, answer it with a page
/// that says what happened, and return the URL it came to.
async fn callback(listener: &TcpListener, port: u16) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let mut read = 0;
        while read < buf.len() {
            let n = stream.read(&mut buf[read..]).await?;
            if n == 0 {
                break;
            }
            read += n;
            if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&buf[..read]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or("");
        if !target.starts_with("/callback") {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let failed = target.contains("error=");
        let body = if failed {
            "<html><body><p>The sign-in failed. Koda says why in the terminal.</p></body></html>"
        } else {
            "<html><body><p>Signed in. You can close this tab and go back to Koda.</p></body></html>"
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        return Ok(format!("http://127.0.0.1:{port}{target}"));
    }
}

/// An HTTP client that signs each request with `server`'s stored token and
/// refreshes it, or `None` when Koda holds no tokens for it.
pub(crate) async fn authorized_client(
    server: &str,
    url: &str,
    http: reqwest::Client,
) -> Result<Option<AuthClient<reqwest::Client>>> {
    if !has_credentials(server) {
        return Ok(None);
    }
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    manager.set_credential_store(FileStore::new(server)?);
    let found = manager.initialize_from_store().await.map_err(|e| {
        anyhow!("{server}'s stored sign-in can't be used ({e}); run `koda mcp login {server}`")
    })?;
    Ok(found.then(|| AuthClient::new(http, manager)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_name_becomes_a_safe_file_name() {
        let path = credentials_path("../evil server").unwrap();
        assert_eq!(path.file_name().unwrap(), "___evil_server.json");
        assert!(path.parent().unwrap().ends_with("mcp_oauth"));
    }

    #[cfg(unix)]
    #[test]
    fn tokens_are_written_for_the_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp_oauth").join("s.json");
        write_private(&path, "{}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }
}
