//! Internal PKI for the agent channel: a self-signed CA kept in `data_dir`
//! that signs client CSRs delivered inside `Hello` — mTLS with automatic
//! issuance + renewal, no cert tooling on the client side.
//!
//! Renewal flow: a client sends a CSR whenever it needs a cert (first
//! enroll, or <30d left on the current one); HelloAck returns a signed
//! cert + the CA chain. Cert expiry is checked locally before dialing —
//! an expired cert is replaced via the bootstrap (server-auth) listener,
//! which accepts any TLS client but still requires the device credential
//! at the app layer.

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, CertificateSigningRequestParams,
    DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Client certs live ~180d; clients renew when under 30d remain.
pub const CLIENT_CERT_DAYS: u32 = 180;
/// Issued certs carry the device id as CN for audit/debugging.
const CLIENT_CERT_LEEWAY_SECS: i64 = 60;

#[derive(Clone)]
pub struct ClientCa {
    cert_pem: String,
    inner: Arc<CaInner>,
    dir: PathBuf,
}

struct CaInner {
    cert: Certificate,
    key: KeyPair,
}

impl ClientCa {
    /// Load `data_dir/tls/ca.{crt,key}` or generate a fresh CA.
    pub fn load_or_create(data_dir: &str) -> Result<Option<Self>> {
        let dir = Path::new(data_dir).join("tls");
        let crt = dir.join("ca.crt");
        let key_path = dir.join("ca.key");
        let (cert_pem, key_pem) = match (
            std::fs::read_to_string(&crt),
            std::fs::read_to_string(&key_path),
        ) {
            (Ok(c), Ok(k)) => (c, k),
            _ => {
                std::fs::create_dir_all(&dir)?;
                let key = KeyPair::generate()?;
                let cert = ca_params().self_signed(&key)?;
                let c = cert.pem();
                let k = key.serialize_pem();
                std::fs::write(&crt, &c)?;
                write_private(&key_path, &k)?;
                (c, k)
            }
        };
        let key = KeyPair::from_pem(&key_pem).context("parse ca.key")?;
        // Rebuild the issuer cert FROM THE PERSISTED PEM — regenerating
        // params here would produce a different subject DN and break the
        // issuer chain against ca.crt in client trust stores.
        let params =
            CertificateParams::from_ca_cert_pem(&cert_pem).context("parse ca.crt for signing")?;
        let cert = params.self_signed(&key)?;
        Ok(Some(Self {
            cert_pem,
            inner: Arc::new(CaInner { cert, key }),
            dir,
        }))
    }

    /// PEM chain a client should trust (the CA cert itself for self-signed setups).
    pub fn ca_pem(&self) -> &str {
        &self.cert_pem
    }

    /// Directory holding the CA + generated certs.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Sign a client CSR; returns (cert_pem, expires_unix).
    pub fn sign_csr(&self, csr_pem: &str, client_id: &str) -> Result<(String, i64)> {
        let csr = CertificateSigningRequestParams::from_pem(csr_pem).context("invalid CSR")?;
        let mut params = csr.params;
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        // Bind the cert to the enrolled device id — replace whatever DN the
        // CSR claimed so a client cannot mint a cert naming another device.
        // The CN is informational; the app layer still authenticates via
        // the device credential.
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, format!("loom-client-{client_id}"));
        params.distinguished_name = dn;
        // SAN keeps verifiers happy even though client certs are checked by CA only.
        params.subject_alt_names = vec![SanType::DnsName(
            format!("{client_id}.clients.loom").try_into()?,
        )];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::seconds(CLIENT_CERT_LEEWAY_SECS);
        let not_after = now + time::Duration::days(CLIENT_CERT_DAYS as i64);
        params.not_after = not_after;
        let cert = params.signed_by(&csr.public_key, &self.inner.cert, &self.inner.key)?;
        Ok((cert.pem(), not_after.unix_timestamp()))
    }
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "LOOM Agent CA");
    params
}

fn write_private(path: &Path, pem: &str) -> Result<()> {
    std::fs::write(path, pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_persists_across_load() {
        let dir = std::env::temp_dir().join(format!("loom-ca-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ca1 = ClientCa::load_or_create(dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        let pem1 = ca1.ca_pem().to_string();
        // second load must reuse the same CA, not regenerate
        let ca2 = ClientCa::load_or_create(dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(pem1, ca2.ca_pem());
        // CA key file must be private
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("tls/ca.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sign_csr_binds_authenticated_id() {
        let dir = std::env::temp_dir().join(format!("loom-ca-sign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ca = ClientCa::load_or_create(dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        // CSR claiming a different CN than the authenticated id
        let key = KeyPair::generate().unwrap();
        let mut p = CertificateParams::default();
        p.distinguished_name
            .push(DnType::CommonName, "loom-client-IMPOSTOR");
        let csr = p.serialize_request(&key).unwrap().pem().unwrap();
        let (cert_pem, exp) = ca.sign_csr(&csr, "real-device").unwrap();
        assert!(exp > time::OffsetDateTime::now_utc().unix_timestamp());
        // the cert must carry the authenticated id, not the CSR's claim
        let pem_block = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
            .unwrap()
            .1;
        let parsed = x509_parser::parse_x509_certificate(&pem_block.contents)
            .unwrap()
            .1;
        let cn = parsed
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(cn, "loom-client-real-device");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sign_csr_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("loom-ca-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ca = ClientCa::load_or_create(dir.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert!(ca.sign_csr("not a pem", "dev").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
