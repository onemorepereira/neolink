//! Tests of the camera instance machinery against `neolink_core::test_util::FakeCamera`

use super::*;
use crate::config::CameraConfig;
use neolink_core::test_util::{FakeCamera, FakeCameraConfig, FakeStats};
use std::time::Duration;

/// A camera config that reaches `cam` over TCP, with extra TOML lines
pub(crate) fn config_for(cam: &FakeCamera, extra: &str) -> CameraConfig {
    // State files of all tests go to one temporary directory (`state_dir` under test);
    // the fake's port keeps them apart
    let text = format!(
        "name = \"fake\"\nusername = \"admin\"\npassword = \"fakepass\"\naddress = \"{}\"\n{}",
        cam.addr(),
        extra
    );
    toml::from_str(&text).unwrap()
}

pub(crate) async fn new_neocam(config: CameraConfig) -> NeoCam {
    #[cfg(feature = "pushnoti")]
    let neocam = {
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        NeoCam::new(config, tx).await.unwrap()
    };
    #[cfg(not(feature = "pushnoti"))]
    let neocam = NeoCam::new(config).await.unwrap();
    neocam
}

/// Wait until `pred` holds for the fake's stats, or panic after `secs` seconds
pub(crate) async fn wait_stats(cam: &FakeCamera, secs: u64, pred: impl Fn(&FakeStats) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let stats = cam.stats();
        if pred(&stats) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out, stats: {:?}",
            stats
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn test_run_task_reaches_camera() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "")).await;
    let instance = neocam.subscribe().await.unwrap();
    let uid = instance
        .run_task(|cam| Box::pin(async move { Ok(cam.uid().await?) }))
        .await
        .unwrap();
    assert_eq!(uid, "FAKEUID0000000001");
    assert!(cam.stats().logins >= 1);
}

#[tokio::test]
async fn test_reconnects_after_drop() {
    let _ = env_logger::builder().is_test(true).try_init();
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_disconnect = false")).await;
    let instance = neocam.subscribe().await.unwrap();
    wait_stats(&cam, 10, |s| s.logins >= 1).await;
    cam.drop_connections();
    wait_stats(&cam, 5, |s| s.open_connections == 0).await;
    // The next task must be served by a new login
    let battery = tokio::time::timeout(
        Duration::from_secs(20),
        instance.run_task(|cam| Box::pin(async move { Ok(cam.battery_info().await?) })),
    )
    .await
    .expect("task did not recover from the drop")
    .unwrap();
    assert_eq!(battery.battery_percent, 80);
    assert!(cam.stats().logins >= 2);
}

// ---------------------------------------------------------------------------------------
// Power (SPEC §4.1.1)

/// P1: a sleeping camera is not contacted at boot, even after an hour
#[tokio::test(start_paused = true)]
async fn test_boot_makes_no_connection() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let _neocam = new_neocam(config_for(&cam, "")).await;
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(cam.stats(), FakeStats::default());
}

/// Always-on must be asked for, and then it connects at boot
#[tokio::test]
async fn test_always_on_connects_at_boot() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let _neocam = new_neocam(config_for(&cam, "idle_disconnect = false")).await;
    wait_stats(&cam, 10, |s| s.logins == 1).await;
}

/// A known mains camera stays connected unless idle_disconnect is set
#[tokio::test]
async fn test_mains_camera_stays_connected() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let _neocam = new_neocam(config_for(&cam, "battery = false")).await;
    wait_stats(&cam, 10, |s| s.logins == 1).await;
}

/// P2, P13: a task wakes the camera, which is released with a logout after idle_grace_s
#[tokio::test]
async fn test_task_wakes_then_releases_cleanly() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_grace_s = 1")).await;
    let instance = neocam.subscribe().await.unwrap();
    let percent = instance
        .run_task(|cam| Box::pin(async move { Ok(cam.battery_info().await?.battery_percent) }))
        .await
        .unwrap();
    assert_eq!(percent, 80);
    assert_eq!(cam.stats().logins, 1);
    wait_stats(&cam, 10, |s| s.open_connections == 0).await;
    let s = cam.stats();
    assert_eq!(s.logouts, 1, "released without a logout: {:?}", s);
    // No probe login: one connection for discovery, one for the session
    assert_eq!(s.failed_logins, 0);
    assert_eq!(s.connections, 2);

    let wakes = neocam.ledger().lock().unwrap().wakes();
    assert_eq!(wakes.len(), 1);
    assert!(
        wakes[0].reason.starts_with("task:src/common/tests.rs:"),
        "{:?}",
        wakes
    );
    assert!(wakes[0].awake.is_some());
}

/// P2: a use within the grace period reuses the connection
#[tokio::test]
async fn test_grace_period_reuses_connection() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_grace_s = 5")).await;
    let instance = neocam.subscribe().await.unwrap();
    for _ in 0..3 {
        instance
            .run_task(|cam| Box::pin(async move { Ok(cam.uid().await?) }))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(cam.stats().logins, 1);
}

/// P4: an unreachable camera is not retried once nothing needs it
#[tokio::test]
async fn test_no_retry_without_demand() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    cam.set_unreachable(true);
    let neocam = new_neocam(config_for(&cam, "idle_grace_s = 1")).await;
    let instance = neocam.subscribe().await.unwrap();
    let task = tokio::time::timeout(
        Duration::from_secs(4),
        instance.run_task(|cam| Box::pin(async move { Ok(cam.uid().await?) })),
    )
    .await;
    assert!(task.is_err(), "the task should still be waiting");
    // Demand ended when the task was dropped; after the grace period attempts stop
    tokio::time::sleep(Duration::from_secs(3)).await;
    let attempts = cam.stats().connections;
    assert!(attempts >= 1);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        cam.stats().connections,
        attempts,
        "kept retrying without demand"
    );
}

/// P14: re-publishing a config, or changing a key that doesn't affect the connection,
/// does not reconnect
#[tokio::test]
async fn test_config_push_reconnects_only_when_needed() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let config = config_for(&cam, "idle_disconnect = false");
    let neocam = new_neocam(config.clone()).await;
    wait_stats(&cam, 10, |s| s.logins == 1).await;

    neocam.update_config(config.clone()).await.unwrap();
    let mut changed = config.clone();
    changed.idle_grace_s = 99;
    changed.use_splash = !changed.use_splash;
    neocam.update_config(changed.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        cam.stats().logins,
        1,
        "reconnected for a non-connection change"
    );

    // A different (equivalent) address is a connection change
    let mut moved = changed.clone();
    moved.camera_addr = Some(format!("localhost:{}", cam.addr().port()));
    neocam.update_config(moved).await.unwrap();
    wait_stats(&cam, 10, |s| s.logins == 2).await;
    assert_eq!(cam.stats().logouts, 1);
}

/// P7, P15: a wake records battery, UID, model and streams in the state file
#[tokio::test]
async fn test_wake_fills_state_file() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_grace_s = 1")).await;
    let instance = neocam.subscribe().await.unwrap();
    instance
        .run_task(|cam| Box::pin(async move { Ok(cam.get_linktype().await?) }))
        .await
        .unwrap();
    let state = neocam.state();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while state.load().model.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "state: {:?}",
            state.load()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let s = state.load();
    assert_eq!(s.battery, Some(true));
    assert_eq!(s.last_battery.as_ref().map(|b| b.percent), Some(80));
    assert_eq!(s.uid.as_deref(), Some("FAKEUID0000000001"));
    assert_eq!(s.supported_streams, ["mainStream", "subStream"]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while state.snapshot().is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no snapshot: {:?}",
            state.load()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state.snapshot().unwrap().0, b"\xff\xd8fake jpeg\xff\xd9");
}

/// PLAN 1.5: shutting a connected camera down logs it out, quickly
#[tokio::test]
async fn test_shutdown_logs_out() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let mut neocam = new_neocam(config_for(&cam, "idle_disconnect = false")).await;
    wait_stats(&cam, 10, |s| s.logins == 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t = tokio::time::Instant::now();
    neocam.shutdown().await;
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "shutdown took {:?}",
        t.elapsed()
    );
    wait_stats(&cam, 2, |s| s.logouts == 1 && s.open_connections == 0).await;
}

fn fixture() -> std::sync::Arc<Vec<neolink_core::fixture::Record>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/synth/plain_h264.bcm");
    let file = std::fs::File::open(path).expect("run cargo xtask mkfixture --standard first");
    std::sync::Arc::new(neolink_core::fixture::read_all(file).unwrap())
}

/// PLAN 1.3: dropping a stream releases the camera (stop + logout after the grace)
#[tokio::test]
async fn test_stream_cancel_releases_permit() {
    let cam = FakeCamera::start(FakeCameraConfig {
        main: fixture(),
        loop_media: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_grace_s = 1")).await;
    let instance = neocam.subscribe().await.unwrap();
    let mut rx = instance
        .stream(neolink_core::bc_protocol::StreamKind::Main)
        .await
        .unwrap();
    assert!(rx.recv().await.is_some());
    drop(rx);
    wait_stats(&cam, 5, |s| s.video_stops == 1 && s.open_connections == 0).await;
    assert_eq!(cam.stats().logouts, 1);
}

/// PLAN 1.3: a stalled camera (no frames) is released too; before, the permit was only
/// dropped when the next frame failed to send
#[tokio::test]
async fn test_stalled_stream_cancel_releases_permit() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_grace_s = 1")).await;
    let instance = neocam.subscribe().await.unwrap();
    let rx = instance
        .stream(neolink_core::bc_protocol::StreamKind::Main)
        .await
        .unwrap();
    wait_stats(&cam, 10, |s| s.video_starts == 1).await;
    drop(rx);
    wait_stats(&cam, 5, |s| s.open_connections == 0).await;
    assert_eq!(cam.stats().logouts, 1);
}

/// PLAN 1.3: the pause.on_motion watchers stop with the stream
#[cfg(any(feature = "gstreamer", feature = "native-rtsp"))]
#[tokio::test]
async fn test_on_motion_stream_drop_stops_tasks() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(
        &cam,
        "idle_grace_s = 1\n[pause]\non_motion = true",
    ))
    .await;
    let instance = neocam.subscribe().await.unwrap();
    // Baseline after one plain wake, so per-wake tasks are counted on both sides
    instance
        .run_task(|cam| Box::pin(async move { Ok(cam.get_linktype().await?) }))
        .await
        .unwrap();
    wait_stats(&cam, 10, |s| s.open_connections == 0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    let rx = instance
        .stream_while_live(
            neolink_core::bc_protocol::StreamKind::Main,
            Some(tokio::time::Duration::from_secs(5)),
        )
        .await
        .unwrap();
    wait_stats(&cam, 10, |s| s.video_starts == 1).await;
    drop(rx);
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The 5 s initial permit and the grace end, then everything it started is gone
    wait_stats(&cam, 10, |s| s.open_connections == 0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    assert!(after <= before, "tasks leaked: {} -> {}", before, after);
}

/// Gate 1: 100 connect/teardown cycles; after each, no permits and the task count is
/// back to its baseline
#[tokio::test]
async fn test_100_cycles_return_to_baseline() {
    let cam = FakeCamera::start(FakeCameraConfig {
        snapshot: None,
        ..Default::default()
    })
    .await
    .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_grace_s = 0")).await;
    let instance = neocam.subscribe().await.unwrap();
    let alive = || {
        tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks()
    };
    let mut baseline = None;
    for cycle in 1..=100u32 {
        instance
            .run_task(|cam| Box::pin(async move { Ok(cam.get_linktype().await?) }))
            .await
            .unwrap();
        wait_stats(&cam, 10, |s| s.open_connections == 0 && s.logouts == cycle).await;
        assert_eq!(neocam.use_count(), 0, "cycle {}: permits left", cycle);
        // Let the wake's passive reads and the connection tasks finish
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let n = alive();
            match baseline {
                None if cycle == 2 => {
                    baseline = Some(n);
                    break;
                }
                Some(b) if n <= b => break,
                None => break,
                Some(b) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "cycle {}: {} tasks, baseline {}",
                        cycle,
                        n,
                        b
                    );
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    assert_eq!(cam.stats().logins, 100);
}

/// A25 / P4: with demand, an unreachable camera is retried with backoff (1, 2, 4.. 60 s).
/// Real time: tokio's paused clock races real sockets
#[tokio::test]
async fn test_backoff_with_demand() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    cam.set_unreachable(true);
    let neocam = new_neocam(config_for(&cam, "")).await;
    let instance = neocam.subscribe().await.unwrap();
    let permit = instance.permit("test:demand").await.unwrap();
    tokio::time::sleep(Duration::from_secs(40)).await;
    drop(permit);
    // Each attempt is one probe connection (it fails, so no login connection follows).
    // With backoff the attempts start at 0, 1, 3, 7, 15 and 31 s; the old 5 s cap made
    // about 13 in 40 s
    let attempts = cam.stats().connections;
    assert!((4..=8).contains(&attempts), "{} attempts in 40 s", attempts);
}

/// A28 / P12: a motion Start that never stops does not hold the camera after a drop
#[tokio::test]
async fn test_motion_permit_released_on_disconnect() {
    let _ = env_logger::builder().is_test(true).try_init();
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(
        &cam,
        "idle_grace_s = 1\n[pause]\non_motion = true",
    ))
    .await;
    let instance = neocam.subscribe().await.unwrap();
    let permit = instance.permit("test:wake").await.unwrap();
    // Wait until the motion listener has subscribed (MSG_ID_MOTION_REQUEST)
    wait_stats(&cam, 10, |s| s.messages.get(&31).is_some()).await;
    drop(permit);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while neocam.use_count() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "test permit not released"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cam.motion(true);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while neocam.use_count() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "motion took no permit"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cam.drop_connections();
    wait_stats(&cam, 10, |s| s.open_connections == 0).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while neocam.use_count() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "motion permit kept after the drop: {:?}",
            neocam.permit_reasons()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // No demand left: it stays disconnected
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(cam.stats().open_connections, 0);
}

// ---------------------------------------------------------------------------------------
// Phase 1 review regressions

/// Switching from always-on to sleeping releases the connected camera
#[tokio::test]
async fn test_always_on_to_sleep_disconnects() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let config = config_for(&cam, "idle_disconnect = false\nidle_grace_s = 1");
    let neocam = new_neocam(config.clone()).await;
    wait_stats(&cam, 10, |s| s.logins == 1).await;
    let mut sleeping = config;
    sleeping.idle_disconnect = Some(true);
    neocam.update_config(sleeping).await.unwrap();
    wait_stats(&cam, 10, |s| s.open_connections == 0 && s.logouts == 1).await;
}

/// A dropped connection after a good session retries at once, not on the slow schedule
#[tokio::test]
async fn test_retry_state_resets_after_a_good_session() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    let neocam = new_neocam(config_for(&cam, "idle_disconnect = false")).await;
    wait_stats(&cam, 10, |s| s.logins == 1).await;
    // One failure, then a good session, then a drop
    cam.set_unreachable(true);
    tokio::time::sleep(Duration::from_secs(2)).await;
    cam.set_unreachable(false);
    wait_stats(&cam, 10, |s| s.logins == 2).await;
    cam.drop_connections();
    wait_stats(&cam, 5, |s| s.logins == 3).await;
    drop(neocam);
}

/// Shutdown is quick even while waiting to retry an unreachable camera
#[tokio::test]
async fn test_shutdown_during_backoff_is_fast() {
    let cam = FakeCamera::start(FakeCameraConfig::default())
        .await
        .unwrap();
    cam.set_unreachable(true);
    let mut neocam = new_neocam(config_for(&cam, "idle_disconnect = false")).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let t = tokio::time::Instant::now();
    neocam.shutdown().await;
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "shutdown took {:?}",
        t.elapsed()
    );
}
