//! Atomic TOML config file persistence (temp file + `rename`).
//!
//! Writes to `<path>.tmp` then atomically renames over `<path>`, so a crash
//! mid-write never leaves a corrupt config file.

use std::path::Path;

use anyhow::Result;

use crate::config::Config;

pub(crate) fn persist_config(path: &Path, config: &Config) -> Result<()> {
    let toml = toml::to_string(config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, toml)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_persist_config_atomic() {
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

        persist_config(&path, &config).unwrap();
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("name = \"test\""));
        assert!(!content.contains("secret")); // passwords are skip_serializing

        // Second write doesn't corrupt
        persist_config(&path, &config).unwrap();
        assert!(path.exists());
        let content2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, content2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
