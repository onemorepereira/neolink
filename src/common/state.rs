//! Per-camera state shared by every neolink mode and process (SPEC P15)
//!
//! `{state_dir}/camera/<id>.json` holds what neolink learnt during the camera's last
//! wakes: whether it has a battery, its streams, UID, model and firmware, the last battery
//! reading and when the last snapshot (`<id>.jpg`) was taken. Values are only refreshed
//! while the camera is awake for another reason, so reading them never wakes it.
//!
//! `state_dir` is `$NEOLINK_STATE_DIR`, else `$XDG_STATE_HOME/neolink`, else
//! `~/.local/state/neolink`, else `/tmp/neolink`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::CameraConfig;
use neolink_core::bc::xml::BatteryInfo;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct BatteryReading {
    pub(crate) percent: u32,
    pub(crate) voltage: i32,
    pub(crate) current: i32,
    pub(crate) temperature: i32,
    pub(crate) charge_status: String,
    pub(crate) adapter_status: String,
    /// Unix seconds
    pub(crate) at: u64,
}

impl BatteryReading {
    /// A reading taken now
    pub(crate) fn now(b: &BatteryInfo) -> Self {
        Self {
            percent: b.battery_percent,
            voltage: b.voltage,
            current: b.current,
            temperature: b.temperature,
            charge_status: b.charge_status.clone(),
            adapter_status: b.adapter_status.clone(),
            at: unix_now(),
        }
    }

    /// As the camera's reply, for the CLI and MQTT output formats
    pub(crate) fn to_info(&self, channel_id: u8) -> BatteryInfo {
        BatteryInfo {
            channel_id,
            charge_status: self.charge_status.clone(),
            adapter_status: self.adapter_status.clone(),
            voltage: self.voltage,
            current: self.current,
            temperature: self.temperature,
            battery_percent: self.percent,
            low_power: 0,
            battery_version: 0,
        }
    }

    /// Seconds since it was taken
    pub(crate) fn age(&self) -> u64 {
        unix_now().saturating_sub(self.at)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct CameraState {
    /// True once a battery read succeeded; never inferred false (solar cameras report an
    /// adapter too)
    pub(crate) battery: Option<bool>,
    pub(crate) supported_streams: Vec<String>,
    pub(crate) uid: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) firmware: Option<String>,
    pub(crate) last_battery: Option<BatteryReading>,
    /// Unix seconds of the snapshot in `<id>.jpg`
    pub(crate) snapshot_at: Option<u64>,
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Tests run as parallel threads of one process: they share one directory instead of each
/// test module setting `NEOLINK_STATE_DIR` to its own value, which raced. It starts empty
/// (a reused pid must not see an earlier run's state), and the first call removes the
/// directories of test processes that have exited, which nothing else cleans up
#[cfg(test)]
pub(crate) fn state_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let tmp = std::env::temp_dir();
        // Liveness is read from /proc, so only on Linux; elsewhere they are left alone
        if cfg!(target_os = "linux") {
            for entry in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
                let name = entry.file_name();
                let pid = name
                    .to_str()
                    .and_then(|n| n.strip_prefix("neolink-tests-"))
                    .and_then(|p| p.parse::<u32>().ok());
                if let Some(pid) = pid {
                    if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
        let dir = tmp.join(format!("neolink-tests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    })
    .clone()
}

#[cfg(not(test))]
pub(crate) fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NEOLINK_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(dir).join("neolink");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/neolink");
    }
    PathBuf::from("/tmp/neolink")
}

/// FNV-1a, so file names don't change between Rust versions
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

/// The state file of one camera
#[derive(Debug, Clone)]
pub(crate) struct StateFile {
    json: PathBuf,
    jpg: PathBuf,
}

impl StateFile {
    pub(crate) fn for_camera(config: &CameraConfig) -> Self {
        Self::in_dir(&state_dir(), config)
    }

    pub(crate) fn in_dir(dir: &Path, config: &CameraConfig) -> Self {
        let key = format!(
            "{}|{}|{}|{}",
            config.name,
            config.camera_addr.as_deref().unwrap_or(""),
            config.camera_uid.as_deref().unwrap_or(""),
            config.channel_id
        );
        let id = format!("{}-{:016x}", sanitize(&config.name), fnv1a(&key));
        let dir = dir.join("camera");
        Self {
            json: dir.join(format!("{id}.json")),
            jpg: dir.join(format!("{id}.jpg")),
        }
    }

    /// The saved state, or the default if there is none (or it can't be read)
    pub(crate) fn load(&self) -> CameraState {
        std::fs::read(&self.json)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Change the state and write it atomically
    /// Updates within a process are serialized, so concurrent writers (wake reads, a
    /// fresh snapshot) don't lose each other's fields
    pub(crate) fn update(&self, f: impl FnOnce(&mut CameraState)) -> Result<CameraState> {
        let _guard = UPDATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.load();
        f(&mut state);
        write_atomic(&self.json, &serde_json::to_vec_pretty(&state)?)?;
        Ok(state)
    }

    pub(crate) fn save_snapshot(&self, jpeg: &[u8]) -> Result<()> {
        write_atomic(&self.jpg, jpeg)?;
        self.update(|s| s.snapshot_at = Some(unix_now()))?;
        Ok(())
    }

    /// The last snapshot and when it was taken
    pub(crate) fn snapshot(&self) -> Option<(Vec<u8>, u64)> {
        let at = self.load().snapshot_at?;
        Some((std::fs::read(&self.jpg).ok()?, at))
    }

    /// Warn when another live process holds this camera's lock; the guard removes it
    pub(crate) fn lock(&self) -> Option<LockGuard> {
        let path = self.json.with_extension("lock");
        if let Ok(pid) = std::fs::read_to_string(&path) {
            let pid = pid.trim();
            if !pid.is_empty()
                && pid != std::process::id().to_string()
                && Path::new("/proc").join(pid).exists()
            {
                log::warn!(
                    "Another neolink process (pid {pid}) uses this camera too; each one wakes it separately"
                );
                // Leave its lock alone: a CLI run must not take over the daemon's lock
                return None;
            }
        }
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, std::process::id().to_string()).ok()?;
        Some(LockGuard { path })
    }
}

pub(crate) struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path).ok().as_deref()
            == Some(&std::process::id().to_string())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

static UPDATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("state path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Unique per file, process and write, so concurrent writes never share a temp file
    let n = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}.{}", std::process::id(), n));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(name: &str, addr: &str) -> CameraConfig {
        toml::from_str(&format!(
            "name = \"{name}\"\nusername = \"admin\"\naddress = \"{addr}\""
        ))
        .unwrap()
    }

    #[test]
    fn test_concurrent_updates_keep_every_field() {
        let dir = std::env::temp_dir().join(format!("neolink-state-conc-{}", std::process::id()));
        let f = StateFile::in_dir(&dir, &config("argus", "10.0.0.1"));
        f.update(|s| s.uid = Some("UID".to_string())).unwrap();
        let threads: Vec<_> = (0..2)
            .map(|t| {
                let f = f.clone();
                std::thread::spawn(move || {
                    for i in 0..200u64 {
                        if t == 0 {
                            f.update(|s| s.snapshot_at = Some(i)).unwrap();
                        } else {
                            f.save_snapshot(b"jpg").unwrap();
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(f.load().uid.as_deref(), Some("UID"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_roundtrip_and_identity() {
        let dir = std::env::temp_dir().join(format!("neolink-state-test-{}", std::process::id()));
        let a = StateFile::in_dir(&dir, &config("argus", "192.168.1.2"));
        let b = StateFile::in_dir(&dir, &config("argus", "192.168.1.3"));
        assert_ne!(a.json, b.json);
        assert_eq!(a.load(), CameraState::default());
        a.update(|s| s.battery = Some(true)).unwrap();
        assert_eq!(a.load().battery, Some(true));
        assert_eq!(b.load().battery, None);
        assert!(a.snapshot().is_none());
        a.save_snapshot(b"\xff\xd8jpeg").unwrap();
        assert_eq!(a.snapshot().unwrap().0, b"\xff\xd8jpeg");
        let lock = a.lock().unwrap();
        assert!(a.json.with_extension("lock").exists());
        drop(lock);
        assert!(!a.json.with_extension("lock").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
