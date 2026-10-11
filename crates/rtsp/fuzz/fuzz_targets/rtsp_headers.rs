#![no_main]
//! Header parsers, routing normalisation and the auth decision never panic
use libfuzzer_sys::fuzz_target;
use neolink_rtsp::rtsp::auth::{Allowed, Auth, Schemes, User};
use neolink_rtsp::rtsp::headers::{authorization, choose_transport, params, session_id};
use neolink_rtsp::rtsp::route::normalise;
use std::sync::OnceLock;

static AUTH: OnceLock<Auth> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let _ = choose_transport(s);
    let _ = authorization(s);
    let _ = params(s);
    let _ = session_id(s);
    let _ = normalise(s);
    let auth = AUTH.get_or_init(|| {
        Auth::new(
            vec![User {
                name: "alice".into(),
                pass: Some("alicepw".into()),
            }],
            Schemes::Both,
        )
    });
    let ip = "127.0.0.1".parse().unwrap();
    for allowed in [Allowed::Everyone, Allowed::Users(vec!["alice".into()])] {
        let _ = auth.check(&allowed, "DESCRIBE", s, Some(s), ip);
    }
});
