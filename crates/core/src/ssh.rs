//! SSH tunnels: reach a database through a bastion host.
//!
//! A tunnel listens on a random local port and forwards each connection through the SSH
//! session to the database, so the database drivers just connect to localhost.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use russh::{Error as SshError, client};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::config::{Connection, SshConfig};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// `SHA256:…` with any base64 padding removed, as OpenSSH prints it.
fn normalize_fingerprint(value: &str) -> String {
    value.trim().trim_end_matches('=').to_owned()
}

/// Accepts the server only if its key matches the fingerprint the user saved.
struct Verifier {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Verifier {
    type Error = SshError;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let actual =
            normalize_fingerprint(&key.public_key().fingerprint(HashAlg::Sha256).to_string());
        let accepted = self.expected.as_deref() == Some(actual.as_str());
        *self.seen.lock().expect("fingerprint lock") = Some(actual);
        Ok(accepted)
    }
}

fn client_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    })
}

/// Reads a server's key fingerprint so the user can confirm it before trusting it.
pub async fn fetch_fingerprint(host: &str, port: u16) -> Result<String> {
    let seen = Arc::new(Mutex::new(None));
    let verifier = Verifier {
        expected: None,
        seen: seen.clone(),
    };
    // With nothing expected the key is rejected, which ends the handshake once it's seen.
    let attempt = client::connect(client_config(), (host, port), verifier);
    let _ = timeout(CONNECT_TIMEOUT, attempt)
        .await
        .with_context(|| format!("timed out connecting to {host}:{port}"))?;
    seen.lock()
        .expect("fingerprint lock")
        .take()
        .ok_or_else(|| anyhow!("could not read the server key from {host}:{port}"))
}

/// A running tunnel. Dropping it stops the forwarding.
pub struct Tunnel {
    local_port: u16,
    session: Arc<client::Handle<Verifier>>,
    accept_task: JoinHandle<()>,
}

impl Tunnel {
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    pub fn is_alive(&self) -> bool {
        !self.session.is_closed()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), directories::BaseDirs::new()) {
        (Some(rest), Some(dirs)) => dirs.home_dir().join(rest),
        _ => PathBuf::from(path),
    }
}

async fn authenticate(
    session: &mut client::Handle<Verifier>,
    ssh: &SshConfig,
    secret: Option<String>,
) -> Result<()> {
    let result = if ssh.key_path.is_empty() {
        let password = secret.context("no SSH password is saved for this connection")?;
        session.authenticate_password(&ssh.user, password).await?
    } else {
        let path = expand_home(&ssh.key_path);
        let key = load_secret_key(&path, secret.as_deref())
            .with_context(|| format!("reading the SSH key {}", path.display()))?;
        let hash = session.best_supported_rsa_hash().await?.flatten();
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        session.authenticate_publickey(&ssh.user, key).await?
    };
    if !result.success() {
        bail!("the SSH server rejected these credentials");
    }
    Ok(())
}

/// Opens an SSH session and starts forwarding a local port to the connection's database.
/// `secret` is the SSH password, or the private key's passphrase.
pub async fn open(conn: &Connection, ssh: &SshConfig, secret: Option<String>) -> Result<Tunnel> {
    let seen = Arc::new(Mutex::new(None));
    let verifier = Verifier {
        expected: Some(normalize_fingerprint(&ssh.fingerprint)),
        seen: seen.clone(),
    };
    let attempt = client::connect(client_config(), (ssh.host.as_str(), ssh.port), verifier);
    let mut session = match timeout(CONNECT_TIMEOUT, attempt).await {
        Err(_) => bail!("timed out connecting to the SSH host {}", ssh.host),
        Ok(Ok(session)) => session,
        Ok(Err(SshError::UnknownKey)) => {
            let actual = seen.lock().expect("fingerprint lock").take();
            bail!(
                "the SSH server's key does not match the saved fingerprint (the server sent {}). \
                 If the server changed on purpose, update the fingerprint.",
                actual.as_deref().unwrap_or("no key")
            )
        }
        Ok(Err(e)) => return Err(e).context("connecting to the SSH host"),
    };
    authenticate(&mut session, ssh, secret).await?;

    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let local_port = listener.local_addr()?.port();
    let session = Arc::new(session);
    let forwarding = session.clone();
    let (host, port) = (conn.host.clone(), u32::from(conn.port));
    let accept_task = tokio::spawn(async move {
        while let Ok((mut socket, peer)) = listener.accept().await {
            let session = forwarding.clone();
            let host = host.clone();
            tokio::spawn(async move {
                let origin_port = u32::from(peer.port());
                let channel = session
                    .channel_open_direct_tcpip(host, port, "127.0.0.1", origin_port)
                    .await;
                if let Ok(channel) = channel {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                }
            });
        }
    });
    Ok(Tunnel {
        local_port,
        session,
        accept_task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_compare_with_or_without_padding() {
        assert_eq!(normalize_fingerprint(" SHA256:abc= \n"), "SHA256:abc");
        assert_eq!(normalize_fingerprint("SHA256:abc"), "SHA256:abc");
    }

    #[test]
    fn home_is_expanded_only_at_the_start() {
        assert!(!expand_home("~/.ssh/id_ed25519").starts_with("~"));
        assert_eq!(expand_home("/a/~/b"), PathBuf::from("/a/~/b"));
    }
}
