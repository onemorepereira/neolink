//! Tolerant Transport, Session and Authorization parsing (DESIGN §6.1, SPEC R4)

use std::collections::HashMap;

/// A transport the server accepted: TCP interleaved (SPEC R4)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpTransport {
    /// Requested interleaved channels (RTP, RTCP), if any
    pub interleaved: Option<(u8, u8)>,
}

/// Why no transport was accepted
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    /// No TCP spec offered: 461 (ffmpeg and live555 then retry over TCP)
    Unsupported,
    /// Unparsable: 400
    Malformed,
}

/// Choose the first acceptable spec from a Transport header: `RTP/AVP/TCP`, unicast,
/// play mode. Multicast or record specs are skipped.
pub fn choose_transport(header: &str) -> Result<TcpTransport, TransportError> {
    let mut saw_any = false;
    for spec in header.split(',') {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        saw_any = true;
        let mut parts = spec.split(';').map(|p| p.trim());
        let proto = parts.next().unwrap_or("").to_ascii_uppercase();
        if proto != "RTP/AVP/TCP" {
            continue;
        }
        let mut interleaved = None;
        let mut ok = true;
        for p in parts {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            match k.to_ascii_lowercase().as_str() {
                "multicast" => ok = false,
                "mode" => {
                    let v = v.trim_matches('"').to_ascii_uppercase();
                    if !v.split(',').any(|m| m.trim() == "PLAY") {
                        ok = false;
                    }
                }
                "interleaved" => {
                    // An RTP/RTCP pair: n, n+1 with n even (R23: no duplicates)
                    let mut ch = v.split('-').map(|c| c.trim().parse::<u8>());
                    interleaved = match (ch.next(), ch.next(), ch.next()) {
                        (Some(Ok(a)), Some(Ok(b)), None)
                            if a % 2 == 0 && b == a.wrapping_add(1) =>
                        {
                            Some((a, b))
                        }
                        (Some(Ok(a)), None, None) if a % 2 == 0 && a < 255 => Some((a, a + 1)),
                        _ => return Err(TransportError::Malformed),
                    };
                }
                _ => {}
            }
        }
        if ok {
            return Ok(TcpTransport { interleaved });
        }
    }
    if saw_any {
        Err(TransportError::Unsupported)
    } else {
        Err(TransportError::Malformed)
    }
}

/// The Transport header echoed for an accepted TCP transport (as GStreamer does)
pub fn transport_reply(channels: (u8, u8), ssrc: u32) -> String {
    format!(
        "RTP/AVP/TCP;unicast;interleaved={}-{};ssrc={:08X};mode=\"PLAY\"",
        channels.0, channels.1, ssrc
    )
}

/// The session id from a Session header (`id[;timeout=n]`)
pub fn session_id(header: &str) -> &str {
    header.split(';').next().unwrap_or("").trim()
}

/// Credentials from an Authorization header
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorization {
    Basic { user: String, pass: String },
    Digest(HashMap<String, String>),
}

/// Parse an Authorization header; `None` if it is neither Basic nor Digest
pub fn authorization(header: &str) -> Option<Authorization> {
    let header = header.trim();
    let (scheme, rest) = header
        .split_once(char::is_whitespace)
        .unwrap_or((header, ""));
    match scheme.to_ascii_lowercase().as_str() {
        "basic" => {
            use base64::Engine;
            let raw = base64::engine::general_purpose::STANDARD
                .decode(rest.trim())
                .ok()?;
            let s = String::from_utf8(raw).ok()?;
            let (user, pass) = s.split_once(':')?;
            Some(Authorization::Basic {
                user: user.to_string(),
                pass: pass.to_string(),
            })
        }
        "digest" => Some(Authorization::Digest(params(rest))),
        _ => None,
    }
}

/// `k=v, k="v, with comma"` parameters, keys lowercased
pub fn params(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq]
            .trim()
            .trim_start_matches(',')
            .trim()
            .to_ascii_lowercase();
        rest = rest[eq + 1..].trim_start();
        let value;
        if let Some(stripped) = rest.strip_prefix('"') {
            // Quoted, with \" escapes
            let mut v = String::new();
            let mut chars = stripped.char_indices();
            let mut end = stripped.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            v.push(n);
                        }
                    }
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    _ => v.push(c),
                }
            }
            value = v;
            rest = stripped[end.min(stripped.len())..].trim_start();
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            value = rest[..end].trim().to_string();
            rest = &rest[end..];
        }
        rest = rest.trim_start_matches(',').trim_start();
        if !key.is_empty() {
            out.insert(key, value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_choose_transport() {
        // ffmpeg's default (UDP) only: 461
        assert_eq!(
            choose_transport("RTP/AVP/UDP;unicast;client_port=16866-16867"),
            Err(TransportError::Unsupported)
        );
        assert_eq!(
            choose_transport("RTP/AVP/TCP;unicast;interleaved=0-1"),
            Ok(TcpTransport {
                interleaved: Some((0, 1))
            })
        );
        // The first acceptable of several; case-insensitive
        assert_eq!(
            choose_transport(
                "RTP/AVP;unicast;client_port=1-2, rtp/avp/tcp;UNICAST;interleaved=2-3"
            ),
            Ok(TcpTransport {
                interleaved: Some((2, 3))
            })
        );
        assert_eq!(
            choose_transport("RTP/AVP/TCP"),
            Ok(TcpTransport { interleaved: None })
        );
        assert_eq!(
            choose_transport("RTP/AVP/TCP;multicast"),
            Err(TransportError::Unsupported)
        );
        assert_eq!(
            choose_transport("RTP/AVP/TCP;mode=\"RECORD\""),
            Err(TransportError::Unsupported)
        );
        assert_eq!(
            choose_transport("RTP/AVP/TCP;interleaved=x"),
            Err(TransportError::Malformed)
        );
        assert_eq!(choose_transport(""), Err(TransportError::Malformed));
        for bad in ["5-5", "0-200", "255", "1-2", "0-1-2"] {
            assert_eq!(
                choose_transport(&format!("RTP/AVP/TCP;interleaved={bad}")),
                Err(TransportError::Malformed),
                "{bad}"
            );
        }
        assert_eq!(
            choose_transport("RTP/AVP/TCP;interleaved=4"),
            Ok(TcpTransport {
                interleaved: Some((4, 5))
            })
        );
        assert_eq!(
            transport_reply((0, 1), 0x1a2b),
            "RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=00001A2B;mode=\"PLAY\""
        );
    }

    #[test]
    fn test_session_and_auth() {
        assert_eq!(session_id("abc123;timeout=60"), "abc123");
        assert_eq!(
            authorization("Basic YWxpY2U6YWxpY2Vwdw=="),
            Some(Authorization::Basic {
                user: "alice".into(),
                pass: "alicepw".into()
            })
        );
        match authorization(
            r#"Digest username="alice", realm="neolink", nonce="abc", uri="rtsp://h/x", response="ff", qop=auth, nc=00000001, cnonce="c,d""#,
        ) {
            Some(Authorization::Digest(p)) => {
                assert_eq!(p["username"], "alice");
                assert_eq!(p["qop"], "auth");
                assert_eq!(p["nc"], "00000001");
                assert_eq!(p["cnonce"], "c,d");
                assert_eq!(p["uri"], "rtsp://h/x");
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(authorization("Bearer x"), None);
        assert_eq!(authorization("Basic !!!"), None);
    }
}
