//! Write-through to the Kubernetes ConfigMap so config survives pod restarts.
//!
//! When running in-cluster, the ServiceAccount token and CA cert are mounted
//! at standard paths. We PATCH `neolink-config` in the pod's own namespace
//! after every config write so the ConfigMap is always in sync with the live
//! config — and the next pod that starts picks it up automatically.

use anyhow::{Context, Result};

const TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
const CA_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";
const NS_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";
const K8S_HOST: &str = "https://kubernetes.default.svc";

/// Patch the `neolink-config` ConfigMap with the new TOML content.
/// Silently no-ops when not running in-cluster (missing token file).
pub(crate) async fn patch_configmap(toml: &str) -> Result<()> {
    // Not running in-cluster — skip silently.
    if !std::path::Path::new(TOKEN_PATH).exists() {
        return Ok(());
    }

    let token = std::fs::read_to_string(TOKEN_PATH).context("read SA token")?;
    let namespace = std::fs::read_to_string(NS_PATH)
        .unwrap_or_else(|_| "neolink".to_string());
    let namespace = namespace.trim();
    let ca = std::fs::read(CA_PATH).context("read SA CA")?;

    let cert = reqwest::Certificate::from_pem(&ca).context("parse CA cert")?;
    let client = reqwest::Client::builder()
        .add_root_certificate(cert)
        .build()
        .context("build k8s client")?;

    // Strategic merge patch: replace only the data.neolink.toml key.
    let body = serde_json::json!({
        "data": { "neolink.toml": toml }
    });

    let url = format!(
        "{K8S_HOST}/api/v1/namespaces/{namespace}/configmaps/neolink-config"
    );

    let resp = client
        .patch(&url)
        .bearer_auth(token.trim())
        .header("Content-Type", "application/strategic-merge-patch+json")
        .json(&body)
        .send()
        .await
        .context("PATCH configmap")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("ConfigMap PATCH {status}: {text}");
    }

    Ok(())
}
