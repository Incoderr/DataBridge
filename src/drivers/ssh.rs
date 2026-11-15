//! SSH tunnels: a connection with `DbConnection::ssh` set is served by a
//! [`TunneledSession`] — an SSH client (russh, ring crypto) forwards a
//! loopback port to the database host as seen from the SSH server
//! (`direct-tcpip`, like `ssh -L`), and the real driver connects to
//! `127.0.0.1:<that port>` without knowing about SSH.
//!
//! - The tunnel opens on the first session call (the wizard's Test
//!   Connection), probing the forward once so "the bastion cannot reach
//!   the database" surfaces as an SSH error, not a driver timeout.
//! - Every pooled DB connection is its own SSH channel over one SSH
//!   session; a session the server dropped is re-established on the next
//!   new DB connection. Keepalives every 30 s.
//! - Auth: password, private key file (+ passphrase), or agent (the
//!   OpenSSH agent pipe, then Pageant on Windows; `SSH_AUTH_SOCK`
//!   elsewhere).
//! - Host keys: checked against `~/.ssh/known_hosts` (shared with
//!   OpenSSH). An unknown host is trusted and recorded on first use
//!   (`StrictHostKeyChecking=accept-new`); a changed key is refused.
//! - The tunnel lives as long as the session (dropped with it).
//!
//! Not supported: jump hosts / `~/.ssh/config`, keyboard-interactive auth,
//! seed lists and `+srv` URLs (one forwarded port serves one host). TLS
//! that verifies the server name (`sslmode=verify-full`, Mongo `tls=true`)
//! sees `127.0.0.1` and fails unless the certificate covers it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, known_hosts};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::OnceCell;

use crate::core::error::DbError;
use crate::core::model::{
    CellChange, DbConnection, Page, Ping, QueryPage, Relation, RowQuery, SchemaMeta, SshAuth,
    SshConfig, TableRef, parse_db_url, rewrite_url_host,
};
use crate::core::session::{CancelToken, DbSession};

/// TCP connect + key exchange + authentication.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const KEEPALIVE: Duration = Duration::from_secs(30);
/// The loopback address the tunnel listens on (and the driver dials).
const LOCAL_HOST: &str = "127.0.0.1";

/// Builds the real driver session for a (rewritten) connection — the
/// registry's own factory, so drivers stay unaware of tunnels.
pub type DriverFactory = fn(&DbConnection) -> Result<Arc<dyn DbSession>, DbError>;

// ---------------- the session wrapper ----------------

pub struct TunneledSession {
    conn: DbConnection,
    ssh: SshConfig,
    /// The database as dialled from the SSH server.
    remote: (String, u16),
    make: DriverFactory,
    live: OnceCell<Live>,
}

struct Live {
    /// Declared first: dropped (pool closed) before the tunnel.
    session: Arc<dyn DbSession>,
    _tunnel: Tunnel,
}

impl TunneledSession {
    /// Validates what can be checked offline (a tunnelable URL); the SSH
    /// connection itself opens on the first call.
    pub fn new(conn: &DbConnection, ssh: SshConfig, make: DriverFactory) -> Result<Self, DbError> {
        if ssh.host.trim().is_empty() {
            return Err(DbError::new("SSH tunnel: the SSH host is empty"));
        }
        if ssh.user.trim().is_empty() {
            return Err(DbError::new("SSH tunnel: the SSH user is empty"));
        }
        let remote = match &conn.url {
            Some(url) => {
                // Probe the rewrite now: seed lists / +srv cannot tunnel.
                if rewrite_url_host(url, LOCAL_HOST, 1).is_none() {
                    return Err(DbError::new(
                        "SSH tunnel needs a URL with a single host (no seed lists or +srv)",
                    ));
                }
                let parts = parse_db_url(url)
                    .ok_or_else(|| DbError::new("SSH tunnel: cannot read the URL's host"))?;
                let port = if parts.port > 0 {
                    parts.port
                } else {
                    conn.db_type.default_port()
                };
                (parts.host, port)
            }
            None => (conn.host.clone(), conn.port),
        };
        let host = remote.0.trim_start_matches('[').trim_end_matches(']');
        let host = if host.is_empty() { "localhost" } else { host };
        Ok(Self {
            conn: conn.clone(),
            ssh,
            remote: (host.to_string(), remote.1),
            make,
            live: OnceCell::new(),
        })
    }

    /// The driver session behind the tunnel, opening both on first use (a
    /// failed attempt is retried on the next call).
    async fn inner(&self) -> Result<&Arc<dyn DbSession>, DbError> {
        let live = self
            .live
            .get_or_try_init(|| async {
                let tunnel =
                    Tunnel::open(&self.ssh, &self.remote.0, self.remote.1, known_hosts_file())
                        .await?;
                let mut local = self.conn.clone();
                local.ssh = None;
                local.host = LOCAL_HOST.into();
                local.port = tunnel.local_port;
                if let Some(url) = &self.conn.url {
                    local.url = rewrite_url_host(url, LOCAL_HOST, tunnel.local_port);
                }
                let session = (self.make)(&local)?;
                Ok::<_, DbError>(Live {
                    session,
                    _tunnel: tunnel,
                })
            })
            .await?;
        Ok(&live.session)
    }
}

#[async_trait]
impl DbSession for TunneledSession {
    async fn ping(&self) -> Result<Ping, DbError> {
        self.inner().await?.ping().await
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaMeta>, DbError> {
        self.inner().await?.list_schemas().await
    }

    async fn list_relations(&self, schema: &str) -> Result<Vec<Relation>, DbError> {
        self.inner().await?.list_relations(schema).await
    }

    async fn fetch_rows(
        &self,
        table: TableRef,
        query: RowQuery,
        offset: u64,
        limit: u64,
    ) -> Result<Page, DbError> {
        self.inner()
            .await?
            .fetch_rows(table, query, offset, limit)
            .await
    }

    async fn execute(
        &self,
        sql: &str,
        limit: u64,
        cancel: &CancelToken,
    ) -> Result<QueryPage, DbError> {
        self.inner().await?.execute(sql, limit, cancel).await
    }

    async fn update_row(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
        changes: Vec<CellChange>,
    ) -> Result<u64, DbError> {
        self.inner().await?.update_row(table, pk, changes).await
    }

    async fn insert_row(&self, table: TableRef, values: Vec<CellChange>) -> Result<u64, DbError> {
        self.inner().await?.insert_row(table, values).await
    }

    async fn delete_row(&self, table: TableRef, pk: Vec<CellChange>) -> Result<u64, DbError> {
        self.inner().await?.delete_row(table, pk).await
    }

    async fn save_document(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
        text: String,
    ) -> Result<u64, DbError> {
        self.inner().await?.save_document(table, pk, text).await
    }

    async fn drop_table(&self, table: TableRef) -> Result<(), DbError> {
        self.inner().await?.drop_table(table).await
    }
}

// ---------------- the tunnel ----------------

/// A loopback listener forwarding every accepted socket through its own
/// SSH channel. Dropping it stops accepting; open forwards end with their
/// sockets (and the SSH session with the last of them).
struct Tunnel {
    local_port: u16,
    accept: tokio::task::AbortHandle,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl Tunnel {
    async fn open(
        cfg: &SshConfig,
        host: &str,
        port: u16,
        known_hosts: PathBuf,
    ) -> Result<Self, DbError> {
        let link = Arc::new(SshLink {
            cfg: cfg.clone(),
            known_hosts,
            handle: tokio::sync::Mutex::new(None),
        });
        // Connect + authenticate now, and probe the forward once, so the
        // wizard reports SSH problems as such.
        let handle = link.handle().await?;
        let probe = handle
            .channel_open_direct_tcpip(host, port as u32, LOCAL_HOST, 0)
            .await
            .map_err(|e| {
                DbError::with_source(
                    format!("SSH server {} cannot reach {host}:{port}: {e}", cfg.host),
                    e,
                )
            })?;
        drop(probe);
        let listener = TcpListener::bind((LOCAL_HOST, 0))
            .await
            .map_err(|e| DbError::with_source("SSH tunnel: cannot open a local port", e))?;
        let local_port = listener
            .local_addr()
            .map_err(|e| DbError::with_source("SSH tunnel: cannot open a local port", e))?
            .port();
        let accept =
            tokio::spawn(accept_loop(listener, link, host.to_string(), port)).abort_handle();
        Ok(Self { local_port, accept })
    }
}

async fn accept_loop(listener: TcpListener, link: Arc<SshLink>, host: String, port: u16) {
    loop {
        let (mut socket, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                eprintln!("DataBridge: SSH tunnel accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let link = link.clone();
        let host = host.clone();
        tokio::spawn(async move {
            // A failure here reaches the driver as a closed connection;
            // the reason goes to the log.
            let handle = match link.handle().await {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("DataBridge: SSH tunnel: {e}");
                    return;
                }
            };
            let channel = match handle
                .channel_open_direct_tcpip(
                    host.as_str(),
                    port as u32,
                    LOCAL_HOST,
                    peer.port() as u32,
                )
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("DataBridge: SSH tunnel to {host}:{port} refused: {e}");
                    return;
                }
            };
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
        });
    }
}

/// One SSH session, re-established when the server dropped it.
struct SshLink {
    cfg: SshConfig,
    known_hosts: PathBuf,
    handle: tokio::sync::Mutex<Option<Arc<Handle<Client>>>>,
}

impl SshLink {
    async fn handle(&self) -> Result<Arc<Handle<Client>>, DbError> {
        let mut slot = self.handle.lock().await;
        if let Some(h) = slot.as_ref()
            && !h.is_closed()
        {
            return Ok(h.clone());
        }
        let h = Arc::new(
            tokio::time::timeout(CONNECT_TIMEOUT, connect(&self.cfg, &self.known_hosts))
                .await
                .map_err(|_| {
                    DbError::new(format!(
                        "SSH connection to {}:{} timed out",
                        self.cfg.host, self.cfg.port
                    ))
                })??,
        );
        *slot = Some(h.clone());
        Ok(h)
    }
}

async fn connect(
    cfg: &SshConfig,
    known_hosts: &std::path::Path,
) -> Result<Handle<Client>, DbError> {
    let config = Arc::new(client::Config {
        keepalive_interval: Some(KEEPALIVE),
        keepalive_max: 3,
        ..Default::default()
    });
    let handler = Client {
        host: cfg.host.clone(),
        port: cfg.port,
        known_hosts: known_hosts.to_path_buf(),
    };
    let mut handle = client::connect(config, (cfg.host.as_str(), cfg.port), handler)
        .await
        .map_err(|e| e.into_db(cfg))?;
    let ok = match cfg.auth {
        SshAuth::Password => handle
            .authenticate_password(cfg.user.as_str(), cfg.secret.as_str())
            .await
            .map_err(ssh_error)?
            .success(),
        SshAuth::Key => {
            let path = expand_home(&cfg.key_path);
            let pass = (!cfg.secret.is_empty()).then_some(cfg.secret.as_str());
            let key = russh::keys::load_secret_key(&path, pass).map_err(|e| key_error(e, &path))?;
            let hash = if key.algorithm().is_rsa() {
                handle
                    .best_supported_rsa_hash()
                    .await
                    .ok()
                    .flatten()
                    .flatten()
            } else {
                None
            };
            handle
                .authenticate_publickey(
                    cfg.user.as_str(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
                .map_err(ssh_error)?
                .success()
        }
        SshAuth::Agent => agent_auth(&mut handle, &cfg.user).await?,
    };
    if !ok {
        return Err(DbError::new(format!(
            "SSH authentication failed for {}",
            cfg.label()
        )));
    }
    Ok(handle)
}

/// Try every identity the agent holds; `Ok(false)` when none was accepted.
async fn agent_auth(handle: &mut Handle<Client>, user: &str) -> Result<bool, DbError> {
    #[cfg(windows)]
    {
        // The Windows OpenSSH agent service first, then Pageant.
        if let Ok(agent) = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
            if try_agent(handle, user, agent).await? {
                return Ok(true);
            }
        }
        match AgentClient::connect_pageant().await {
            Ok(agent) => try_agent(handle, user, agent).await,
            Err(_) => Err(DbError::new(
                "SSH agent not found: start the OpenSSH Authentication Agent service or Pageant",
            )),
        }
    }
    #[cfg(unix)]
    {
        let agent = AgentClient::connect_env()
            .await
            .map_err(|e| DbError::new(format!("SSH agent not available: {e}")))?;
        try_agent(handle, user, agent).await
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (handle, user);
        Err(DbError::new(
            "SSH agents are not supported on this platform",
        ))
    }
}

async fn try_agent<S>(
    handle: &mut Handle<Client>,
    user: &str,
    mut agent: AgentClient<S>,
) -> Result<bool, DbError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| DbError::new(format!("SSH agent: {e}")))?;
    for identity in identities {
        let AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        let hash = if key.algorithm().is_rsa() {
            handle
                .best_supported_rsa_hash()
                .await
                .ok()
                .flatten()
                .flatten()
        } else {
            None
        };
        if let Ok(res) = handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            && res.success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------- host keys ----------------

struct Client {
    host: String,
    port: u16,
    known_hosts: PathBuf,
}

#[derive(Debug)]
enum ClientError {
    Ssh(russh::Error),
    /// The recorded key differs — possible man-in-the-middle.
    HostKeyChanged {
        line: usize,
    },
}

impl From<russh::Error> for ClientError {
    fn from(e: russh::Error) -> Self {
        Self::Ssh(e)
    }
}

impl ClientError {
    fn into_db(self, cfg: &SshConfig) -> DbError {
        match self {
            Self::Ssh(e) => DbError::with_source(
                format!("SSH connection to {}:{} failed: {e}", cfg.host, cfg.port),
                e,
            ),
            Self::HostKeyChanged { line } => DbError::new(format!(
                "SSH host key of {} has changed (known_hosts line {line}) — possible \
                 man-in-the-middle attack. If the server was reinstalled, remove that line \
                 from ~/.ssh/known_hosts.",
                cfg.host
            )),
        }
    }
}

impl client::Handler for Client {
    type Error = ClientError;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match server_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        match known_hosts::check_known_hosts_path(&self.host, self.port, &key, &self.known_hosts) {
            Ok(true) => Ok(true),
            Err(russh::keys::Error::KeyChanged { line }) => {
                Err(ClientError::HostKeyChanged { line })
            }
            // Unknown host (or no known_hosts yet): trust on first use.
            _ => {
                if let Err(e) = known_hosts::learn_known_hosts_path(
                    &self.host,
                    self.port,
                    &key,
                    &self.known_hosts,
                ) {
                    eprintln!(
                        "DataBridge: cannot record the SSH host key of {}: {e}",
                        self.host
                    );
                }
                Ok(true)
            }
        }
    }
}

// ---------------- helpers ----------------

fn ssh_error(e: russh::Error) -> DbError {
    DbError::with_source(format!("SSH: {e}"), e)
}

fn key_error(e: russh::keys::Error, path: &std::path::Path) -> DbError {
    let shown = path.display();
    match e {
        russh::keys::Error::KeyIsEncrypted => DbError::new(format!(
            "SSH key {shown} is encrypted — enter its passphrase"
        )),
        russh::keys::Error::IO(io) if io.kind() == std::io::ErrorKind::NotFound => {
            DbError::new(format!("SSH key {shown} not found"))
        }
        e => DbError::with_source(format!("cannot read SSH key {shown}: {e}"), e),
    }
}

/// `~/.ssh/known_hosts` — shared with OpenSSH.
fn known_hosts_file() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_default()
        .join(".ssh")
        .join("known_hosts")
}

/// `~/…` → the home dir (both separators); anything else unchanged.
fn expand_home(path: &str) -> PathBuf {
    let path = path.trim();
    let rest = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix("~\\"))
        .or_else(|| (path == "~").then_some(""));
    match (rest, std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{ConnectionStatus, DbType};

    fn conn(url: Option<&str>) -> DbConnection {
        DbConnection {
            id: "t".into(),
            name: "t".into(),
            db_type: DbType::PostgreSql,
            host: "db.internal".into(),
            port: 6543,
            database: "app".into(),
            user: "u".into(),
            password: String::new(),
            url: url.map(Into::into),
            status: ConnectionStatus::Disconnected,
            latency_ms: 0,
            ssh: None,
        }
    }

    fn ssh() -> SshConfig {
        SshConfig {
            host: "bastion".into(),
            port: 22,
            user: "deploy".into(),
            auth: SshAuth::Agent,
            key_path: String::new(),
            secret: String::new(),
        }
    }

    fn no_driver(_: &DbConnection) -> Result<Arc<dyn DbSession>, DbError> {
        Err(DbError::new("unused"))
    }

    #[test]
    fn remote_target_comes_from_fields_or_url() {
        let s = TunneledSession::new(&conn(None), ssh(), no_driver).unwrap();
        assert_eq!(s.remote, ("db.internal".into(), 6543));
        let s = TunneledSession::new(&conn(Some("postgres://a:b@[::1]/app")), ssh(), no_driver)
            .unwrap();
        assert_eq!(s.remote, ("::1".into(), 5432));
    }

    #[test]
    fn seed_lists_cannot_tunnel() {
        let c = conn(Some("mongodb://h1:27017,h2:27017/db"));
        assert!(TunneledSession::new(&c, ssh(), no_driver).is_err());
        let c = conn(Some("mongodb+srv://cluster0.example.net/db"));
        assert!(TunneledSession::new(&c, ssh(), no_driver).is_err());
    }

    #[test]
    fn urls_are_pointed_at_the_tunnel() {
        assert_eq!(
            rewrite_url_host(
                "postgres://a:p@db:5432/app?sslmode=require",
                "127.0.0.1",
                40001
            )
            .as_deref(),
            Some("postgres://a:p@127.0.0.1:40001/app?sslmode=require")
        );
        assert_eq!(
            rewrite_url_host("mysql://root@db", "127.0.0.1", 40002).as_deref(),
            Some("mysql://root@127.0.0.1:40002")
        );
        assert_eq!(
            rewrite_url_host("mongodb://u:p%40x@[::1]:27017/?tls=false", "127.0.0.1", 1).as_deref(),
            Some("mongodb://u:p%40x@127.0.0.1:1/?tls=false")
        );
    }

    #[test]
    fn home_expands() {
        let home = std::env::home_dir().unwrap();
        assert_eq!(
            expand_home("~/.ssh/id_ed25519"),
            home.join(".ssh/id_ed25519")
        );
        assert_eq!(expand_home("C:/keys/k"), PathBuf::from("C:/keys/k"));
    }

    // ---------------- end-to-end: an in-process SSH server ----------------

    /// Throwaway keys generated for these tests only.
    const HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACA0HDqq4q0QKD9ydttlbgOqKr5y+ZnoCEeqRIYoQLjkrAAAAJiQE2Z3kBNm
dwAAAAtzc2gtZWQyNTUxOQAAACA0HDqq4q0QKD9ydttlbgOqKr5y+ZnoCEeqRIYoQLjkrA
AAAEBLbiN91ujZLH3NjER9mAFNkE3WsFcKSfCRtohBfiQhEDQcOqrirRAoP3J222VuA6oq
vnL5megIR6pEhihAuOSsAAAAFGRhdGFicmlkZ2UtdGVzdC1ob3N0AQ==
-----END OPENSSH PRIVATE KEY-----
";
    /// Encrypted with the passphrase `s3cret`.
    const USER_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABDjjFD6Ac
aYmxdl6uy51PFRAAAAIAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAILI1Mf9lDrJOEnpQ
AsceO9pfkDQ3p97S3no0Q0+ePxULAAAAoArmsMMQYn9eyg2kxdM4cVpzxMfzuBG5bwQRzj
FM1hq7hXlOfsGjZkOYUs5zQtzFZv9yHTQrCYwqcmCyza8kBrprMID4+RURHLqZd6RY6znL
jaR3KkS450XEwvbNpP8Dcbt5/RoOBlcCRu3n47lj0Zsr6liVal1NAuIutuHptLsVSvB9Zk
qG9Dgg+VCAtcn7TajDDN3KNEcksBugLAdTmN0=
-----END OPENSSH PRIVATE KEY-----
";

    #[derive(Clone)]
    struct TestServer {
        user_key: PublicKey,
    }

    impl russh::server::Handler for TestServer {
        type Error = russh::Error;

        async fn auth_password(
            &mut self,
            user: &str,
            password: &str,
        ) -> Result<russh::server::Auth, Self::Error> {
            Ok(if user == "deploy" && password == "pw" {
                russh::server::Auth::Accept
            } else {
                russh::server::Auth::reject()
            })
        }

        async fn auth_publickey(
            &mut self,
            user: &str,
            key: russh::server::VerifiedPublicKey<'_>,
        ) -> Result<russh::server::Auth, Self::Error> {
            Ok(
                if user == "deploy" && key.public_key.key_data() == self.user_key.key_data() {
                    russh::server::Auth::Accept
                } else {
                    russh::server::Auth::reject()
                },
            )
        }

        async fn channel_open_direct_tcpip(
            &mut self,
            channel: russh::Channel<russh::server::Msg>,
            host: &str,
            port: u32,
            _: &str,
            _: u32,
            reply: russh::server::ChannelOpenHandle,
            _: &mut russh::server::Session,
        ) -> Result<(), Self::Error> {
            match tokio::net::TcpStream::connect((host, port as u16)).await {
                Ok(mut tcp) => {
                    reply.accept().await;
                    tokio::spawn(async move {
                        let mut stream = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                    });
                }
                Err(_) => {
                    reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
                }
            }
            Ok(())
        }
    }

    /// An echo server, an SSH server in front of it; returns their ports.
    async fn servers() -> (u16, u16) {
        let echo = TcpListener::bind((LOCAL_HOST, 0)).await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = tokio::io::split(sock);
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        let config = Arc::new(russh::server::Config {
            keys: vec![russh::keys::decode_secret_key(HOST_KEY, None).unwrap()],
            auth_rejection_time: Duration::from_millis(10),
            ..Default::default()
        });
        let handler = TestServer {
            user_key: russh::keys::decode_secret_key(USER_KEY, Some("s3cret"))
                .unwrap()
                .public_key()
                .clone(),
        };
        let ssh = TcpListener::bind((LOCAL_HOST, 0)).await.unwrap();
        let ssh_port = ssh.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = ssh.accept().await {
                let (config, handler) = (config.clone(), handler.clone());
                tokio::spawn(async move {
                    if let Ok(session) = russh::server::run_stream(config, sock, handler).await {
                        let _ = session.await;
                    }
                });
            }
        });
        (echo_port, ssh_port)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("databridge-ssh-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn echo_through(local_port: u16, msg: &[u8]) -> Vec<u8> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::net::TcpStream::connect((LOCAL_HOST, local_port))
            .await
            .unwrap();
        sock.write_all(msg).await.unwrap();
        let mut buf = vec![0u8; msg.len()];
        sock.read_exact(&mut buf).await.unwrap();
        buf
    }

    #[test]
    fn tunnel_forwards_and_checks_host_keys() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (echo_port, ssh_port) = servers().await;
            let dir = temp_dir("e2e");
            let known = dir.join("known_hosts");
            let mut cfg = SshConfig {
                host: LOCAL_HOST.into(),
                port: ssh_port,
                user: "deploy".into(),
                auth: SshAuth::Password,
                key_path: String::new(),
                secret: "pw".into(),
            };

            // Password auth: two DB connections over one SSH session; the
            // unknown host key is recorded (trust on first use).
            let tunnel = Tunnel::open(&cfg, LOCAL_HOST, echo_port, known.clone())
                .await
                .unwrap();
            assert_eq!(echo_through(tunnel.local_port, b"ping").await, b"ping");
            assert_eq!(echo_through(tunnel.local_port, b"pong!").await, b"pong!");
            let recorded = std::fs::read_to_string(&known).unwrap();
            assert!(
                recorded.contains(&format!("[{LOCAL_HOST}]:{ssh_port}")),
                "{recorded}"
            );
            drop(tunnel);

            // The recorded key is accepted next time.
            let tunnel = Tunnel::open(&cfg, LOCAL_HOST, echo_port, known.clone())
                .await
                .unwrap();
            assert_eq!(echo_through(tunnel.local_port, b"again").await, b"again");
            drop(tunnel);

            // Wrong password.
            cfg.secret = "nope".into();
            let err = Tunnel::open(&cfg, LOCAL_HOST, echo_port, known.clone())
                .await
                .err()
                .unwrap();
            assert!(
                err.message().contains("authentication failed"),
                "{}",
                err.message()
            );

            // Key file with a passphrase; then without it.
            let key = dir.join("id_test");
            std::fs::write(&key, USER_KEY).unwrap();
            cfg.auth = SshAuth::Key;
            cfg.key_path = key.to_string_lossy().into_owned();
            cfg.secret = "s3cret".into();
            let tunnel = Tunnel::open(&cfg, LOCAL_HOST, echo_port, known.clone())
                .await
                .unwrap();
            assert_eq!(echo_through(tunnel.local_port, b"key").await, b"key");
            drop(tunnel);
            cfg.secret = String::new();
            let err = Tunnel::open(&cfg, LOCAL_HOST, echo_port, known.clone())
                .await
                .err()
                .unwrap();
            assert!(err.message().contains("passphrase"), "{}", err.message());

            // The database is unreachable from the SSH server.
            cfg.secret = "s3cret".into();
            let dead = TcpListener::bind((LOCAL_HOST, 0)).await.unwrap();
            let dead_port = dead.local_addr().unwrap().port();
            drop(dead);
            let err = Tunnel::open(&cfg, LOCAL_HOST, dead_port, known.clone())
                .await
                .err()
                .unwrap();
            assert!(err.message().contains("cannot reach"), "{}", err.message());

            // A different key recorded for this host (same algorithm,
            // ed25519): refused as changed.
            let other = russh::keys::decode_secret_key(USER_KEY, Some("s3cret")).unwrap();
            let other_known = dir.join("known_hosts_changed");
            known_hosts::learn_known_hosts_path(
                LOCAL_HOST,
                ssh_port,
                other.public_key(),
                &other_known,
            )
            .unwrap();
            let err = Tunnel::open(&cfg, LOCAL_HOST, echo_port, other_known)
                .await
                .err()
                .unwrap();
            assert!(err.message().contains("has changed"), "{}", err.message());

            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
