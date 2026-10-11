//! URL routing (SPEC R8, DESIGN §6.4)
//!
//! 19 aliases per camera: `/{name}/main` (+ Main, mainStream, MainStream, Mainstream,
//! mainstream), the same six for sub and extern, and `/{name}`, which resolves to the
//! first configured stream of main, sub, extern. Matching is exact and case-sensitive
//! after percent-decoding; the query string, a trailing `/` and a track suffix
//! (`/stream=N`, `/trackID=N`) are ignored.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StreamKind {
    Main,
    Sub,
    Extern,
}

impl StreamKind {
    pub fn name(self) -> &'static str {
        match self {
            StreamKind::Main => "main",
            StreamKind::Sub => "sub",
            StreamKind::Extern => "extern",
        }
    }
}

/// What a path resolves to
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MountTarget {
    pub camera: String,
    pub kind: StreamKind,
}

/// A normalised request path: the mount path and the track, if any
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedPath {
    pub mount: String,
    pub track: Option<u32>,
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

/// The path of an RTSP request URI (`rtsp://host:port/a/b?q` or `/a/b`), normalised
pub fn normalise(uri: &str) -> RoutedPath {
    let path = match uri.split_once("://") {
        Some((_, rest)) => rest.find('/').map(|i| &rest[i..]).unwrap_or("/"),
        None => uri,
    };
    let path = path.split('?').next().unwrap_or("");
    let mut path = percent_decode(path);
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    let mut track = None;
    if let Some(i) = path.rfind('/') {
        let last = &path[i + 1..];
        for prefix in ["stream=", "trackID="] {
            if let Some(n) = last
                .strip_prefix(prefix)
                .and_then(|n| n.parse::<u32>().ok())
            {
                track = Some(n);
                path.truncate(i);
                break;
            }
        }
    }
    if path.is_empty() {
        path.push('/');
    }
    RoutedPath { mount: path, track }
}

/// The mount paths of one camera, given which kinds are configured (in config order is
/// irrelevant: the bare path takes main, then sub, then extern)
pub fn aliases(camera: &str, kinds: &[StreamKind]) -> Vec<(String, MountTarget)> {
    let mut out = vec![];
    for &kind in kinds {
        let names: &[&str] = match kind {
            StreamKind::Main => &[
                "main",
                "Main",
                "mainStream",
                "MainStream",
                "Mainstream",
                "mainstream",
            ],
            StreamKind::Sub => &[
                "sub",
                "Sub",
                "subStream",
                "SubStream",
                "Substream",
                "substream",
            ],
            StreamKind::Extern => &[
                "extern",
                "Extern",
                "externStream",
                "ExternStream",
                "Externstream",
                "externstream",
            ],
        };
        for n in names {
            out.push((
                format!("/{camera}/{n}"),
                MountTarget {
                    camera: camera.to_string(),
                    kind,
                },
            ));
        }
    }
    let first = [StreamKind::Main, StreamKind::Sub, StreamKind::Extern]
        .into_iter()
        .find(|k| kinds.contains(k));
    if let Some(kind) = first {
        out.push((
            format!("/{camera}"),
            MountTarget {
                camera: camera.to_string(),
                kind,
            },
        ));
    }
    out
}

/// The routing table, swapped whole on config or availability change
#[derive(Debug, Default, Clone)]
pub struct Routes {
    table: Arc<RwLock<Arc<HashMap<String, MountTarget>>>>,
}

impl Routes {
    pub fn set(&self, mounts: impl IntoIterator<Item = (String, MountTarget)>) {
        *self.table.write().unwrap() = Arc::new(mounts.into_iter().collect());
    }

    pub fn lookup(&self, uri: &str) -> Option<(MountTarget, RoutedPath)> {
        let p = normalise(uri);
        let table = self.table.read().unwrap().clone();
        table.get(&p.mount).cloned().map(|t| (t, p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalise() {
        assert_eq!(normalise("rtsp://h:8554/argus/main").mount, "/argus/main");
        assert_eq!(normalise("rtsp://h:8554/argus/main/").mount, "/argus/main");
        let p = normalise("rtsp://h/argus/main/stream=1?x=1");
        assert_eq!((p.mount.as_str(), p.track), ("/argus/main", Some(1)));
        assert_eq!(normalise("rtsp://h/argus/sub/trackID=0").track, Some(0));
        assert_eq!(
            normalise("rtsp://h/front%20door/main").mount,
            "/front door/main"
        );
        assert_eq!(normalise("/argus").mount, "/argus");
        assert_eq!(normalise("rtsp://h").mount, "/");
        assert_eq!(normalise("*").mount, "*");
    }

    #[test]
    fn test_19_aliases() {
        let all = aliases(
            "argus",
            &[StreamKind::Main, StreamKind::Sub, StreamKind::Extern],
        );
        assert_eq!(all.len(), 19);
        let r = Routes::default();
        r.set(all);
        assert_eq!(r.lookup("rtsp://h/argus").unwrap().0.kind, StreamKind::Main);
        assert_eq!(
            r.lookup("rtsp://h/argus/Substream/").unwrap().0.kind,
            StreamKind::Sub
        );
        assert!(r.lookup("rtsp://h/argus/MAIN").is_none(), "case-sensitive");
        assert!(r.lookup("rtsp://h/nope/main").is_none());
        // Bare path follows the first configured stream
        let sub_only = aliases("cam", &[StreamKind::Sub]);
        assert_eq!(sub_only.len(), 7);
        assert_eq!(sub_only.last().unwrap().1.kind, StreamKind::Sub);
    }
}
