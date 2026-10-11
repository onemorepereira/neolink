//! Cached snapshot endpoint (SPEC P5)
//!
//! `GET /{camera}/snapshot.jpg` on `snapshot_port` (default `bind_port + 1`, 0 disables)
//! serves the JPEG taken during the camera's last wake, from its state file. It never
//! wakes the camera: that is what lets Home Assistant refresh a thumbnail every few
//! seconds without draining the battery. Until a first wake it serves a grey placeholder
//! (HTTP 200) so that HA's config flow accepts the URL.
//!
//! `?fresh=1` takes a new snapshot, which wakes the camera. It is off unless the camera
//! sets `snapshot_fresh = true`, and limited to once a minute per camera.
//!
//! Auth follows the RTSP rules: with no `[[users]]` anyone may fetch; otherwise HTTP
//! Basic, and the user must be in the camera's `permitted_users` when that is set.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::common::{unix_now, NeoReactor, StateFile};
use crate::config::{CameraConfig, Config};

const PLACEHOLDER: &[u8] = include_bytes!("placeholder.jpg");
const MAX_REQUEST: usize = 8 * 1024;
const MAX_CONNECTIONS: usize = 32;
/// For reading the request headers (slowloris)
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// For the whole request: a `?fresh=1` wakes the camera over UDP, which can take 15 s+
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(75);
const FRESH_INTERVAL: Duration = Duration::from_secs(60);

/// The port to serve on, `None` when disabled
pub(crate) fn port(config: &Config) -> Option<u16> {
    match config.snapshot_port {
        Some(0) => None,
        Some(port) => Some(port),
        None => config.bind_port.checked_add(1),
    }
}

/// Serve until cancelled
pub(crate) async fn serve(reactor: NeoReactor, cancel: CancellationToken) -> Result<()> {
    let config = reactor.config().await?.borrow().clone();
    let Some(port) = port(&config) else {
        return Ok(());
    };
    let addr = format!("{}:{}", config.bind_addr, port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding the snapshot endpoint on {addr}"))?;
    log::info!("Snapshots available at http://{addr}/<camera>/snapshot.jpg");
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let fresh_at: Arc<Mutex<HashMap<String, Instant>>> = Default::default();
    loop {
        let (stream, _) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            v = listener.accept() => match v {
                Ok(v) => v,
                Err(e) => {
                    log::debug!("snapshot accept: {e}");
                    continue;
                }
            },
        };
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            // Over the limit: say so instead of silently dropping
            tokio::spawn(async move {
                let mut stream = stream;
                let r = Response::text("503 Service Unavailable", "too many connections\n");
                let _ = tokio::time::timeout(Duration::from_secs(2), write(&mut stream, r)).await;
            });
            continue;
        };
        let (reactor, fresh_at) = (reactor.clone(), fresh_at.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(RESPONSE_TIMEOUT, handle(stream, reactor, fresh_at)).await;
        });
    }
}

struct Response {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    /// HEAD: send the headers (with the body's length) but not the body
    head_only: bool,
}

impl Response {
    fn text(status: &'static str, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", "text/plain".to_string())],
            body: body.as_bytes().to_vec(),
            head_only: false,
        }
    }
}

async fn handle(
    mut stream: TcpStream,
    reactor: NeoReactor,
    fresh_at: Arc<Mutex<HashMap<String, Instant>>>,
) -> Result<()> {
    let mut buf = vec![];
    let mut chunk = [0u8; 1024];
    let read = tokio::time::timeout(REQUEST_TIMEOUT, async {
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(None);
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(Some(String::from_utf8_lossy(&buf[..end]).to_string()));
            }
            if buf.len() > MAX_REQUEST {
                return Err(anyhow::anyhow!("request too large"));
            }
        }
    })
    .await;
    let head = match read {
        Ok(Ok(Some(head))) => head,
        Ok(Ok(None)) | Err(_) => return Ok(()),
        Ok(Err(_)) => {
            return write(
                &mut stream,
                Response::text("431 Request Header Fields Too Large", ""),
            )
            .await
        }
    };
    let config = reactor.config().await?.borrow().clone();
    let response = respond(&head, &config, &reactor, &fresh_at).await;
    write(&mut stream, response).await
}

async fn write(stream: &mut TcpStream, r: Response) -> Result<()> {
    let mut out = format!(
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        r.status,
        r.body.len()
    );
    for (k, v) in &r.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes()).await?;
    if !r.head_only {
        stream.write_all(&r.body).await?;
    }
    Ok(())
}

async fn respond(
    head: &str,
    config: &Config,
    reactor: &NeoReactor,
    fresh_at: &Mutex<HashMap<String, Instant>>,
) -> Response {
    let mut lines = head.split("\r\n");
    let mut request = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (request.next().unwrap_or(""), request.next().unwrap_or(""));
    if method != "GET" && method != "HEAD" {
        return Response::text("405 Method Not Allowed", "GET only\n");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let authorization = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.trim().to_string());
    // Authenticate before looking at the path, so camera names can't be probed
    let user = match authenticate(config, authorization.as_deref()) {
        Ok(user) => user,
        Err(()) => {
            if authorization.is_some() {
                pace_failed_auth(config.limits.failed_auth_per_minute).await;
            }
            let mut r = Response::text("401 Unauthorized", "");
            r.headers
                .push(("WWW-Authenticate", "Basic realm=\"neolink\"".to_string()));
            return r;
        }
    };
    let Some(name) = path
        .strip_prefix('/')
        .and_then(|p| p.strip_suffix("/snapshot.jpg"))
        .map(percent_decode)
    else {
        return Response::text("404 Not Found", "use /<camera>/snapshot.jpg\n");
    };
    let name = name.as_str();
    let Some(camera) = config
        .cameras
        .iter()
        .find(|c| c.name == name && c.enabled && permitted(c, user.as_deref()))
    else {
        return Response::text("404 Not Found", "no such camera\n");
    };

    let state = StateFile::for_camera(camera);
    if query.split('&').any(|kv| kv == "fresh=1") {
        if !camera.snapshot_fresh {
            return Response::text(
                "403 Forbidden",
                "fresh snapshots wake the camera; set snapshot_fresh = true to allow them\n",
            );
        }
        let allowed = {
            let mut map = fresh_at.lock().unwrap();
            match map.get(name) {
                Some(t) if t.elapsed() < FRESH_INTERVAL => false,
                _ => {
                    map.insert(name.to_string(), Instant::now());
                    true
                }
            }
        };
        if allowed {
            if let Err(e) = take_fresh(reactor, name, &state).await {
                log::warn!("{name}: fresh snapshot failed: {e:?}");
            }
        }
    }

    let (body, at) = state
        .snapshot()
        .unwrap_or_else(|| (PLACEHOLDER.to_vec(), unix_now()));
    Response {
        status: "200 OK",
        headers: vec![
            ("Content-Type", "image/jpeg".to_string()),
            ("Last-Modified", http_date(at)),
            ("Cache-Control", "no-cache".to_string()),
        ],
        body,
        head_only: method == "HEAD",
    }
}

async fn take_fresh(reactor: &NeoReactor, name: &str, state: &StateFile) -> Result<()> {
    let camera = reactor.get(name).await?;
    let jpeg = camera
        .run_task_for("http:snapshot?fresh=1", |cam| {
            Box::pin(async move { Ok(cam.get_snapshot().await?) })
        })
        .await?;
    state.save_snapshot(&jpeg)
}

/// Failed logins, across all clients: past `per_minute` in a minute each failure waits for
/// its own 1 s slot, like the RTSP server, so the endpoint can't be used to guess the RTSP
/// passwords quickly
async fn pace_failed_auth(per_minute: usize) {
    use std::collections::VecDeque;
    static FAILS: Mutex<Option<(VecDeque<Instant>, Instant)>> = Mutex::new(None);
    let wait = {
        let mut g = FAILS.lock().unwrap();
        let now = Instant::now();
        let (times, next) = g.get_or_insert_with(|| (VecDeque::new(), now));
        while times
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            times.pop_front();
        }
        times.push_back(now);
        if times.len() > per_minute {
            times.pop_front();
            let slot = (*next).max(now) + Duration::from_secs(1);
            *next = slot;
            Some(slot)
        } else {
            None
        }
    };
    if let Some(slot) = wait {
        tokio::time::sleep_until(slot.into()).await;
    }
}

/// The authenticated user: `Ok(None)` when no users are configured (anyone may fetch)
fn authenticate(config: &Config, authorization: Option<&str>) -> Result<Option<String>, ()> {
    if config.users.is_empty() {
        return Ok(None);
    }
    let decoded = authorization
        .and_then(|a| a.strip_prefix("Basic "))
        .and_then(|b| BASE64.decode(b.trim()).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or(())?;
    let (name, pass) = decoded.split_once(':').ok_or(())?;
    // Check every user's password in constant time so timing reveals neither
    let mut found = None;
    for u in &config.users {
        let pass_ok = constant_time_eq(u.pass.as_deref().unwrap_or("").as_bytes(), pass.as_bytes())
            && u.pass.is_some();
        if pass_ok && u.name == name {
            found = Some(u.name.clone());
        }
    }
    found.map(Some).ok_or(())
}

/// The RTSP rule: no `permitted_users` means every user; "anyone" means every user
fn permitted(camera: &CameraConfig, user: Option<&str>) -> bool {
    match (&camera.permitted_users, user) {
        (_, None) => true,
        (None, Some(_)) => true,
        (Some(list), Some(user)) => list.iter().any(|p| p == user || p == "anyone"),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8 | ((a.len() ^ b.len()) >> 8) as u8;
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0xff);
    }
    diff == 0
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RFC 7231 IMF-fixdate
fn http_date(unix: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = (unix / 86400) as i64;
    let secs = unix % 86400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[(days % 7) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &str) -> Config {
        toml::from_str(&format!(
            "{extra}\n[[cameras]]\nname = \"argus\"\nusername = \"admin\"\naddress = \"127.0.0.1:1\"\n"
        ))
        .unwrap()
    }

    #[test]
    fn test_http_date() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_791_726_226), "Sun, 11 Oct 2026 13:43:46 GMT");
        assert_eq!(http_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
    }

    #[test]
    fn test_authenticate_and_permitted() {
        let open = config("");
        assert_eq!(authenticate(&open, None), Ok(None));
        assert!(permitted(&open.cameras[0], None));

        let mut users = config(
            "[[users]]\nname = \"alice\"\npass = \"alicepw\"\n[[users]]\nname = \"bob\"\npass = \"bobpw\"",
        );
        let basic = |u: &str| format!("Basic {}", BASE64.encode(u));
        assert_eq!(authenticate(&users, None), Err(()));
        assert_eq!(authenticate(&users, Some(&basic("alice:nope"))), Err(()));
        assert_eq!(authenticate(&users, Some(&basic("alice:bobpw"))), Err(()));
        assert_eq!(
            authenticate(&users, Some(&basic("alice:alicepw"))),
            Ok(Some("alice".to_string()))
        );
        users.cameras[0].permitted_users = Some(vec!["alice".to_string()]);
        assert!(!permitted(&users.cameras[0], Some("bob")));
        assert!(permitted(&users.cameras[0], Some("alice")));
    }

    #[test]
    fn test_helpers() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert_eq!(percent_decode("front%20door"), "front door");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%aé%41"), "%aéA");
    }

    #[test]
    fn test_port() {
        assert_eq!(port(&config("")), Some(8555));
        assert_eq!(port(&config("snapshot_port = 0")), None);
        assert_eq!(port(&config("snapshot_port = 9000")), Some(9000));
    }

    #[test]
    fn test_placeholder_is_a_jpeg() {
        assert_eq!(&PLACEHOLDER[..2], b"\xff\xd8");
        assert_eq!(&PLACEHOLDER[PLACEHOLDER.len() - 2..], b"\xff\xd9");
    }
}
