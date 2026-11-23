//! `sich://pair?v=1&id=..&addrs=a,b&relay=..&secret=..&host=..`

const PREFIX: &str = "sich://pair";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairUrl {
    pub endpoint_id: String,
    pub addrs: Vec<String>,
    pub relay: Option<String>,
    pub secret: String,
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairUrlError {
    #[error("not a sich://pair url")]
    BadScheme,
    #[error("unsupported pair url version")]
    BadVersion,
    #[error("missing or empty parameter: {0}")]
    Missing(&'static str),
    #[error("bad percent-encoding")]
    BadEncoding,
}

fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn dec(s: &str) -> Result<String, PairUrlError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3).ok_or(PairUrlError::BadEncoding)?;
            if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err(PairUrlError::BadEncoding);
            }
            out.push(u8::from_str_radix(hex, 16).map_err(|_| PairUrlError::BadEncoding)?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| PairUrlError::BadEncoding)
}

impl PairUrl {
    pub fn to_url(&self) -> String {
        let addrs = self.addrs.iter().map(|a| enc(a)).collect::<Vec<_>>().join(",");
        format!(
            "{PREFIX}?v=1&id={}&addrs={}&relay={}&secret={}&host={}",
            enc(&self.endpoint_id),
            addrs,
            enc(self.relay.as_deref().unwrap_or("")),
            enc(&self.secret),
            enc(&self.host),
        )
    }

    pub fn parse(url: &str) -> Result<PairUrl, PairUrlError> {
        let rest = url.strip_prefix(PREFIX).ok_or(PairUrlError::BadScheme)?;
        let query = rest.strip_prefix('?').ok_or(PairUrlError::BadScheme)?;
        let (mut v, mut id, mut addrs, mut relay, mut secret, mut host) =
            (None, None, None, None, None, None);
        for pair in query.split('&') {
            let (k, val) = pair.split_once('=').unwrap_or((pair, ""));
            let slot = match k {
                "v" => &mut v,
                "id" => &mut id,
                "addrs" => &mut addrs,
                "relay" => &mut relay,
                "secret" => &mut secret,
                "host" => &mut host,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(val);
            }
        }
        if v != Some("1") {
            return Err(PairUrlError::BadVersion);
        }
        let endpoint_id = dec(id.unwrap_or(""))?;
        if endpoint_id.is_empty() {
            return Err(PairUrlError::Missing("id"));
        }
        let secret = dec(secret.unwrap_or(""))?;
        if secret.is_empty() {
            return Err(PairUrlError::Missing("secret"));
        }
        let mut addr_list = Vec::new();
        for a in addrs.unwrap_or("").split(',').filter(|a| !a.is_empty()) {
            addr_list.push(dec(a)?);
        }
        let relay = dec(relay.unwrap_or(""))?;
        let host = dec(host.unwrap_or(""))?;
        Ok(PairUrl {
            endpoint_id,
            addrs: addr_list,
            relay: if relay.is_empty() { None } else { Some(relay) },
            secret,
            host,
        })
    }
}
