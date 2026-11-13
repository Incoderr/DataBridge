//! SCRAM-SHA-256 (RFC 7677) and SCRAM-SHA-1 (RFC 5802) authentication
//! over `saslStart` / `saslContinue`. SHA-1 hashes the password the
//! MongoDB way first (`md5("user:mongo:password")` hex); SHA-256 applies
//! SASLprep. The server signature is verified, so a fake server cannot
//! pretend the login succeeded.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bson::spec::BinarySubtype;
use bson::{Binary, Bson, Document, doc};
use hmac::{Hmac, Mac};
use sha2::Digest as _;

use super::wire::{Conn, check_ok};
use crate::core::error::DbError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mechanism {
    Sha1,
    Sha256,
}

impl Mechanism {
    pub fn name(self) -> &'static str {
        match self {
            Mechanism::Sha1 => "SCRAM-SHA-1",
            Mechanism::Sha256 => "SCRAM-SHA-256",
        }
    }

    fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        match self {
            Mechanism::Sha1 => {
                let mut m = Hmac::<sha1::Sha1>::new_from_slice(key).expect("any key length");
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }
            Mechanism::Sha256 => {
                let mut m = Hmac::<sha2::Sha256>::new_from_slice(key).expect("any key length");
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }
        }
    }

    fn hash(self, data: &[u8]) -> Vec<u8> {
        match self {
            Mechanism::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Mechanism::Sha256 => sha2::Sha256::digest(data).to_vec(),
        }
    }

    /// `Hi()` of RFC 5802 — PBKDF2 with this mechanism's HMAC.
    fn hi(self, password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
        let mut first = salt.to_vec();
        first.extend_from_slice(&1u32.to_be_bytes());
        let mut u = self.hmac(password, &first);
        let mut out = u.clone();
        for _ in 1..iterations {
            u = self.hmac(password, &u);
            out.iter_mut().zip(&u).for_each(|(o, x)| *o ^= x);
        }
        out
    }

    /// The password as the mechanism feeds it into `Hi()`.
    fn prepare_password(self, user: &str, password: &str) -> Result<String, DbError> {
        Ok(match self {
            Mechanism::Sha1 => {
                use md5::Digest as _;
                hex(&md5::Md5::digest(format!("{user}:mongo:{password}").as_bytes()))
            }
            Mechanism::Sha256 => stringprep::saslprep(password)
                .map_err(|e| DbError::new(format!("invalid password: {e}")))?
                .into_owned(),
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `saslname` escaping of RFC 5802.
fn escape_user(user: &str) -> String {
    user.replace('=', "=3D").replace(',', "=2C")
}

fn binary(bytes: Vec<u8>) -> Bson {
    Bson::Binary(Binary {
        subtype: BinarySubtype::Generic,
        bytes,
    })
}

fn payload(reply: &Document) -> Result<String, DbError> {
    let bytes = reply
        .get_binary_generic("payload")
        .map_err(|_| DbError::new("authentication reply has no payload"))?;
    String::from_utf8(bytes.clone()).map_err(|_| DbError::new("authentication payload is not UTF-8"))
}

/// `k=v,k=v` → the value of `key`.
fn attr<'a>(msg: &'a str, key: char) -> Option<&'a str> {
    msg.split(',')
        .find_map(|part| part.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

pub async fn authenticate(
    conn: &mut Conn,
    source: &str,
    user: &str,
    password: &str,
    mech: Mechanism,
) -> Result<(), DbError> {
    let failed = |e: DbError| DbError::new(format!("authentication failed: {}", e.message()));
    let nonce = B64.encode(rand::random::<[u8; 24]>());
    let first_bare = format!("n={},r={nonce}", escape_user(user));
    let reply = conn
        .command(
            source,
            doc! {
                "saslStart": 1,
                "mechanism": mech.name(),
                "payload": binary(format!("n,,{first_bare}").into_bytes()),
                "autoAuthorize": 1,
                "options": { "skipEmptyExchange": true },
            },
        )
        .await?;
    let reply = check_ok(reply).map_err(failed)?;
    let conversation = reply.get("conversationId").cloned().unwrap_or(Bson::Int32(1));
    let server_first = payload(&reply)?;

    let (Some(server_nonce), Some(salt), Some(iterations)) = (
        attr(&server_first, 'r'),
        attr(&server_first, 's'),
        attr(&server_first, 'i').and_then(|i| i.parse::<u32>().ok()),
    ) else {
        return Err(DbError::new("malformed SCRAM server message"));
    };
    if !server_nonce.starts_with(&nonce) || iterations == 0 {
        return Err(DbError::new("SCRAM server nonce mismatch"));
    }
    let salt = B64
        .decode(salt)
        .map_err(|_| DbError::new("malformed SCRAM salt"))?;

    let prepared = mech.prepare_password(user, password)?;
    let salted = mech.hi(prepared.as_bytes(), &salt, iterations);
    let client_key = mech.hmac(&salted, b"Client Key");
    let stored_key = mech.hash(&client_key);
    let without_proof = format!("c=biws,r={server_nonce}");
    let auth_message = format!("{first_bare},{server_first},{without_proof}");
    let signature = mech.hmac(&stored_key, auth_message.as_bytes());
    let proof: Vec<u8> = client_key.iter().zip(&signature).map(|(a, b)| a ^ b).collect();
    let server_key = mech.hmac(&salted, b"Server Key");
    let server_signature = B64.encode(mech.hmac(&server_key, auth_message.as_bytes()));

    let reply = conn
        .command(
            source,
            doc! {
                "saslContinue": 1,
                "conversationId": conversation.clone(),
                "payload": binary(format!("{without_proof},p={}", B64.encode(proof)).into_bytes()),
            },
        )
        .await?;
    let mut reply = check_ok(reply).map_err(failed)?;
    let server_final = payload(&reply)?;
    if let Some(e) = attr(&server_final, 'e') {
        return Err(DbError::new(format!("authentication failed: {e}")));
    }
    if attr(&server_final, 'v') != Some(server_signature.as_str()) {
        return Err(DbError::new("authentication failed: server signature mismatch"));
    }
    // Servers without `skipEmptyExchange` want one more empty round.
    while !reply.get_bool("done").unwrap_or(true) {
        reply = check_ok(
            conn.command(
                source,
                doc! {
                    "saslContinue": 1,
                    "conversationId": conversation.clone(),
                    "payload": binary(Vec::new()),
                },
            )
            .await?,
        )
        .map_err(failed)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7677 §3 test vector (user "user", password "pencil").
    #[test]
    fn sha256_proof_matches_rfc7677() {
        let m = Mechanism::Sha256;
        let salt = B64.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let salted = m.hi(b"pencil", &salt, 4096);
        let client_key = m.hmac(&salted, b"Client Key");
        let stored = m.hash(&client_key);
        let auth = "n=user,r=rOprNGfwEbeRWgbNEkqO,\
                    r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096,\
                    c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        let sig = m.hmac(&stored, auth.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(&sig).map(|(a, b)| a ^ b).collect();
        assert_eq!(B64.encode(proof), "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=");
    }

    #[test]
    fn sha1_password_digest_is_mongo_style() {
        // md5("user:mongo:pencil")
        let p = Mechanism::Sha1.prepare_password("user", "pencil").unwrap();
        assert_eq!(p, "1c33006ec1ffd90f9cadcbcc0e118200");
    }

    #[test]
    fn user_names_are_escaped() {
        assert_eq!(escape_user("a=b,c"), "a=3Db=2Cc");
    }
}
