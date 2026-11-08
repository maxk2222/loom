//! Client-side mTLS material: persistent keypair, CSR generation, and a
//! small cert store in the client data dir. Certs are issued by the server
//! CA (CSR exchange inside Hello) and renewed automatically when fewer than
//! `RENEW_BEFORE_SECS` remain — renewal only matters before a reconnect,
//! since an established TLS session outlives cert expiry.

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use std::path::{Path, PathBuf};

/// Renew when less than 30d remain on the client cert.
const RENEW_BEFORE_SECS: i64 = 30 * 24 * 3600;

pub struct CertStore {
    dir: PathBuf,
}

impl CertStore {
    /// `data_dir`/tls — holds `client.key`, `client.crt`, `client.meta`.
    pub fn new(data_dir: &str) -> Result<Self> {
        let dir = Path::new(data_dir).join("tls");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join("client.key")
    }
    fn crt_path(&self) -> PathBuf {
        self.dir.join("client.crt")
    }
    fn meta_path(&self) -> PathBuf {
        self.dir.join("client.meta")
    }

    /// Load or generate the persistent keypair (0600).
    fn keypair(&self) -> Result<KeyPair> {
        if let Ok(pem) = std::fs::read_to_string(self.key_path()) {
            return KeyPair::from_pem(&pem).context("parse client.key");
        }
        let kp = KeyPair::generate()?;
        std::fs::write(self.key_path(), kp.serialize_pem())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(self.key_path(), std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(kp)
    }

    /// A fresh CSR PEM for the persistent keypair.
    pub fn csr_pem(&self, client_id: &str) -> Result<String> {
        let kp = self.keypair()?;
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            format!("loom-client-{client_id}"),
        );
        let csr = params.serialize_request(&kp)?;
        Ok(csr.pem()?)
    }

    /// Current cert PEM + its expiry (unix), if stored.
    pub fn cert(&self) -> Option<(String, i64)> {
        let crt = std::fs::read_to_string(self.crt_path()).ok()?;
        let exp: i64 = std::fs::read_to_string(self.meta_path())
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some((crt, exp))
    }

    /// Cert+key usable for the TLS handshake: stored and not yet expired.
    /// (Longer renewal margin is enforced by `needs_renewal`.)
    pub fn handshake_material(&self) -> Option<(String, String)> {
        let (crt, exp) = self.cert()?;
        let now = now_unix();
        if exp > now {
            let key = std::fs::read_to_string(self.key_path()).ok()?;
            Some((crt, key))
        } else {
            None
        }
    }

    /// Seconds until the stored cert expires (negative = expired/missing).
    pub fn expires_in(&self) -> i64 {
        match self.cert() {
            Some((_, exp)) => exp - now_unix(),
            None => i64::MIN,
        }
    }

    /// A renewal CSR is warranted: no cert, or under RENEW_BEFORE_SECS left.
    pub fn needs_renewal(&self) -> bool {
        self.expires_in() < RENEW_BEFORE_SECS
    }

    /// Persist an issued cert + expiry.
    pub fn save(&self, cert_pem: &str, expires_unix: i64) -> Result<()> {
        std::fs::write(self.crt_path(), cert_pem)?;
        std::fs::write(self.meta_path(), expires_unix.to_string())?;
        Ok(())
    }

    /// Persist the server CA received via HelloAck (pins it for bootstrap).
    pub fn save_ca(&self, ca_pem: &str) -> Result<()> {
        std::fs::write(self.dir.join("server-ca.crt"), ca_pem)?;
        Ok(())
    }

    /// Previously pinned server CA PEM, if any.
    pub fn saved_ca(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("server-ca.crt")).ok()
    }

    /// Drop the stored cert (keep the key — it can be re-enrolled).
    /// Used when the cert no longer chains to the live CA (rotation).
    pub fn clear_cert(&self) {
        let _ = std::fs::remove_file(self.crt_path());
        let _ = std::fs::remove_file(self.meta_path());
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
