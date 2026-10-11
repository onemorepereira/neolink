//! Tests of the camera instance machinery against `neolink_core::test_util::FakeCamera`

use super::*;
use crate::config::CameraConfig;
use neolink_core::test_util::{FakeCamera, FakeCameraConfig, FakeStats};
use std::time::Duration;

/// A camera config that reaches `cam` over TCP, with extra TOML lines
pub(crate) fn config_for(cam: &FakeCamera, extra: &str) -> CameraConfig {
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
    let neocam = new_neocam(config_for(&cam, "")).await;
    let instance = neocam.subscribe().await.unwrap();
    wait_stats(&cam, 10, |s| s.logins >= 1).await;
    cam.drop_connections();
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
