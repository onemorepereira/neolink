//! Authentication (SPEC R6, DESIGN §6.5)
//!
//! Basic and Digest (RFC 2617 qop=auth, and RFC 2069 without qop). The Digest nonce is
//! `b64(ts ‖ salt ‖ HMAC-SHA256(key, ts ‖ salt ‖ client_ip)[..16])` with 8 random salt
//! bytes (so two clients behind one IP never share a nonce): bound to the client IP
//! (clients reconnect after a 401), valid 5 minutes, then `stale=true`. `nc` must increase
//! per nonce (LRU of 4096 nonces). The digest's `uri` must name the requested mount.
//! Without qop (RFC 2069, live555) a response can be replayed by the same IP for the same
//! mount within the nonce's 5 minutes; that is the protocol's limit. Credential checks are
//! constant time, also for unknown user names.
//!
//! The allowed set per mount follows today's GStreamer rules: `anyone` = every user,
//! `anonymous` = no credentials needed, a list = those users, absent with users =
//! everyone, absent without users = anonymous. Wrong or missing credentials get 401; a
//! valid user outside the allowed set gets 404.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;

use super::headers::{authorization, Authorization};
use super::route::normalise;

pub const REALM: &str = "neolink";
const NONCE_TTL_S: u64 = 300;
const NC_LRU: usize = 4096;

/// Which schemes to offer (`rtsp_auth`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schemes {
    Both,
    Digest,
    Basic,
}

#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub pass: Option<String>,
}

/// Who may open a mount
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Allowed {
    Anonymous,
    /// Anonymous plus these users (`anonymous` listed with others)
    AnonymousAnd(Vec<String>),
    Everyone,
    Users(Vec<String>),
}

impl Allowed {
    /// From a camera's `permitted_users` and whether any users are configured
    pub fn from_config(permitted: Option<&[String]>, have_users: bool) -> Self {
        match permitted {
            None if have_users => Allowed::Everyone,
            None => Allowed::Anonymous,
            Some(list) if list.iter().any(|u| u == "anyone") => Allowed::Everyone,
            Some(list) if list.iter().any(|u| u == "anonymous") => {
                Allowed::AnonymousAnd(list.iter().filter(|u| *u != "anonymous").cloned().collect())
            }
            Some(list) => Allowed::Users(list.to_vec()),
        }
    }

    fn anonymous(&self) -> bool {
        matches!(self, Allowed::Anonymous | Allowed::AnonymousAnd(_))
    }

    fn permits(&self, user: &str) -> bool {
        match self {
            Allowed::Anonymous => false,
            Allowed::Everyone => true,
            Allowed::AnonymousAnd(l) | Allowed::Users(l) => l.iter().any(|u| u == user),
        }
    }
}

/// The outcome for one request
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Proceed, as this user (None: anonymous)
    Ok(Option<String>),
    /// 401 with these WWW-Authenticate values
    Challenge(Vec<String>),
    /// 404: a valid user who may not open this mount
    NotFound,
    /// The request carried credentials that failed (for failed-auth limiting)
    Failed(Vec<String>),
}

pub struct Auth {
    users: Vec<User>,
    schemes: Schemes,
    key: [u8; 32],
    /// nonce → last nc, oldest first
    nc: Mutex<(HashMap<String, u32>, VecDeque<String>)>,
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn md5_hex(s: &str) -> String {
    format!("{:x}", md5::compute(s.as_bytes()))
}

/// Constant time over the longer input
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() != b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

impl Auth {
    pub fn new(users: Vec<User>, schemes: Schemes) -> Self {
        use rand::RngCore;
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        Self {
            users,
            schemes,
            key,
            nc: Mutex::new((HashMap::new(), VecDeque::new())),
        }
    }

    pub fn have_users(&self) -> bool {
        !self.users.is_empty()
    }

    fn mac(&self, ts: u64, salt: &[u8], ip: IpAddr) -> [u8; 16] {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.key).expect("any key length");
        m.update(&ts.to_be_bytes());
        m.update(salt);
        m.update(ip.to_string().as_bytes());
        let out = m.finalize().into_bytes();
        let mut r = [0u8; 16];
        r.copy_from_slice(&out[..16]);
        r
    }

    pub fn nonce_at(&self, ts: u64, ip: IpAddr) -> String {
        let salt: [u8; 8] = rand::random();
        let mut raw = ts.to_be_bytes().to_vec();
        raw.extend_from_slice(&salt);
        raw.extend_from_slice(&self.mac(ts, &salt, ip));
        B64.encode(raw)
    }

    /// `Ok(true)` valid and fresh, `Ok(false)` valid but stale, `Err` not ours/not this IP
    fn check_nonce(&self, nonce: &str, ip: IpAddr, now: u64) -> Result<bool, ()> {
        let raw = B64.decode(nonce).map_err(|_| ())?;
        if raw.len() != 32 {
            return Err(());
        }
        let ts = u64::from_be_bytes(raw[..8].try_into().unwrap());
        if !ct_eq(&raw[16..], &self.mac(ts, &raw[8..16], ip)) {
            return Err(());
        }
        Ok(now.saturating_sub(ts) <= NONCE_TTL_S && ts <= now + 5)
    }

    fn challenges(&self, ip: IpAddr, stale: bool, now: u64) -> Vec<String> {
        let mut out = vec![];
        if self.schemes != Schemes::Basic {
            out.push(format!(
                "Digest realm=\"{REALM}\", nonce=\"{}\"{}",
                self.nonce_at(now, ip),
                if stale { ", stale=TRUE" } else { "" }
            ));
        }
        if self.schemes != Schemes::Digest {
            out.push(format!("Basic realm=\"{REALM}\""));
        }
        out
    }

    fn user(&self, name: &str) -> Option<&User> {
        // Scan everything so timing doesn't reveal which names exist
        let mut found = None;
        for u in &self.users {
            if ct_eq(u.name.as_bytes(), name.as_bytes()) {
                found = Some(u);
            }
        }
        found
    }

    /// Decide on a request
    /// `uri` is the request URI: a Digest response must be for the same mount
    pub fn check(
        &self,
        allowed: &Allowed,
        method: &str,
        uri: &str,
        authorization_header: Option<&str>,
        ip: IpAddr,
    ) -> Decision {
        self.check_at(allowed, method, uri, authorization_header, ip, now_s())
    }

    pub fn check_at(
        &self,
        allowed: &Allowed,
        method: &str,
        uri: &str,
        header: Option<&str>,
        ip: IpAddr,
        now: u64,
    ) -> Decision {
        let Some(header) = header else {
            if allowed.anonymous() {
                return Decision::Ok(None);
            }
            return Decision::Challenge(self.challenges(ip, false, now));
        };
        let verified = match authorization(header) {
            Some(Authorization::Basic { user, pass }) if self.schemes != Schemes::Digest => self
                .user(&user)
                .filter(|u| ct_eq(u.pass.as_deref().unwrap_or("").as_bytes(), pass.as_bytes()))
                .map(|u| u.name.clone()),
            Some(Authorization::Digest(p)) if self.schemes != Schemes::Basic => {
                match self.digest(&p, method, uri, ip, now) {
                    Ok(name) => Some(name),
                    Err(DigestError::Stale) => {
                        return Decision::Challenge(self.challenges(ip, true, now))
                    }
                    Err(DigestError::Bad) => None,
                }
            }
            _ => None,
        };
        match verified {
            Some(name) if allowed.permits(&name) || allowed.anonymous() => Decision::Ok(Some(name)),
            Some(_) => Decision::NotFound,
            None if allowed.anonymous() => Decision::Ok(None),
            None => Decision::Failed(self.challenges(ip, false, now)),
        }
    }

    fn digest(
        &self,
        p: &HashMap<String, String>,
        method: &str,
        request_uri: &str,
        ip: IpAddr,
        now: u64,
    ) -> Result<String, DigestError> {
        let get = |k: &str| p.get(k).map(|s| s.as_str()).ok_or(DigestError::Bad);
        let (name, nonce, uri, response) = (
            get("username")?,
            get("nonce")?,
            get("uri")?,
            get("response")?,
        );
        if p.get("realm").is_some_and(|r| r != REALM) {
            return Err(DigestError::Bad);
        }
        // The response must be for this mount, not one captured for another
        if uri != request_uri && normalise(uri).mount != normalise(request_uri).mount {
            return Err(DigestError::Bad);
        }
        let fresh = self
            .check_nonce(nonce, ip, now)
            .map_err(|_| DigestError::Bad)?;
        // An unknown name costs the same hashing as a known one
        let found = self.user(name);
        let (uname, upass) = match found {
            Some(u) => (u.name.as_str(), u.pass.as_deref().unwrap_or("")),
            None => ("\u{0}unknown", "\u{0}"),
        };
        let ha1 = md5_hex(&format!("{uname}:{REALM}:{upass}"));
        let ha2 = md5_hex(&format!("{method}:{uri}"));
        let expected = match p.get("qop").map(|s| s.as_str()) {
            Some("auth") => {
                let (nc, cnonce) = (get("nc")?, get("cnonce")?);
                md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"))
            }
            None => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
            Some(_) => return Err(DigestError::Bad),
        };
        let matches = ct_eq(
            expected.as_bytes(),
            response.to_ascii_lowercase().as_bytes(),
        );
        let user = match found {
            Some(u) if matches => u,
            _ => return Err(DigestError::Bad),
        };
        if !fresh {
            return Err(DigestError::Stale);
        }
        if let Some(nc) = p.get("nc") {
            let nc = u32::from_str_radix(nc, 16).map_err(|_| DigestError::Bad)?;
            let mut guard = self.nc.lock().unwrap();
            let (map, order) = &mut *guard;
            match map.get(nonce) {
                Some(&last) if nc <= last => return Err(DigestError::Bad),
                Some(_) => {}
                None => {
                    order.push_back(nonce.to_string());
                    if order.len() > NC_LRU {
                        if let Some(old) = order.pop_front() {
                            map.remove(&old);
                        }
                    }
                }
            }
            map.insert(nonce.to_string(), nc);
        }
        Ok(user.name.clone())
    }
}

enum DigestError {
    Bad,
    Stale,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        Auth::new(
            vec![
                User {
                    name: "alice".into(),
                    pass: Some("alicepw".into()),
                },
                User {
                    name: "bob".into(),
                    pass: Some("bobpw".into()),
                },
                User {
                    name: "nopass".into(),
                    pass: None,
                },
            ],
            Schemes::Both,
        )
    }

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 5));

    fn basic(u: &str) -> String {
        format!("Basic {}", B64.encode(u))
    }

    fn digest_header(a: &Auth, user: &str, pass: &str, nonce: &str, nc: Option<u32>) -> String {
        let uri = "rtsp://h/argus/main";
        let ha1 = md5_hex(&format!("{user}:{REALM}:{pass}"));
        let ha2 = md5_hex(&format!("DESCRIBE:{uri}"));
        let _ = a;
        match nc {
            Some(nc) => {
                let nc = format!("{nc:08x}");
                let r = md5_hex(&format!("{ha1}:{nonce}:{nc}:cn:auth:{ha2}"));
                format!(
                    r#"Digest username="{user}", realm="{REALM}", nonce="{nonce}", uri="{uri}", qop=auth, nc={nc}, cnonce="cn", response="{r}""#
                )
            }
            None => {
                let r = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
                format!(
                    r#"Digest username="{user}", realm="{REALM}", nonce="{nonce}", uri="{uri}", response="{r}""#
                )
            }
        }
    }

    /// RFC 2617 §3.5 example: the response computation itself
    #[test]
    fn test_rfc2617_vector() {
        let ha1 = md5_hex("Mufasa:testrealm@host.com:Circle Of Life");
        let ha2 = md5_hex("GET:/dir/index.html");
        let r = md5_hex(&format!(
            "{ha1}:dcd98b7102dd2f0e8b11d0f600bfb0c093:00000001:0a4f113b:auth:{ha2}"
        ));
        assert_eq!(r, "6629fae49393a05397450978507c4ef1");
    }

    #[test]
    fn test_golden_matrix() {
        let a = auth();
        let only_alice = Allowed::from_config(Some(&["alice".into()]), true);
        // No credentials: 401 with Digest then Basic
        match a.check_at(
            &only_alice,
            "DESCRIBE",
            "rtsp://h/argus/main",
            None,
            IP,
            1000,
        ) {
            Decision::Challenge(c) => {
                assert!(c[0].starts_with("Digest realm=\"neolink\", nonce="));
                assert_eq!(c[1], "Basic realm=\"neolink\"");
            }
            d => panic!("{d:?}"),
        }
        assert!(matches!(
            a.check_at(
                &only_alice,
                "DESCRIBE",
                "rtsp://h/argus/main",
                Some(&basic("alice:wrong")),
                IP,
                1000
            ),
            Decision::Failed(_)
        ));
        assert!(matches!(
            a.check_at(
                &only_alice,
                "DESCRIBE",
                "rtsp://h/argus/main",
                Some(&basic("mallory:x")),
                IP,
                1000
            ),
            Decision::Failed(_)
        ));
        assert_eq!(
            a.check_at(
                &only_alice,
                "DESCRIBE",
                "rtsp://h/argus/main",
                Some(&basic("bob:bobpw")),
                IP,
                1000
            ),
            Decision::NotFound
        );
        assert_eq!(
            a.check_at(
                &only_alice,
                "DESCRIBE",
                "rtsp://h/argus/main",
                Some(&basic("alice:alicepw")),
                IP,
                1000
            ),
            Decision::Ok(Some("alice".into()))
        );
        // No users configured at all: anonymous
        let open = Allowed::from_config(None, false);
        assert_eq!(
            a.check_at(&open, "DESCRIBE", "rtsp://h/argus/main", None, IP, 1000),
            Decision::Ok(None)
        );
        // A user without a password only matches the empty password
        let all = Allowed::from_config(Some(&["anyone".into()]), true);
        assert_eq!(
            a.check_at(
                &all,
                "DESCRIBE",
                "rtsp://h/argus/main",
                Some(&basic("nopass:")),
                IP,
                1000
            ),
            Decision::Ok(Some("nopass".into()))
        );
        assert!(matches!(
            a.check_at(
                &all,
                "DESCRIBE",
                "rtsp://h/argus/main",
                Some(&basic("nopass:x")),
                IP,
                1000
            ),
            Decision::Failed(_)
        ));
        // anonymous in the list: no challenge
        let anon = Allowed::from_config(Some(&["anonymous".into(), "alice".into()]), true);
        assert_eq!(
            a.check_at(&anon, "DESCRIBE", "rtsp://h/argus/main", None, IP, 1000),
            Decision::Ok(None)
        );
    }

    #[test]
    fn test_digest() {
        let a = auth();
        let all = Allowed::Everyone;
        let nonce = a.nonce_at(1000, IP);
        let ok =
            |h: &str, now| a.check_at(&all, "DESCRIBE", "rtsp://h/argus/main", Some(h), IP, now);
        // qop=auth, then a replayed nc is refused, a higher one accepted
        let h1 = digest_header(&a, "alice", "alicepw", &nonce, Some(1));
        assert_eq!(ok(&h1, 1001), Decision::Ok(Some("alice".into())));
        assert!(matches!(ok(&h1, 1002), Decision::Failed(_)));
        assert_eq!(
            ok(
                &digest_header(&a, "alice", "alicepw", &nonce, Some(2)),
                1003
            ),
            Decision::Ok(Some("alice".into()))
        );
        // RFC 2069 (no qop), as go2rtc sends
        assert_eq!(
            ok(&digest_header(&a, "bob", "bobpw", &nonce, None), 1004),
            Decision::Ok(Some("bob".into()))
        );
        // Wrong password
        assert!(matches!(
            ok(&digest_header(&a, "bob", "nope", &nonce, None), 1004),
            Decision::Failed(_)
        ));
        // After 5 minutes: stale challenge, not a failure
        match ok(&digest_header(&a, "bob", "bobpw", &nonce, None), 1000 + 301) {
            Decision::Challenge(c) => assert!(c[0].contains("stale=TRUE")),
            d => panic!("{d:?}"),
        }
        // The nonce is bound to the client IP
        let other: IpAddr = "10.0.0.6".parse().unwrap();
        assert!(matches!(
            a.check_at(
                &all,
                "DESCRIBE",
                "rtsp://h/argus/main",
                Some(&digest_header(&a, "bob", "bobpw", &nonce, None)),
                other,
                1004
            ),
            Decision::Failed(_)
        ));
        // A forged nonce
        assert!(matches!(
            ok(
                &digest_header(&a, "bob", "bobpw", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", None),
                1004
            ),
            Decision::Failed(_)
        ));
    }

    /// Security review #3, #4: a Digest response only works for the mount it names, and two
    /// clients behind one IP get different nonces in the same second
    #[test]
    fn test_digest_uri_binding_and_salted_nonce() {
        let a = auth();
        let all = Allowed::Everyone;
        assert_ne!(a.nonce_at(1000, IP), a.nonce_at(1000, IP));
        let nonce = a.nonce_at(1000, IP);
        let h = digest_header(&a, "bob", "bobpw", &nonce, None);
        let at = |uri: &str| a.check_at(&all, "DESCRIBE", uri, Some(&h), IP, 1001);
        assert_eq!(at("rtsp://h/argus/main"), Decision::Ok(Some("bob".into())));
        // Same mount, other spelling or a track URI: fine
        assert_eq!(at("rtsp://h/argus/main/"), Decision::Ok(Some("bob".into())));
        assert_eq!(
            at("rtsp://h/argus/main/stream=1"),
            Decision::Ok(Some("bob".into()))
        );
        // Another mount: refused
        assert!(matches!(at("rtsp://h/argus/sub"), Decision::Failed(_)));
        assert!(matches!(at("rtsp://h/other/main"), Decision::Failed(_)));
    }

    #[test]
    fn test_schemes_filter() {
        let a = Auth::new(
            vec![User {
                name: "u".into(),
                pass: Some("p".into()),
            }],
            Schemes::Digest,
        );
        match a.check_at(
            &Allowed::Everyone,
            "DESCRIBE",
            "rtsp://h/argus/main",
            None,
            IP,
            0,
        ) {
            Decision::Challenge(c) => assert_eq!(c.len(), 1),
            d => panic!("{d:?}"),
        }
        assert!(matches!(
            a.check_at(
                &Allowed::Everyone,
                "DESCRIBE",
                "rtsp://h/argus/main",
                Some(&basic("u:p")),
                IP,
                0
            ),
            Decision::Failed(_)
        ));
    }
}
