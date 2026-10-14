//! TLS for server connections, configured by libpq-style URL parameters that
//! work the same for PostgreSQL and MySQL:
//!
//! - `sslmode=disable|prefer|require|verify-ca|verify-full`
//! - `sslrootcert=ca.pem`: CA to verify the server with (default: the Mozilla roots)
//! - `sslcert=client.pem` and `sslkey=client.key`: client certificate authentication
//!
//! [`split_url`] removes them before the URL reaches the driver.

use anyhow::{Context, Result, bail};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    /// Plain TCP.
    Disable,
    /// TLS if the server supports it, without verifying its certificate.
    Prefer,
    /// TLS required, without verifying the server certificate.
    Require,
    /// TLS required, server certificate signed by a trusted CA.
    VerifyCa,
    /// As `VerifyCa`, and the certificate must match the host name.
    VerifyFull,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct TlsParams {
    pub sslmode: Option<SslMode>,
    pub root_cert: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

impl TlsParams {
    /// The mode to connect with. Without `sslmode` it is `prefer`, or
    /// `require` when certificate files are given. As in libpq, `require`
    /// with `sslrootcert` verifies the CA.
    pub fn mode(&self) -> SslMode {
        let has_files = self.root_cert.is_some() || self.cert.is_some();
        match self.sslmode.unwrap_or(if has_files { SslMode::Require } else { SslMode::Prefer }) {
            SslMode::Require if self.root_cert.is_some() => SslMode::VerifyCa,
            mode => mode,
        }
    }

    /// The client certificate and key, both or neither.
    pub fn client_identity(&self) -> Result<Option<(&Path, &Path)>> {
        match (&self.cert, &self.key) {
            (Some(cert), Some(key)) => Ok(Some((cert, key))),
            (None, None) => Ok(None),
            _ => bail!("sslcert and sslkey must be given together"),
        }
    }

    /// Fail early, with the path in the message, if a file is missing.
    pub fn check_files(&self) -> Result<()> {
        self.client_identity()?;
        for (param, path) in [("sslrootcert", &self.root_cert), ("sslcert", &self.cert), ("sslkey", &self.key)] {
            if let Some(path) = path {
                anyhow::ensure!(path.is_file(), "{param}: {} is not a file", path.display());
            }
        }
        Ok(())
    }
}

/// Split the TLS parameters out of a connection URL. Other query parameters
/// stay in the returned URL.
pub fn split_url(url: &str) -> Result<(String, TlsParams)> {
    let Some((base, query)) = url.split_once('?') else {
        return Ok((url.to_string(), TlsParams::default()));
    };
    let mut params = TlsParams::default();
    let mut rest = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
        let value = || -> Result<String> {
            Ok(percent_encoding::percent_decode_str(raw)
                .decode_utf8()
                .with_context(|| format!("invalid {key} value"))?
                .into_owned())
        };
        match key {
            "sslmode" => {
                params.sslmode = Some(match value()?.as_str() {
                    "disable" => SslMode::Disable,
                    "prefer" | "allow" => SslMode::Prefer,
                    "require" => SslMode::Require,
                    "verify-ca" => SslMode::VerifyCa,
                    "verify-full" => SslMode::VerifyFull,
                    other => bail!(
                        "unknown sslmode '{other}' (expected disable, prefer, require, verify-ca or verify-full)"
                    ),
                })
            }
            "sslrootcert" => params.root_cert = Some(value()?.into()),
            "sslcert" => params.cert = Some(value()?.into()),
            "sslkey" => params.key = Some(value()?.into()),
            _ => rest.push(pair),
        }
    }
    let url = if rest.is_empty() { base.to_string() } else { format!("{base}?{}", rest.join("&")) };
    Ok((url, params))
}

/// Whether `host` is this machine: empty, `localhost`, a loopback address or
/// a Unix socket directory. Connections to it are not warned about.
pub fn is_local_host(host: &str) -> bool {
    let host = host.trim();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    host.is_empty()
        || host.starts_with('/')
        || host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || bare.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// A warning for a server URL whose TLS settings don't check who the server
/// is (`disable`, `prefer`, `require`), unless the host is local.
pub fn warning(url: &str) -> Option<String> {
    let (conn, _) = crate::connections::Connection::from_target("", url).ok()?;
    if conn.kind == crate::connections::Kind::Sqlite || is_local_host(&conn.host) {
        return None;
    }
    let host = &conn.host;
    let (_, params) = split_url(url).ok()?;
    match params.mode() {
        SslMode::Disable => Some(format!("TLS is off for {host} (sslmode=disable): the password and data travel unencrypted")),
        SslMode::Prefer => Some(format!(
            "the certificate of {host} is not verified, and plain TCP is used if the server offers no TLS \
             (sslmode=prefer); use sslmode=verify-full"
        )),
        SslMode::Require => {
            Some(format!("the certificate of {host} is not verified (sslmode=require); use sslmode=verify-full"))
        }
        SslMode::VerifyCa | SslMode::VerifyFull => None,
    }
}

/// Add advice to a connection error that comes from a certificate that
/// could not be verified.
pub fn explain(err: anyhow::Error, mode: SslMode) -> anyhow::Error {
    let verifies = matches!(mode, SslMode::VerifyCa | SslMode::VerifyFull);
    if verifies && format!("{err:#}").to_lowercase().contains("certificate") {
        err.context(format!(
            "the server certificate could not be verified with sslmode={}: set sslrootcert to the CA that signed \
             it, or sslmode=require to connect without checking",
            if mode == SslMode::VerifyCa { "verify-ca" } else { "verify-full" }
        ))
    } else {
        err
    }
}

/// The rustls provider every connection uses. Installed as the process
/// default too, for drivers that build their own `ClientConfig`.
pub fn provider() -> Arc<CryptoProvider> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let _ = CryptoProvider::install_default((*provider).clone());
    provider
}

/// A rustls client configuration for `mode` (anything but `Disable`).
pub fn client_config(params: &TlsParams, mode: SslMode) -> Result<ClientConfig> {
    params.check_files()?;
    let provider = provider();
    let mut roots = RootCertStore::empty();
    match &params.root_cert {
        Some(path) => {
            for cert in CertificateDer::pem_file_iter(path)
                .with_context(|| format!("reading sslrootcert {}", path.display()))?
            {
                roots.add(cert.with_context(|| format!("parsing sslrootcert {}", path.display()))?)?;
            }
            anyhow::ensure!(!roots.is_empty(), "sslrootcert {} contains no certificates", path.display());
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let builder = ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions()?;
    let builder = if mode == SslMode::VerifyFull {
        builder.with_root_certificates(roots)
    } else {
        let verifier = Verifier {
            webpki: WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build()?,
            check_chain: mode == SslMode::VerifyCa,
            algorithms: provider.signature_verification_algorithms,
        };
        builder.dangerous().with_custom_certificate_verifier(Arc::new(verifier))
    };
    Ok(match params.client_identity()? {
        Some((cert, key)) => {
            let chain = CertificateDer::pem_file_iter(cert)
                .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
                .with_context(|| format!("reading sslcert {}", cert.display()))?;
            anyhow::ensure!(!chain.is_empty(), "sslcert {} contains no certificates", cert.display());
            let key = PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading sslkey {}", key.display()))?;
            builder.with_client_auth_cert(chain, key).context("invalid client certificate or key")?
        }
        None => builder.with_no_client_auth(),
    })
}

/// Server certificate verification for `prefer`/`require` (none) and
/// `verify-ca` (chain only, any host name). Handshake signatures are always checked.
#[derive(Debug)]
struct Verifier {
    webpki: Arc<WebPkiServerVerifier>,
    check_chain: bool,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if !self.check_chain {
            return Ok(ServerCertVerified::assertion());
        }
        match self.webpki.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            result => result,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_tls_params_and_keeps_the_rest() {
        let (url, p) = split_url(
            "postgres://u@h/db?sslmode=verify-full&connect_timeout=5&sslrootcert=%2Ftmp%2Fca.pem&sslcert=c.pem&sslkey=c.key",
        )
        .unwrap();
        assert_eq!(url, "postgres://u@h/db?connect_timeout=5");
        assert_eq!(p.sslmode, Some(SslMode::VerifyFull));
        assert_eq!(p.root_cert.as_deref(), Some(Path::new("/tmp/ca.pem")));
        assert_eq!(p.client_identity().unwrap(), Some((Path::new("c.pem"), Path::new("c.key"))));

        let (url, p) = split_url("mysql://u@h/db?sslmode=disable").unwrap();
        assert_eq!(url, "mysql://u@h/db");
        assert_eq!(p.mode(), SslMode::Disable);
        assert!(split_url("mysql://u@h/db?sslmode=maybe").is_err());
    }

    #[test]
    fn mode_defaults() {
        assert_eq!(TlsParams::default().mode(), SslMode::Prefer);
        let with_cert = TlsParams { cert: Some("c".into()), key: Some("k".into()), ..Default::default() };
        assert_eq!(with_cert.mode(), SslMode::Require);
        let with_ca = TlsParams { root_cert: Some("ca".into()), ..Default::default() };
        assert_eq!(with_ca.mode(), SslMode::VerifyCa);
        let full = TlsParams { sslmode: Some(SslMode::VerifyFull), ..with_ca };
        assert_eq!(full.mode(), SslMode::VerifyFull);
    }

    #[test]
    fn warns_about_unverified_remote_servers() {
        assert!(warning("postgres://u@db.example.com/app").unwrap().contains("sslmode=prefer"));
        assert!(warning("mysql://u@db.example.com/app?sslmode=require").unwrap().contains("sslmode=require"));
        assert!(warning("postgres://u@10.0.0.5/app?sslmode=disable").unwrap().contains("unencrypted"));
        assert_eq!(warning("postgres://u@db.example.com/app?sslmode=verify-full"), None);
        assert_eq!(warning("postgres://u@db.example.com/app?sslmode=verify-ca"), None);
        for local in ["postgres://u@localhost/app", "postgres://u@127.0.0.1:5433/app", "mysql://u@[::1]/app", "app.db"] {
            assert_eq!(warning(local), None, "{local}");
        }
        assert!(is_local_host("") && is_local_host("/var/run/postgresql") && !is_local_host("10.0.0.5"));
    }

    #[test]
    fn rejects_incomplete_or_missing_files() {
        let half = TlsParams { cert: Some("c".into()), ..Default::default() };
        assert!(half.client_identity().is_err());
        let missing = TlsParams { root_cert: Some("/nonexistent/ca.pem".into()), ..Default::default() };
        let err = client_config(&missing, SslMode::VerifyCa).unwrap_err();
        assert!(format!("{err:#}").contains("/nonexistent/ca.pem"));
        assert!(client_config(&TlsParams::default(), SslMode::Require).is_ok());
    }
}
