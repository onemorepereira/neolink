//! Atomic TOML config file persistence (temp file + `rename`) with
//! write-through to the Kubernetes ConfigMap so config survives pod restarts.

use std::path::Path;

use anyhow::Result;

use crate::config::Config;

/// Write `config` atomically to `path` AND patch the in-cluster ConfigMap.
/// The ConfigMap patch is best-effort: a failure logs a warning but does not
/// abort the write (the file on disk is always the authoritative copy).
pub(crate) async fn persist_config(path: &Path, config: &Config) -> Result<()> {
    let toml = toml::to_string(config).map_err(|e| anyhow::anyhow!("{e}"))?;

    // 1. Atomic write to disk (temp + rename).
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &toml)?;
    std::fs::rename(&tmp, path)?;

    // 2. Write-through to Kubernetes ConfigMap so the next pod restart picks
    //    up the current config without needing a git commit.
    if let Err(e) = crate::api::k8s::patch_configmap(&toml).await {
        log::warn!("ConfigMap write-through failed (config saved to disk): {e}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_persist_config_atomic() {
        let dir = std::env::temp_dir().join(format!("neolink-api-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let config: Config = toml::from_str(
            r#"
            [[cameras]]
            name = "test"
            username = "admin"
            address = "1.2.3.4"
            "#,
        )
        .unwrap();

        persist_config(&path, &config).await.unwrap();
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("name = \"test\""));
        assert!(!content.contains("secret"));

        // Second write doesn't corrupt
        persist_config(&path, &config).await.unwrap();
        assert!(path.exists());
        let content2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, content2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
