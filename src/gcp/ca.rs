//! The machine's local CA. Persisted under the machine dir so the guest trust
//! store and the webhook `caBundle` stay valid across VMM restarts; leaf
//! certificates are minted in memory at VMM start.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rcgen::{Certificate, CertificateParams, DnType, IsCa, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

const CA_NAME: &str = "vat local GCP CA";

pub struct Ca {
    cert: Certificate,
    key: KeyPair,
    pem: String,
}

fn ca_params() -> Result<CertificateParams> {
    let mut params = CertificateParams::new(Vec::new()).context("ca params")?;
    params.distinguished_name.push(DnType::CommonName, CA_NAME);
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2045, 1, 1);
    Ok(params)
}

fn files(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("ca.pem"), dir.join("ca.key"))
}

impl Ca {
    /// Load the CA from `dir`, creating it on first use.
    pub fn ensure(dir: &Path) -> Result<Self> {
        let (pem_path, key_path) = files(dir);
        let params = ca_params()?;
        if let (Ok(pem), Ok(key_pem)) = (
            std::fs::read_to_string(&pem_path),
            std::fs::read_to_string(&key_path),
        ) {
            let key = KeyPair::from_pem(&key_pem).context("parse the CA key")?;
            // Re-signing with the same key and subject yields a certificate
            // that verifies leaves exactly like the trusted one on disk.
            let cert = params.self_signed(&key).context("load the CA")?;
            return Ok(Self { cert, key, pem });
        }
        std::fs::create_dir_all(dir)?;
        let key = KeyPair::generate().context("CA key")?;
        let cert = params.self_signed(&key).context("self-sign the CA")?;
        let pem = cert.pem();
        crate::vm::write_atomic(&key_path, key.serialize_pem().as_bytes())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        crate::vm::write_atomic(&pem_path, pem.as_bytes())?;
        Ok(Self { cert, key, pem })
    }

    /// The CA certificate (PEM) that clients trust.
    pub fn pem(&self) -> &str {
        &self.pem
    }

    /// A TLS server config for the given DNS names and IP addresses.
    pub fn server_config(&self, dns: &[String], ips: &[&str]) -> Result<Arc<ServerConfig>> {
        let mut params = CertificateParams::new(dns.to_vec()).context("leaf params")?;
        for ip in ips {
            params
                .subject_alt_names
                .push(SanType::IpAddress(ip.parse().context("leaf ip")?));
        }
        params.distinguished_name.push(
            DnType::CommonName,
            dns.first().map(String::as_str).unwrap_or("vat"),
        );
        let key = KeyPair::generate().context("leaf key")?;
        let leaf = params
            .signed_by(&key, &self.cert, &self.key)
            .context("sign leaf")?;
        let chain: Vec<CertificateDer<'static>> = vec![leaf.der().clone()];
        let key = PrivateKeyDer::try_from(key.serialize_der())
            .map_err(|e| anyhow::anyhow!("leaf key: {e}"))?;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut cfg = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .context("TLS server config")?;
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(cfg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let first = Ca::ensure(dir.path()).unwrap();
        let again = Ca::ensure(dir.path()).unwrap();
        assert_eq!(first.pem(), again.pem());
        again
            .server_config(&["us-central1-docker.pkg.dev".into()], &["169.254.169.251"])
            .unwrap();
    }
}
