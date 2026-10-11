use std::env;
use std::process::Command;

fn main() {
    build_ver();
    platform_cfg();
}

fn build_ver() {
    let cargo_ver = env::var("CARGO_PKG_VERSION").unwrap();
    let version = git_ver().unwrap_or(format!("{} (unknown commit)", cargo_ver));

    println!("cargo:rustc-env=NEOLINK_VERSION={}", version);
    println!(
        "cargo:rustc-env=NEOLINK_PROFILE={}",
        env::var("PROFILE").unwrap()
    );
}

fn git_ver() -> Option<String> {
    github_ver().or_else(git_cmd_ver)
}

fn git_cmd_ver() -> Option<String> {
    let mut git_cmd = Command::new("git");
    git_cmd.args(["describe", "--tags"]);

    if let Some(true) = git_cmd.status().ok().map(|exit| exit.success()) {
        println!("cargo:rerun-if-changed=.git/HEAD");
        git_cmd
            .output()
            .ok()
            .map(|o| String::from_utf8(o.stdout).unwrap())
    } else {
        None
    }
}

fn github_ver() -> Option<String> {
    if let Ok(sha1) = env::var("GITHUB_SHA") {
        println!("cargo:rerun-if-env-changed=GITHUB_SHA");
        Some(sha1)
    } else {
        None
    }
}

/// GStreamer and OpenSSL link paths, only for builds that use GStreamer, decided by the
/// *target* OS (a build script's own `cfg` is the host's) (PLAN 1.8)
fn platform_cfg() {
    if env::var_os("CARGO_FEATURE_GSTREAMER").is_none() {
        return;
    }
    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("windows") => {
            let gstreamer_dir = env::var_os("GSTREAMER_1_0_ROOT_X86_64")
                .and_then(|x| x.into_string().ok())
                .unwrap_or_else(|| {
                    env::var_os("GSTREAMER_1_0_ROOT_MSVC_X86_64")
                        .and_then(|x| x.into_string().ok())
                        .unwrap_or_else(|| r#"C:\gstreamer\1.0\x86_64\"#.to_string())
                });
            println!(r"cargo:rustc-link-search=native={}\lib", gstreamer_dir);
        }
        Ok("macos") => {
            let gstreamer_dir = env::var_os("GSTREAMER_1_0_ROOT_MACOSX")
                .and_then(|x| x.into_string().ok())
                .unwrap_or_else(|| {
                    r"/Library/Frameworks/GStreamer.framework/Versions/1.0".to_string()
                });
            let openssl_dir = env::var_os("OPENSSL_1_1_ROOT_MACOSX")
                .and_then(|x| x.into_string().ok())
                .unwrap_or_else(|| r"/usr/local/opt/openssl@1.1".to_string());
            println!(
                r"cargo:rustc-link-search=native={}/lib;{}/lib",
                gstreamer_dir, openssl_dir
            );
            println!(
                r"cargo:rustc-link-arg=-Wl,-rpath,{}/lib;{}/lib/",
                gstreamer_dir, openssl_dir
            );
        }
        _ => {}
    }
}
