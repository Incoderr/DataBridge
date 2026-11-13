//! SRV and TXT lookups for `mongodb+srv://` seed lists — without a
//! resolver crate. Windows asks the system resolver (`DnsQuery_W`, honours
//! the OS DNS settings, VPNs, hosts policies); other platforms send a
//! plain DNS query (UDP, TCP when truncated) to the `nameserver`s of
//! `/etc/resolv.conf`.

use crate::core::error::DbError;

/// SRV targets as `(host, port)`, ordered by priority.
pub async fn lookup_srv(name: &str) -> Result<Vec<(String, u16)>, DbError> {
    let name = name.to_string();
    let mut records = query(name.clone(), TYPE_SRV).await?;
    records.sort_by_key(|r| match r {
        Record::Srv { priority, .. } => *priority,
        Record::Txt(_) => u16::MAX,
    });
    let out: Vec<(String, u16)> = records
        .into_iter()
        .filter_map(|r| match r {
            Record::Srv { target, port, .. } => {
                Some((target.trim_end_matches('.').to_ascii_lowercase(), port))
            }
            Record::Txt(_) => None,
        })
        .collect();
    if out.is_empty() {
        return Err(DbError::new(format!("DNS: no SRV records for {name}")));
    }
    Ok(out)
}

/// TXT records of `name` (each record's strings concatenated); empty when
/// there are none.
pub async fn lookup_txt(name: &str) -> Result<Vec<String>, DbError> {
    Ok(query(name.to_string(), TYPE_TXT)
        .await?
        .into_iter()
        .filter_map(|r| match r {
            Record::Txt(s) => Some(s),
            Record::Srv { .. } => None,
        })
        .collect())
}

const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;

#[derive(Debug, Clone, PartialEq)]
enum Record {
    Srv {
        priority: u16,
        port: u16,
        target: String,
    },
    Txt(String),
}

// ---------------- Windows: the system resolver ----------------

#[cfg(windows)]
async fn query(name: String, qtype: u16) -> Result<Vec<Record>, DbError> {
    tokio::task::spawn_blocking(move || windows_query(&name, qtype))
        .await
        .map_err(|_| DbError::new("DNS lookup task failed"))?
}

#[cfg(windows)]
fn windows_query(name: &str, qtype: u16) -> Result<Vec<Record>, DbError> {
    use windows_sys::Win32::NetworkManagement::Dns::{
        DNS_QUERY_STANDARD, DNS_RECORDA, DNS_RECORDW, DnsFree, DnsFreeRecordList, DnsQuery_W,
    };
    const NAME_ERROR: u32 = 9003; // DNS_ERROR_RCODE_NAME_ERROR (NXDOMAIN)
    const NO_RECORDS: u32 = 9501; // DNS_INFO_NO_RECORDS

    unsafe fn wide(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut len = 0;
        // SAFETY: `p` is a NUL-terminated string owned by the record list.
        unsafe {
            while *p.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
        }
    }

    let wname: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut results: *mut DNS_RECORDA = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated name, out-pointer to a record list we
    // free below with DnsFree(DnsFreeRecordList).
    let status = unsafe {
        DnsQuery_W(
            wname.as_ptr(),
            qtype,
            DNS_QUERY_STANDARD,
            std::ptr::null_mut(),
            &mut results,
            std::ptr::null_mut(),
        )
    };
    if status == NAME_ERROR || status == NO_RECORDS {
        return Ok(Vec::new());
    }
    if status != 0 {
        return Err(DbError::new(format!(
            "DNS lookup of {name} failed (error {status})"
        )));
    }
    let mut out = Vec::new();
    let mut rec = results as *const DNS_RECORDW;
    // SAFETY: walking the list DnsQuery_W returned; union fields are read
    // according to each record's `wType`.
    unsafe {
        while !rec.is_null() {
            let r = &*rec;
            if r.wType == qtype && qtype == TYPE_SRV {
                let srv = r.Data.SRV;
                out.push(Record::Srv {
                    priority: srv.wPriority,
                    port: srv.wPort,
                    target: wide(srv.pNameTarget),
                });
            } else if r.wType == qtype && qtype == TYPE_TXT {
                let txt = &r.Data.TXT;
                let strings = std::slice::from_raw_parts(
                    txt.pStringArray.as_ptr(),
                    txt.dwStringCount as usize,
                );
                out.push(Record::Txt(strings.iter().map(|s| wide(*s)).collect()));
            }
            rec = r.pNext;
        }
        if !results.is_null() {
            DnsFree(results as *const _, DnsFreeRecordList);
        }
    }
    Ok(out)
}

// ---------------- elsewhere: a minimal DNS client ----------------

#[cfg(not(windows))]
async fn query(name: String, qtype: u16) -> Result<Vec<Record>, DbError> {
    let conf = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let servers = nameservers(&conf);
    if servers.is_empty() {
        return Err(DbError::new("DNS: no nameserver in /etc/resolv.conf"));
    }
    let mut last = None;
    for server in servers {
        match ask(&server, &name, qtype).await {
            Ok(r) => return Ok(r),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap())
}

#[cfg(not(windows))]
async fn ask(server: &str, name: &str, qtype: u16) -> Result<Vec<Record>, DbError> {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let fail = |e: std::io::Error| DbError::with_source(format!("DNS query to {server} failed: {e}"), e);
    let id: u16 = rand::random();
    let packet = encode_query(id, name, qtype)?;
    let addr = if server.contains(':') { format!("[{server}]:53") } else { format!("{server}:53") };

    let bind = if server.contains(':') { "[::]:0" } else { "0.0.0.0:0" };
    let sock = tokio::net::UdpSocket::bind(bind).await.map_err(fail)?;
    sock.connect(&addr).await.map_err(fail)?;
    let mut buf = vec![0u8; 4096];
    let mut reply = None;
    for _ in 0..2 {
        sock.send(&packet).await.map_err(fail)?;
        if let Ok(n) = tokio::time::timeout(Duration::from_secs(3), sock.recv(&mut buf)).await {
            reply = Some(buf[..n.map_err(fail)?].to_vec());
            break;
        }
    }
    let reply = reply.ok_or_else(|| DbError::new(format!("DNS server {server} did not answer")))?;
    match decode_reply(&reply, id, qtype)? {
        Some(records) => Ok(records),
        // Truncated: repeat over TCP (2-byte length prefix).
        None => {
            let mut tcp = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(&addr))
                .await
                .map_err(|_| DbError::new(format!("DNS server {server} timed out")))?
                .map_err(fail)?;
            let mut msg = (packet.len() as u16).to_be_bytes().to_vec();
            msg.extend_from_slice(&packet);
            tcp.write_all(&msg).await.map_err(fail)?;
            let len = tcp.read_u16().await.map_err(fail)? as usize;
            let mut body = vec![0u8; len];
            tcp.read_exact(&mut body).await.map_err(fail)?;
            Ok(decode_reply(&body, id, qtype)?.unwrap_or_default())
        }
    }
}

#[cfg_attr(windows, allow(dead_code))]
fn nameservers(conf: &str) -> Vec<String> {
    conf.lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            (parts.next() == Some("nameserver")).then(|| parts.next()).flatten()
        })
        // Drop an IPv6 zone suffix (`fe80::1%eth0`).
        .map(|s| s.split('%').next().unwrap_or(s).to_string())
        .collect()
}

#[cfg_attr(windows, allow(dead_code))]
fn encode_query(id: u16, name: &str, qtype: u16) -> Result<Vec<u8>, DbError> {
    let mut p = Vec::with_capacity(32 + name.len());
    p.extend_from_slice(&id.to_be_bytes());
    p.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
    p.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // 1 question
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(DbError::new(format!("invalid DNS name {name}")));
        }
        p.push(label.len() as u8);
        p.extend_from_slice(label.as_bytes());
    }
    p.push(0);
    p.extend_from_slice(&qtype.to_be_bytes());
    p.extend_from_slice(&1u16.to_be_bytes()); // IN
    Ok(p)
}

/// `Ok(None)` when the reply is truncated (retry over TCP).
#[cfg_attr(windows, allow(dead_code))]
fn decode_reply(msg: &[u8], id: u16, qtype: u16) -> Result<Option<Vec<Record>>, DbError> {
    let bad = || DbError::new("malformed DNS reply");
    let u16_at = |i: usize| -> Result<u16, DbError> {
        msg.get(i..i + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .ok_or_else(bad)
    };
    if u16_at(0)? != id {
        return Err(DbError::new("DNS reply id mismatch"));
    }
    let flags = u16_at(2)?;
    if flags & 0x0200 != 0 {
        return Ok(None);
    }
    match flags & 0x000F {
        0 => {}
        3 => return Ok(Some(Vec::new())), // NXDOMAIN
        rcode => return Err(DbError::new(format!("DNS server error (rcode {rcode})"))),
    }
    let (qd, an) = (u16_at(4)?, u16_at(6)?);
    let mut pos = 12;
    for _ in 0..qd {
        pos = read_name(msg, pos)?.1 + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        pos = read_name(msg, pos)?.1;
        let rtype = u16_at(pos)?;
        let rdlen = u16_at(pos + 8)? as usize;
        let start = pos + 10;
        let rdata = msg.get(start..start + rdlen).ok_or_else(bad)?;
        if rtype == qtype && qtype == TYPE_SRV {
            out.push(Record::Srv {
                priority: u16_at(start)?,
                port: u16_at(start + 4)?,
                target: read_name(msg, start + 6)?.0,
            });
        } else if rtype == qtype && qtype == TYPE_TXT {
            let mut s = Vec::new();
            let mut i = 0;
            while i < rdata.len() {
                let n = rdata[i] as usize;
                s.extend_from_slice(rdata.get(i + 1..i + 1 + n).ok_or_else(bad)?);
                i += 1 + n;
            }
            out.push(Record::Txt(String::from_utf8_lossy(&s).into_owned()));
        }
        pos = start + rdlen;
    }
    Ok(Some(out))
}

/// A (possibly compressed) name at `pos` → (name, position after it).
#[cfg_attr(windows, allow(dead_code))]
fn read_name(msg: &[u8], mut pos: usize) -> Result<(String, usize), DbError> {
    let bad = || DbError::new("malformed DNS name");
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    for _ in 0..128 {
        let len = *msg.get(pos).ok_or_else(bad)? as usize;
        if len & 0xC0 == 0xC0 {
            let ptr = ((len & 0x3F) << 8) | *msg.get(pos + 1).ok_or_else(bad)? as usize;
            end.get_or_insert(pos + 2);
            pos = ptr;
            continue;
        }
        if len == 0 {
            return Ok((labels.join("."), end.unwrap_or(pos + 1)));
        }
        let label = msg.get(pos + 1..pos + 1 + len).ok_or_else(bad)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        pos += 1 + len;
    }
    Err(bad())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built reply: one SRV answer whose target is compressed
    /// against the question name.
    #[test]
    fn decodes_srv_with_compression_and_txt() {
        let q = encode_query(0x1234, "_mongodb._tcp.c0.ab.mongodb.net", TYPE_SRV).unwrap();
        let mut m = q.clone();
        m[2] = 0x81;
        m[3] = 0x80; // response, RD+RA, rcode 0
        m[7] = 1; // ancount = 1
        m.extend_from_slice(&[0xC0, 12]); // name → question
        m.extend_from_slice(&TYPE_SRV.to_be_bytes());
        m.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
        // rdata: prio 0, weight 0, port 27017, "shard-00" + ptr to "c0.ab…" (offset 12 + 1 + 8 + 1 + 4)
        let rdata = [&[0, 0, 0, 0][..], &27017u16.to_be_bytes(), &[8], b"shard-00", &[0xC0, 26]].concat();
        m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        m.extend_from_slice(&rdata);
        let recs = decode_reply(&m, 0x1234, TYPE_SRV).unwrap().unwrap();
        assert_eq!(
            recs,
            vec![Record::Srv { priority: 0, port: 27017, target: "shard-00.c0.ab.mongodb.net".into() }]
        );

        let mut t = encode_query(7, "c0.ab.mongodb.net", TYPE_TXT).unwrap();
        t[2] = 0x81;
        t[3] = 0x80;
        t[7] = 1;
        t.extend_from_slice(&[0xC0, 12]);
        t.extend_from_slice(&TYPE_TXT.to_be_bytes());
        t.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
        let rdata = [&[16][..], b"authSource=admin", &[3], b"&x="].concat();
        t.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        t.extend_from_slice(&rdata);
        assert_eq!(
            decode_reply(&t, 7, TYPE_TXT).unwrap().unwrap(),
            vec![Record::Txt("authSource=admin&x=".into())]
        );
    }

    /// Real resolver: `cargo test -- --ignored dns_live`.
    #[test]
    #[ignore = "needs network access"]
    fn dns_live() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let srv = rt.block_on(lookup_srv("_imaps._tcp.gmail.com")).unwrap();
        assert_eq!(srv, vec![("imap.gmail.com".to_string(), 993)]);
        let txt = rt.block_on(lookup_txt("gmail.com")).unwrap();
        assert!(txt.iter().any(|t| t.starts_with("v=spf1")));
    }

    #[test]
    fn truncated_reply_asks_for_tcp() {
        let mut m = encode_query(1, "a.b.c", TYPE_SRV).unwrap();
        m[2] = 0x83; // TC set
        assert_eq!(decode_reply(&m, 1, TYPE_SRV).unwrap(), None);
    }

    #[test]
    fn resolv_conf_nameservers() {
        let conf = "# c\nnameserver 10.0.0.1\nsearch lan\nnameserver fe80::1%eth0\n";
        assert_eq!(nameservers(conf), vec!["10.0.0.1", "fe80::1"]);
    }
}
