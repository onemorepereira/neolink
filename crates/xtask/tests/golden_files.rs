//! The committed GStreamer golden transcripts stay parseable, normalised and self-consistent

use std::path::PathBuf;
use xtask::golden::{normalize, parse, render, structural_diff, Entry};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../rtsp/tests/golden/gst")
}

fn load(name: &str) -> Vec<Entry> {
    let text = std::fs::read_to_string(dir().join(name)).unwrap();
    parse(&text).unwrap()
}

#[test]
fn test_all_files_are_normalised() {
    let mut n = 0;
    for f in std::fs::read_dir(dir()).unwrap() {
        let path = f.unwrap().path();
        if path.extension().map(|e| e == "txt").unwrap_or(false) {
            let text = std::fs::read_to_string(&path).unwrap();
            let entries = parse(&text).unwrap();
            assert_eq!(render(&normalize(&entries)), text, "{}", path.display());
            n += 1;
        }
    }
    assert!(n >= 28, "only {} golden files", n);
}

#[test]
fn test_rerecording_has_no_structural_deltas() {
    let d = structural_diff(
        &load("open_main_tcp.txt"),
        &load("open_main_tcp_repeat.txt"),
    );
    assert!(d.is_empty(), "{:#?}", d);
}

#[test]
fn test_auth_status_codes() {
    let statuses = |name: &str| -> Vec<String> {
        load(name)
            .into_iter()
            .filter_map(|e| match e {
                Entry::Msg(m) if m.start.starts_with("RTSP/") => {
                    Some(m.start.split_whitespace().nth(1).unwrap().to_string())
                }
                _ => None,
            })
            .collect()
    };
    assert_eq!(statuses("auth_none.txt"), ["200", "401", "401"]);
    assert_eq!(statuses("auth_wrong_pass.txt")[1..], ["401", "401"]);
    assert_eq!(statuses("auth_unknown_user.txt")[1..], ["401", "401"]);
    assert_eq!(statuses("auth_not_permitted.txt"), ["200", "401", "404"]);
    assert_eq!(statuses("auth_ok.txt")[..3], ["200", "401", "200"]);
    assert_eq!(statuses("open_unknown_path.txt"), ["200", "404"]);
    assert_eq!(statuses("open_extern_tcp.txt"), ["200", "400"]);
}
