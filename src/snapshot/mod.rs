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
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
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
            continue; // Over the limit: drop the connection
        };
        let (reactor, fresh_at) = (reactor.clone(), fresh_at.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(REQUEST_TIMEOUT, handle(stream, reactor, fresh_at)).await;
        });
    }
}

struct Response {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Response {
    fn text(status: &'static str, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", "text/plain".to_string())],
            body: body.as_bytes().to_vec(),
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
    let head = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break String::from_utf8_lossy(&buf[..end]).to_string();
        }
        if buf.len() > MAX_REQUEST {
            return write(
                &mut stream,
                Response::text("431 Request Header Fields Too Large", ""),
            )
            .await;
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
    stream.write_all(&r.body).await?;
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
    let Some(name) = path
        .strip_prefix('/')
        .and_then(|p| p.strip_suffix("/snapshot.jpg"))
    else {
        return Response::text("404 Not Found", "use /<camera>/snapshot.jpg\n");
    };
    let Some(camera) = config.cameras.iter().find(|c| c.name == name && c.enabled) else {
        return Response::text("404 Not Found", "no such camera\n");
    };
    let authorization = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.trim().to_string());
    match authorize(config, camera, authorization.as_deref()) {
        Auth::Ok => {}
        Auth::Unauthorized => {
            let mut r = Response::text("401 Unauthorized", "");
            r.headers
                .push(("WWW-Authenticate", "Basic realm=\"neolink\"".to_string()));
            return r;
        }
        Auth::Forbidden => return Response::text("404 Not Found", "no such camera\n"),
    }

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
        body: if method == "HEAD" { vec![] } else { body },
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

#[derive(Debug, PartialEq, Eq)]
enum Auth {
    Ok,
    Unauthorized,
    Forbidden,
}

fn authorize(config: &Config, camera: &CameraConfig, authorization: Option<&str>) -> Auth {
    if config.users.is_empty() {
        return Auth::Ok;
    }
    let user = authorization
        .and_then(|a| a.strip_prefix("Basic "))
        .and_then(|b| BASE64.decode(b.trim()).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| {
            let (name, pass) = s.split_once(':')?;
            config
                .users
                .iter()
                .find(|u| u.name == name && u.pass.as_deref() == Some(pass))
                .map(|u| u.name.clone())
        });
    let Some(user) = user else {
        return Auth::Unauthorized;
    };
    match &camera.permitted_users {
        Some(permitted) if !permitted.iter().any(|p| p == &user || p == "anyone") => {
            Auth::Forbidden
        }
        _ => Auth::Ok,
    }
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
    fn test_authorize() {
        let open = config("");
        assert_eq!(authorize(&open, &open.cameras[0], None), Auth::Ok);

        let mut users = config(
            "[[users]]\nname = \"alice\"\npass = \"alicepw\"\n[[users]]\nname = \"bob\"\npass = \"bobpw\"",
        );
        let basic = |u: &str| format!("Basic {}", BASE64.encode(u));
        let cam = users.cameras[0].clone();
        assert_eq!(authorize(&users, &cam, None), Auth::Unauthorized);
        assert_eq!(
            authorize(&users, &cam, Some(&basic("alice:nope"))),
            Auth::Unauthorized
        );
        assert_eq!(
            authorize(&users, &cam, Some(&basic("alice:alicepw"))),
            Auth::Ok
        );
        users.cameras[0].permitted_users = Some(vec!["alice".to_string()]);
        let cam = users.cameras[0].clone();
        assert_eq!(
            authorize(&users, &cam, Some(&basic("bob:bobpw"))),
            Auth::Forbidden
        );
        assert_eq!(
            authorize(&users, &cam, Some(&basic("alice:alicepw"))),
            Auth::Ok
        );
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
