//! Golden RTSP transcripts: record, normalise, structurally diff
//!
//! **Recording.** [`record`] is a TCP proxy between an RTSP client and server. It writes
//! every RTSP message in both directions, and one line per new (channel, payload type)
//! of interleaved data, plus per-channel packet counts when the connection closes.
//!
//! ```text
//! # conn 1
//! > OPTIONS rtsp://127.0.0.1:8555/argus/main RTSP/1.0
//! > CSeq: 1
//! >
//! < RTSP/1.0 200 OK
//! < Content-Type: application/sdp
//! <
//! <| v=0                       (body lines)
//! < $0 rtp pt=96               (first interleaved packet of that kind)
//! # conn 1 closed: < $0 x812, < $1 x9
//! ```
//!
//! **Normalising** ([`normalize`]) replaces values that change between two recordings of
//! the same server and camera (host:port, session ids, ssrc, seq, rtptime, ports, nonces,
//! dates, SDP origin, npt start, packet counts) so stored transcripts are stable.
//!
//! **Structural diff** ([`structural_diff`]) compares what a client depends on: methods,
//! request paths, status codes, header presence, the shape of Transport, Session, Public,
//! RTP-Info, Range and WWW-Authenticate, SDP attribute sets, rtpmap and fmtp keys, and the
//! interleaved channel/payload-type set. It ignores values that depend on the source
//! (sprop, profile-level-id, resolution) and volatile ones.

use anyhow::{bail, Context, Result};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// Direction of a message
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Dir {
    /// Client to server
    Up,
    /// Server to client
    Down,
}

impl Dir {
    fn mark(self) -> char {
        match self {
            Dir::Up => '>',
            Dir::Down => '<',
        }
    }
}

/// One RTSP request or response
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub dir: Dir,
    pub start: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<String>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn is_response(&self) -> bool {
        self.start.starts_with("RTSP/")
    }
}

/// A transcript entry
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// `# conn N`
    Conn,
    Msg(Message),
    /// First interleaved packet of its kind on this connection
    Interleaved {
        dir: Dir,
        channel: u8,
        desc: String,
    },
    /// Comment lines, including the close summary
    Comment(String),
}

// ---------------------------------------------------------------------------------------
// Recording proxy

/// Incremental RTSP/interleaved parser for one direction
struct Splitter {
    dir: Dir,
    buf: Vec<u8>,
    seen: BTreeSet<(u8, String)>,
    counts: BTreeMap<u8, u64>,
}

impl Splitter {
    fn new(dir: Dir) -> Self {
        Splitter {
            dir,
            buf: vec![],
            seen: BTreeSet::new(),
            counts: BTreeMap::new(),
        }
    }

    /// Feed bytes, return transcript text for every complete unit
    fn feed(&mut self, data: &[u8]) -> String {
        self.buf.extend_from_slice(data);
        let mut out = String::new();
        loop {
            if self.buf.first() == Some(&b'$') {
                if self.buf.len() < 4 {
                    break;
                }
                let channel = self.buf[1];
                let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                if self.buf.len() < 4 + len {
                    break;
                }
                let desc = describe_packet(&self.buf[4..4 + len]);
                *self.counts.entry(channel).or_default() += 1;
                if self.seen.insert((channel, desc.clone())) {
                    let _ = writeln!(out, "{} ${} {}", self.dir.mark(), channel, desc);
                }
                self.buf.drain(..4 + len);
                continue;
            }
            let Some(end) = find(&self.buf, b"\r\n\r\n") else {
                break;
            };
            let head = String::from_utf8_lossy(&self.buf[..end]).to_string();
            let content_length = head
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if self.buf.len() < end + 4 + content_length {
                break;
            }
            let body =
                String::from_utf8_lossy(&self.buf[end + 4..end + 4 + content_length]).to_string();
            let m = self.dir.mark();
            for line in head.split("\r\n") {
                let _ = writeln!(out, "{} {}", m, line);
            }
            let _ = writeln!(out, "{}", m);
            for line in body.lines() {
                let _ = writeln!(out, "{}| {}", m, line);
            }
            self.buf.drain(..end + 4 + content_length);
        }
        out
    }

    fn summary(&self) -> String {
        self.counts
            .iter()
            .map(|(ch, n)| format!("{} ${} x{}", self.dir.mark(), ch, n))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn describe_packet(p: &[u8]) -> String {
    if p.len() < 2 || p[0] >> 6 != 2 {
        return "data".to_string();
    }
    let pt = p[1] & 0x7f;
    if (72..=76).contains(&pt) {
        // RTCP: the full second byte is the packet type (200..=204)
        format!("rtcp pt={}", p[1])
    } else {
        format!("rtp pt={}", pt)
    }
}

/// Run the recording proxy until `max_conns` connections have closed
pub fn record(listen: &str, upstream: &str, out: &std::path::Path, max_conns: usize) -> Result<()> {
    let listener = TcpListener::bind(listen).with_context(|| format!("binding {}", listen))?;
    let file = Arc::new(Mutex::new(std::fs::File::create(out)?));
    eprintln!(
        "golden proxy {} -> {}, writing {}",
        listen,
        upstream,
        out.display()
    );
    let mut handles = vec![];
    for (n, client) in listener.incoming().enumerate().take(max_conns) {
        let client = client?;
        let server =
            TcpStream::connect(upstream).with_context(|| format!("connecting {}", upstream))?;
        let conn = n + 1;
        writeln!(file.lock().unwrap(), "# conn {}", conn)?;
        let pumps: Vec<_> = [
            (Dir::Up, client.try_clone()?, server.try_clone()?),
            (Dir::Down, server, client),
        ]
        .into_iter()
        .map(|(dir, mut from, mut to)| {
            let file = file.clone();
            std::thread::spawn(move || {
                let mut split = Splitter::new(dir);
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let n = match from.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let text = split.feed(&buf[..n]);
                    if !text.is_empty() {
                        let _ = file.lock().unwrap().write_all(text.as_bytes());
                    }
                    if to.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = to.shutdown(Shutdown::Write);
                split.summary()
            })
        })
        .collect();
        let file = file.clone();
        handles.push(std::thread::spawn(move || {
            let sums: Vec<String> = pumps.into_iter().map(|h| h.join().unwrap()).collect();
            let sums: Vec<String> = sums.into_iter().filter(|s| !s.is_empty()).collect();
            let _ = writeln!(
                file.lock().unwrap(),
                "# conn {} closed: {}",
                conn,
                sums.join(", ")
            );
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Parsing

pub fn parse(text: &str) -> Result<Vec<Entry>> {
    let mut out = vec![];
    let mut cur: Option<Message> = None;
    let mut in_body = false;
    let flush = |cur: &mut Option<Message>, out: &mut Vec<Entry>| {
        if let Some(m) = cur.take() {
            out.push(Entry::Msg(m));
        }
    };
    for (n, line) in text.lines().enumerate() {
        if let Some(rest) = line.strip_prefix('#') {
            flush(&mut cur, &mut out);
            let rest = rest.trim();
            if rest.starts_with("conn ") && !rest.contains("closed") {
                out.push(Entry::Conn);
            } else {
                out.push(Entry::Comment(rest.to_string()));
            }
            continue;
        }
        let dir = match line.chars().next() {
            Some('>') => Dir::Up,
            Some('<') => Dir::Down,
            _ if line.trim().is_empty() => continue,
            _ => bail!("line {}: no direction mark: {}", n + 1, line),
        };
        let rest = &line[1..];
        if let Some(body) = rest.strip_prefix("| ").or_else(|| rest.strip_prefix('|')) {
            match cur.as_mut() {
                Some(m) if in_body && m.dir == dir => m.body.push(body.to_string()),
                _ => bail!("line {}: body line outside a message", n + 1),
            }
            continue;
        }
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        if let Some(il) = rest.strip_prefix('$') {
            flush(&mut cur, &mut out);
            let (ch, desc) = il.split_once(' ').unwrap_or((il, ""));
            out.push(Entry::Interleaved {
                dir,
                channel: ch.parse().with_context(|| format!("line {}", n + 1))?,
                desc: desc.to_string(),
            });
            continue;
        }
        match cur.as_mut() {
            Some(m) if m.dir == dir && !in_body => {
                if rest.is_empty() {
                    in_body = true;
                } else {
                    let (k, v) = rest
                        .split_once(':')
                        .with_context(|| format!("line {}: bad header", n + 1))?;
                    m.headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            _ => {
                flush(&mut cur, &mut out);
                in_body = false;
                cur = Some(Message {
                    dir,
                    start: rest.to_string(),
                    headers: vec![],
                    body: vec![],
                });
            }
        }
    }
    flush(&mut cur, &mut out);
    Ok(out)
}

pub fn render(entries: &[Entry]) -> String {
    let mut out = String::new();
    let mut conn = 0;
    for e in entries {
        match e {
            Entry::Conn => {
                conn += 1;
                let _ = writeln!(out, "# conn {}", conn);
            }
            Entry::Comment(c) => {
                let _ = writeln!(out, "# {}", c);
            }
            Entry::Interleaved { dir, channel, desc } => {
                let _ = writeln!(out, "{} ${} {}", dir.mark(), channel, desc);
            }
            Entry::Msg(m) => {
                let mk = m.dir.mark();
                let _ = writeln!(out, "{} {}", mk, m.start);
                for (k, v) in &m.headers {
                    let _ = writeln!(out, "{} {}: {}", mk, k, v);
                }
                let _ = writeln!(out, "{}", mk);
                for b in &m.body {
                    let _ = writeln!(out, "{}| {}", mk, b);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Normalising

struct Rules(Vec<(Regex, &'static str)>);

impl Rules {
    fn apply(&self, s: &str) -> String {
        let mut s = s.to_string();
        for (re, rep) in &self.0 {
            s = re.replace_all(&s, *rep).to_string();
        }
        s
    }
}

fn rules(list: &[(&str, &'static str)]) -> Rules {
    Rules(
        list.iter()
            .map(|(re, rep)| (Regex::new(re).unwrap(), *rep))
            .collect(),
    )
}

/// Replace values that differ between two recordings of the same server and camera
pub fn normalize(entries: &[Entry]) -> Vec<Entry> {
    let everywhere = rules(&[(r"rtsp://[^/\s;,]+", "rtsp://HOST")]);
    let headers = rules(&[
        (r"(?i)\bssrc=[0-9a-f]+", "ssrc=SSRC"),
        (
            r"(?i)\b(client_port|server_port|port)=\d+(-\d+)?",
            "$1=PORT",
        ),
        (r"(?i)\bseq=\d+", "seq=N"),
        (r"(?i)\brtptime=\d+", "rtptime=N"),
        (
            r#"(?i)\b(nonce|opaque|response|cnonce)="[^"]*""#,
            "$1=\"X\"",
        ),
        (r"(?i)\bnc=[0-9a-f]+", "nc=N"),
        (r"(?i)^Basic \S+$", "Basic X"),
        (r"npt=[0-9.]+-", "npt=T-"),
    ]);
    let session = Regex::new(r"^[^;\s]+").unwrap();
    let user_agent = Regex::new(r"(?i)^(\D+?)[\s/]?\d[\w.\-]*").unwrap();
    let sdp = rules(&[
        (
            r"^o=(\S+) \S+ \S+ (\S+) (\S+) \S+",
            "o=$1 ID VER $2 $3 ADDR",
        ),
        (r"^a=range:npt=[0-9.]+-", "a=range:npt=T-"),
    ]);
    let counts = Regex::new(r"x\d+").unwrap();
    let mut out = vec![];
    for e in entries {
        out.push(match e {
            Entry::Msg(m) => {
                let mut m = m.clone();
                m.start = everywhere.apply(&m.start);
                for (k, v) in m.headers.iter_mut() {
                    let lk = k.to_ascii_lowercase();
                    *v = match lk.as_str() {
                        "date" | "expires" => "DATE".to_string(),
                        "content-length" => "N".to_string(),
                        "session" => session.replace(v, "SESSION").to_string(),
                        "user-agent" => user_agent.replace(v, "$1").to_string(),
                        _ => headers.apply(&everywhere.apply(v)),
                    };
                }
                m.body = m
                    .body
                    .iter()
                    .map(|l| sdp.apply(&everywhere.apply(l)))
                    .collect();
                Entry::Msg(m)
            }
            Entry::Comment(c) if c.contains("closed:") => {
                Entry::Comment(counts.replace_all(c, "xN").to_string())
            }
            other => other.clone(),
        });
    }
    out
}

// ---------------------------------------------------------------------------------------
// Structural diff

/// Headers whose presence is not compared
const IGNORED_HEADERS: &[&str] = &["date", "expires", "server", "user-agent", "cache-control"];

/// The structure of one message that clients depend on
fn shape(m: &Message) -> BTreeMap<String, String> {
    let mut s = BTreeMap::new();
    let path = |url: &str| -> String {
        url.split_once("://")
            .map(|(_, rest)| {
                rest.find('/')
                    .map(|i| rest[i..].to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_else(|| url.to_string())
    };
    if m.is_response() {
        let code = m.start.split_whitespace().nth(1).unwrap_or("").to_string();
        s.insert("status".into(), code);
    } else {
        let mut parts = m.start.split_whitespace();
        s.insert("method".into(), parts.next().unwrap_or("").to_string());
        s.insert("path".into(), path(parts.next().unwrap_or("")));
    }
    let names: BTreeSet<String> = m
        .headers
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .filter(|k| !IGNORED_HEADERS.contains(&k.as_str()))
        .collect();
    s.insert("headers".into(), join(names));
    for (k, v) in &m.headers {
        let k = k.to_ascii_lowercase();
        match k.as_str() {
            "transport" => {
                let mut items = vec![v.split(';').next().unwrap_or("").to_string()];
                for p in v.split(';').skip(1) {
                    let p = p.trim();
                    match p.split_once('=') {
                        Some((pk, pv)) if pk.eq_ignore_ascii_case("interleaved") => {
                            items.push(format!("{}={}", pk, pv))
                        }
                        Some((pk, pv)) if pk.eq_ignore_ascii_case("mode") => items.push(format!(
                            "{}={}",
                            pk,
                            pv.to_ascii_uppercase().replace('"', "")
                        )),
                        Some((pk, _)) => items.push(pk.to_string()),
                        None => items.push(p.to_string()),
                    }
                }
                items[1..].sort();
                s.insert("transport".into(), items.join(";"));
            }
            "session" => {
                let timeout = v
                    .split(';')
                    .skip(1)
                    .find_map(|p| p.trim().strip_prefix("timeout="))
                    .unwrap_or("-");
                s.insert("session".into(), format!("timeout={}", timeout));
            }
            "public" => {
                let set: BTreeSet<String> = v.split(',').map(|x| x.trim().to_string()).collect();
                s.insert("public".into(), join(set));
            }
            "content-type" | "content-base" | "require" | "unsupported" => {
                let v = if k == "content-base" {
                    path(v)
                } else {
                    v.clone()
                };
                s.insert(k, v);
            }
            "rtp-info" => {
                let entries: Vec<String> = v
                    .split(',')
                    .map(|e| {
                        let mut keys: Vec<String> = e
                            .split(';')
                            .map(|p| {
                                let (pk, pv) = p.trim().split_once('=').unwrap_or((p.trim(), ""));
                                if pk == "url" {
                                    format!("url={}", path(pv))
                                } else {
                                    pk.to_string()
                                }
                            })
                            .collect();
                        keys.sort();
                        keys.join(";")
                    })
                    .collect();
                s.insert("rtp-info".into(), entries.join(", "));
            }
            "range" => {
                let unit = v.split('=').next().unwrap_or("").to_string();
                let open = v.ends_with('-');
                s.insert("range".into(), format!("{}= open={}", unit, open));
            }
            "www-authenticate" => {
                let scheme = v.split_whitespace().next().unwrap_or("").to_string();
                let keys: BTreeSet<String> = v[scheme.len()..]
                    .split(',')
                    .filter_map(|p| {
                        p.split_once('=')
                            .map(|(pk, _)| pk.trim().to_ascii_lowercase())
                    })
                    .collect();
                let e = s.entry("www-authenticate".into()).or_default();
                if !e.is_empty() {
                    e.push_str(" | ");
                }
                let _ = write!(e, "{} {}", scheme, join(keys));
            }
            "accept-ranges" | "x-accept-dynamic-rate" => {
                s.insert(k, v.clone());
            }
            _ => {}
        }
    }
    if m.header("content-type")
        .map(|c| c.contains("sdp"))
        .unwrap_or(false)
    {
        sdp_shape(&m.body, &mut s);
    }
    s
}

fn sdp_shape(body: &[String], s: &mut BTreeMap<String, String>) {
    let mut section = "sdp.session".to_string();
    let mut attrs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut media_n = 0;
    for line in body {
        let Some((t, v)) = line.split_once('=') else {
            continue;
        };
        match t {
            "m" => {
                let f: Vec<&str> = v.split_whitespace().collect();
                section = format!("sdp.m{}", media_n);
                media_n += 1;
                s.insert(
                    format!("{}.m", section),
                    format!(
                        "{} {} {}",
                        f.first().unwrap_or(&""),
                        f.get(2).unwrap_or(&""),
                        f[3.min(f.len())..].join(" ")
                    ),
                );
            }
            "a" => {
                let (name, val) = v.split_once(':').unwrap_or((v, ""));
                attrs
                    .entry(section.clone())
                    .or_default()
                    .insert(name.to_string());
                match name {
                    "control" => {
                        s.insert(format!("{}.control", section), val.to_string());
                    }
                    "rtpmap" => {
                        // Encoding name and clock rate; channels depend on the source
                        let enc = val.split_whitespace().nth(1).unwrap_or("");
                        let mut p = enc.split('/');
                        let name = p.next().unwrap_or("").to_ascii_uppercase();
                        let rate = p.next().unwrap_or("");
                        s.insert(format!("{}.rtpmap", section), format!("{}/{}", name, rate));
                    }
                    "fmtp" => {
                        let keys: BTreeSet<String> = val
                            .split_once(' ')
                            .map(|(_, p)| p)
                            .unwrap_or("")
                            .split(';')
                            .filter_map(|kv| kv.split('=').next())
                            .map(|k| k.trim().to_ascii_lowercase())
                            .filter(|k| !k.is_empty())
                            .collect();
                        s.insert(format!("{}.fmtp", section), join(keys));
                    }
                    _ => {}
                }
            }
            "v" | "o" | "s" | "c" | "t" | "b" => {
                attrs
                    .entry(section.clone())
                    .or_default()
                    .insert(format!("{}=", t));
            }
            _ => {}
        }
    }
    for (sec, set) in attrs {
        s.insert(format!("{}.lines", sec), join(set));
    }
}

fn join<I: IntoIterator<Item = String>>(i: I) -> String {
    i.into_iter().collect::<Vec<_>>().join(",")
}

/// Structural differences between two transcripts, empty when they match
pub fn structural_diff(a: &[Entry], b: &[Entry]) -> Vec<String> {
    let msgs = |e: &[Entry]| -> Vec<Message> {
        e.iter()
            .filter_map(|e| match e {
                Entry::Msg(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    };
    let interleaved = |e: &[Entry]| -> BTreeSet<String> {
        e.iter()
            .filter_map(|e| match e {
                Entry::Interleaved { dir, channel, desc } => {
                    Some(format!("{} ${} {}", dir.mark(), channel, desc))
                }
                _ => None,
            })
            .collect()
    };
    let conns = |e: &[Entry]| e.iter().filter(|e| matches!(e, Entry::Conn)).count();
    let mut out = vec![];
    if conns(a) != conns(b) {
        out.push(format!("connections: {} vs {}", conns(a), conns(b)));
    }
    let (ma, mb) = (msgs(a), msgs(b));
    for i in 0..ma.len().max(mb.len()) {
        let label = |m: Option<&Message>| {
            m.map(|m| format!("{} {}", m.dir.mark(), m.start))
                .unwrap_or_else(|| "(none)".into())
        };
        match (ma.get(i), mb.get(i)) {
            (Some(x), Some(y)) if x.dir == y.dir => {
                let (sx, sy) = (shape(x), shape(y));
                for k in sx.keys().chain(sy.keys()).collect::<BTreeSet<_>>() {
                    let (vx, vy) = (sx.get(k), sy.get(k));
                    if vx != vy {
                        out.push(format!(
                            "msg {} ({}): {}: {} vs {}",
                            i + 1,
                            label(Some(x)),
                            k,
                            vx.map(|s| s.as_str()).unwrap_or("(absent)"),
                            vy.map(|s| s.as_str()).unwrap_or("(absent)")
                        ));
                    }
                }
            }
            (x, y) => out.push(format!("msg {}: {} vs {}", i + 1, label(x), label(y))),
        }
    }
    let (ia, ib) = (interleaved(a), interleaved(b));
    for x in ia.difference(&ib) {
        out.push(format!("interleaved only in first: {}", x));
    }
    for x in ib.difference(&ia) {
        out.push(format!("interleaved only in second: {}", x));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(dir: Dir, data: &[u8]) -> String {
        // Feed byte by byte to exercise partial reads
        let mut s = Splitter::new(dir);
        data.iter().map(|b| s.feed(&[*b])).collect()
    }

    const SDP_REPLY: &str = "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\nContent-Base: rtsp://127.0.0.1:8554/argus/main/\r\nContent-Length: 55\r\n\r\nv=0\r\no=- 123 1 IN IP4 127.0.0.1\r\nm=video";

    #[test]
    fn test_splitter() {
        let mut data = SDP_REPLY.as_bytes().to_vec();
        data.extend_from_slice(b" 0 RTP/AVP 96\r\n"); // completes the 55-byte body
        data.extend_from_slice(&[b'$', 0, 0, 2, 0x80, 96, b'$', 0, 0, 2, 0x80, 96]);
        data.extend_from_slice(&[b'$', 1, 0, 2, 0x80, 200]);
        let text = feed_all(Dir::Down, &data);
        assert_eq!(
            text,
            "< RTSP/1.0 200 OK\n< CSeq: 2\n< Content-Type: application/sdp\n\
             < Content-Base: rtsp://127.0.0.1:8554/argus/main/\n< Content-Length: 55\n<\n\
             <| v=0\n<| o=- 123 1 IN IP4 127.0.0.1\n<| m=video 0 RTP/AVP 96\n\
             < $0 rtp pt=96\n< $1 rtcp pt=200\n"
        );
        let entries = parse(&text).unwrap();
        assert_eq!(render(&entries), text);
    }

    const A: &str = "# conn 1
> DESCRIBE rtsp://127.0.0.1:8555/argus/main RTSP/1.0
> CSeq: 2
> User-Agent: Lavf62.12.103
>
< RTSP/1.0 200 OK
< CSeq: 2
< Date: Sat, 10 Oct 2026 19:24:01 GMT
< Content-Type: application/sdp
< Content-Base: rtsp://127.0.0.1:8555/argus/main/
< Content-Length: 300
<
<| v=0
<| o=- 1188340656180883 1 IN IP4 127.0.0.1
<| a=control:*
<| m=video 0 RTP/AVP 96
<| a=rtpmap:96 H264/90000
<| a=fmtp:96 packetization-mode=1;sprop-parameter-sets=Z0,aO;profile-level-id=640029
<| a=control:stream=0
> SETUP rtsp://127.0.0.1:8555/argus/main/stream=0 RTSP/1.0
> CSeq: 3
> Transport: RTP/AVP/TCP;unicast;interleaved=0-1
>
< RTSP/1.0 200 OK
< CSeq: 3
< Transport: RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=1A2B3C4D;mode=\"PLAY\"
< Session: OOSjWAnLMabUwtP2;timeout=30
<
< $0 rtp pt=96
# conn 1 closed: < $0 x812
";

    #[test]
    fn test_normalize() {
        let n = render(&normalize(&parse(A).unwrap()));
        assert!(n.contains("> DESCRIBE rtsp://HOST/argus/main RTSP/1.0"));
        assert!(n.contains("> User-Agent: Lavf\n"));
        assert!(n.contains("< Date: DATE\n"));
        assert!(n.contains("<| o=- ID VER IN IP4 ADDR\n"));
        assert!(n.contains("ssrc=SSRC;mode"));
        assert!(n.contains("< Session: SESSION;timeout=30\n"));
        assert!(n.contains("# conn 1 closed: < $0 xN"));
        // Source-dependent values are kept in the stored file
        assert!(n.contains("sprop-parameter-sets=Z0,aO"));
        // Idempotent
        assert_eq!(render(&normalize(&parse(&n).unwrap())), n);
    }

    #[test]
    fn test_diff_ignores_volatile_and_source_values() {
        let b = A
            .replace("8555", "9999")
            .replace("OOSjWAnLMabUwtP2", "zzzzzzzzzzzzzzzz")
            .replace("1A2B3C4D", "FFFF0000")
            .replace("Z0,aO", "QQ,RR")
            .replace("640029", "42e01f")
            .replace("Sat, 10 Oct", "Sun, 11 Oct")
            .replace("x812", "x5");
        let d = structural_diff(&parse(A).unwrap(), &parse(&b).unwrap());
        assert!(d.is_empty(), "{:?}", d);
    }

    #[test]
    fn test_diff_finds_structure() {
        let cases = [
            ("profile-level-id=640029", "level=640029", "sdp.m0.fmtp"),
            ("a=control:*\n", "", "sdp.session"),
            ("main/\n", "main\n", "content-base"),
            (";timeout=30", "", "session"),
            (
                "RTSP/1.0 200 OK\n< CSeq: 3",
                "RTSP/1.0 461 Unsupported transport\n< CSeq: 3",
                "status",
            ),
            ("H264/90000", "H265/90000", "rtpmap"),
            ("< $0 rtp pt=96", "< $0 rtp pt=97", "interleaved"),
            (";mode=\"PLAY\"", "", "transport"),
        ];
        for (from, to, expect) in cases {
            let b = A.replacen(from, to, 1);
            let d = structural_diff(&parse(A).unwrap(), &parse(&b).unwrap());
            assert!(
                d.iter().any(|l| l.contains(expect)),
                "{} -> {}: {:?}",
                from,
                to,
                d
            );
        }
    }
}
