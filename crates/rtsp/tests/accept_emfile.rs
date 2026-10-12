//! Finding 5: out of file descriptors, the accept loop backs off instead of spinning, and
//! recovers once descriptors are free again.
//!
//! Its own test binary (process): it lowers RLIMIT_NOFILE for the whole process.
#![cfg(target_os = "linux")]

mod common;

use common::*;
use neolink_rtsp::rtsp::auth::{Allowed, Auth, Schemes};
use neolink_rtsp::rtsp::route::Routes;
use neolink_rtsp::rtsp::server::{Server, ServerConfig};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

fn cpu_time() -> Duration {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) }, 0);
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(u.ru_utime) + tv(u.ru_stime)
}

/// Lower the soft fd limit to just above the highest fd in use and fill every hole below
/// it: the next fd anyone opens fails with EMFILE. Returns the fillers (to close later).
fn exhaust_fds() -> Vec<i32> {
    let max = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .max()
        .unwrap();
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
    lim.rlim_cur = (max + 1) as libc::rlim_t;
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) }, 0);
    let mut fillers = vec![];
    loop {
        let fd = unsafe { libc::dup(0) };
        if fd < 0 {
            break;
        }
        fillers.push(fd);
    }
    fillers
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_accept_backs_off_when_out_of_fds() {
    let provider = Arc::new(TestProvider {
        hubs: HashMap::new(),
        allowed: Allowed::Anonymous,
    });
    let server = Server::new(
        ServerConfig::default(),
        Routes::default(),
        Auth::new(vec![], Schemes::Both),
        provider,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    tokio::spawn(server.clone().serve(listener, cancel.clone()));
    // Client sockets made before the limit; connecting them needs no new fd
    let socks: Vec<_> = (0..4)
        .map(|_| tokio::net::TcpSocket::new_v4().unwrap())
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let fillers = exhaust_fds();
    let mut clients = vec![];
    for s in socks {
        clients.push(s.connect(addr).await.unwrap());
    }
    // The server can't accept them: it must not spin meanwhile
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (cpu0, t0) = (cpu_time(), std::time::Instant::now());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (cpu, wall) = (cpu_time() - cpu0, t0.elapsed());
    for fd in fillers {
        unsafe { libc::close(fd) };
    }
    assert!(
        cpu < wall / 4,
        "accept loop spun: {cpu:?} CPU in {wall:?} while out of fds"
    );
    // Descriptors back: the waiting clients are served
    let mut c = Client::from_stream(clients.pop().unwrap());
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        c.request("OPTIONS", &format!("rtsp://{addr}/"), &[]),
    )
    .await
    .expect("served after fds came back");
    assert_eq!(r.status, 200);
    cancel.cancel();
}
