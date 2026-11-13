//! MongoDB wire protocol: one TCP (optionally TLS) connection speaking
//! `OP_MSG` (opcode 2013, MongoDB 3.6+). Commands are single-section
//! messages — a kind-0 body document carrying `$db` — and replies are
//! parsed back into a [`Document`]. No compression is negotiated, so
//! `OP_COMPRESSED` never arrives.

use std::sync::Arc;
use std::time::Duration;

use bson::Document;
use rustls::ClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::core::error::DbError;

const OP_MSG: i32 = 2013;
/// The server's default `maxMessageSizeBytes`.
const MAX_MESSAGE: usize = 48_000_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Generous: aggregations over big collections are legitimate.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// TLS settings from the connection string.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tls {
    pub enabled: bool,
    /// `tlsAllowInvalidCertificates` / `tlsInsecure`: skip certificate and
    /// hostname verification (self-signed dev servers).
    pub insecure: bool,
}

pub struct Conn {
    io: Box<dyn Io>,
    next_id: i32,
}

impl Conn {
    pub async fn connect(host: &str, port: u16, tls: Tls) -> Result<Self, DbError> {
        let addr = format!("{}:{port}", host.trim_start_matches('[').trim_end_matches(']'));
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
            .await
            .map_err(|_| DbError::new(format!("timed out connecting to {host}:{port}")))?
            .map_err(|e| DbError::with_source(format!("cannot connect to {host}:{port}: {e}"), e))?;
        let _ = tcp.set_nodelay(true);
        let io: Box<dyn Io> = if tls.enabled {
            let name = ServerName::try_from(
                host.trim_start_matches('[').trim_end_matches(']').to_string(),
            )
            .map_err(|e| DbError::with_source(format!("invalid TLS server name {host}"), e))?;
            let stream = tokio::time::timeout(
                CONNECT_TIMEOUT,
                TlsConnector::from(tls_config(tls.insecure)?).connect(name, tcp),
            )
            .await
            .map_err(|_| DbError::new(format!("TLS handshake with {host}:{port} timed out")))?
            .map_err(|e| DbError::with_source(format!("TLS handshake failed: {e}"), e))?;
            Box::new(stream)
        } else {
            Box::new(tcp)
        };
        Ok(Self { io, next_id: 1 })
    }

    /// Send one command to `db` and return the raw reply. `Err` means the
    /// transport failed (the connection is unusable); a server-side
    /// failure comes back as a reply with `ok: 0` — see [`check_ok`].
    pub async fn command(&mut self, db: &str, mut cmd: Document) -> Result<Document, DbError> {
        cmd.insert("$db", db);
        tokio::time::timeout(COMMAND_TIMEOUT, self.roundtrip(&cmd))
            .await
            .map_err(|_| DbError::new("MongoDB command timed out"))?
    }

    async fn roundtrip(&mut self, cmd: &Document) -> Result<Document, DbError> {
        let mut body = Vec::new();
        cmd.to_writer(&mut body)
            .map_err(|e| DbError::with_source("failed to encode the command", e))?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let len = 16 + 4 + 1 + body.len();
        let mut msg = Vec::with_capacity(len);
        msg.extend_from_slice(&(len as i32).to_le_bytes());
        msg.extend_from_slice(&id.to_le_bytes());
        msg.extend_from_slice(&0i32.to_le_bytes());
        msg.extend_from_slice(&OP_MSG.to_le_bytes());
        msg.extend_from_slice(&0u32.to_le_bytes()); // flagBits
        msg.push(0); // section kind 0: body
        msg.extend_from_slice(&body);
        self.io.write_all(&msg).await.map_err(io_error)?;
        self.io.flush().await.map_err(io_error)?;

        let mut header = [0u8; 16];
        self.io.read_exact(&mut header).await.map_err(io_error)?;
        let len = i32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
        let opcode = i32::from_le_bytes(header[12..16].try_into().unwrap());
        if !(16 + 5..=MAX_MESSAGE).contains(&len) {
            return Err(DbError::new(format!("invalid MongoDB reply length {len}")));
        }
        let mut rest = vec![0u8; len - 16];
        self.io.read_exact(&mut rest).await.map_err(io_error)?;
        if opcode != OP_MSG {
            return Err(DbError::new(format!("unexpected MongoDB reply opcode {opcode}")));
        }
        parse_op_msg(&rest)
    }
}

/// The body (kind-0 section) of an `OP_MSG` payload (after the header).
fn parse_op_msg(rest: &[u8]) -> Result<Document, DbError> {
    let flags = u32::from_le_bytes(rest[0..4].try_into().unwrap());
    // checksumPresent: a trailing CRC-32C we do not verify.
    let end = if flags & 1 != 0 { rest.len().saturating_sub(4) } else { rest.len() };
    let mut pos = 4;
    while pos < end {
        let kind = rest[pos];
        pos += 1;
        let size = rest
            .get(pos..pos + 4)
            .map(|b| i32::from_le_bytes(b.try_into().unwrap()) as usize)
            .ok_or_else(|| DbError::new("truncated MongoDB reply"))?;
        if kind == 0 {
            let bytes = rest
                .get(pos..pos + size)
                .ok_or_else(|| DbError::new("truncated MongoDB reply"))?;
            return Document::from_reader(bytes)
                .map_err(|e| DbError::with_source("malformed MongoDB reply", e));
        }
        pos += size;
    }
    Err(DbError::new("MongoDB reply has no body"))
}

fn io_error(e: std::io::Error) -> DbError {
    DbError::with_source(format!("MongoDB connection lost: {e}"), e)
}

/// Turn an `ok: 0` reply into the server's own message
/// (`errmsg` + `codeName`); `ok: 1` passes through.
pub fn check_ok(reply: Document) -> Result<Document, DbError> {
    let ok = match reply.get("ok") {
        Some(bson::Bson::Double(v)) => *v == 1.0,
        Some(bson::Bson::Int32(v)) => *v == 1,
        Some(bson::Bson::Int64(v)) => *v == 1,
        Some(bson::Bson::Boolean(v)) => *v,
        _ => false,
    };
    if ok {
        return Ok(reply);
    }
    let msg = reply.get_str("errmsg").unwrap_or("command failed");
    Err(match reply.get_str("codeName") {
        Ok(code) if !code.is_empty() => DbError::new(format!("{msg} ({code})")),
        _ => DbError::new(msg.to_string()),
    })
}

fn tls_config(insecure: bool) -> Result<Arc<ClientConfig>, DbError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| DbError::with_source("TLS setup failed", e))?;
    let config = if insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        // OS store too: private CAs of company / cloud deployments.
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// `tlsAllowInvalidCertificates`: accept any certificate, but still check
/// the handshake signatures (the session stays encrypted).
#[derive(Debug)]
struct AcceptAny(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn op_msg_body_roundtrip() {
        let d = doc! { "ok": 1.0, "n": 3 };
        let mut body = Vec::new();
        d.to_writer(&mut body).unwrap();
        let mut rest = 0u32.to_le_bytes().to_vec();
        rest.push(0);
        rest.extend_from_slice(&body);
        assert_eq!(parse_op_msg(&rest).unwrap(), d);
    }

    #[test]
    fn check_ok_reports_server_message() {
        let err = check_ok(doc! { "ok": 0.0, "errmsg": "ns not found", "codeName": "NamespaceNotFound" })
            .unwrap_err();
        assert_eq!(err.message(), "ns not found (NamespaceNotFound)");
        assert!(check_ok(doc! { "ok": 1 }).is_ok());
    }
}
