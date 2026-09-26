//! rustls без глобального состояния: конфиги берут провайдер ring явно.

use std::{path::Path, sync::Arc};

use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    crypto::{CryptoProvider, ring},
    pki_types::CertificateDer,
};

use crate::{config::TlsFiles, transport::TransportError};

/// Ставит ring провайдером процесса: его ждут клиенты, строящие конфиг без
/// явного провайдера. Зовёт бинарь при старте; повторный вызов безвреден.
pub fn install_crypto_provider() {
    _ = ring::default_provider().install_default();
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

fn tls_err(e: impl std::fmt::Display) -> TransportError {
    TransportError::Tls(e.to_string())
}

pub async fn server_config(files: &TlsFiles) -> Result<Arc<ServerConfig>, TransportError> {
    let certs = read_certs(&files.cert).await?;
    let key_pem = tokio::fs::read(&files.key)
        .await
        .map_err(|e| TransportError::Tls(format!("{}: {e}", files.key.display())))?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(tls_err)?
        .ok_or_else(|| TransportError::Tls(format!("{}: no private key", files.key.display())))?;

    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(tls_err)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(tls_err)?;
    Ok(Arc::new(config))
}

/// Клиент, доверяющий только CA из `ca`.
pub async fn client_config(ca: &Path) -> Result<Arc<ClientConfig>, TransportError> {
    let mut roots = RootCertStore::empty();
    for cert in read_certs(ca).await? {
        roots.add(cert).map_err(tls_err)?;
    }

    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(tls_err)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

async fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TransportError> {
    let pem = tokio::fs::read(path)
        .await
        .map_err(|e| TransportError::Tls(format!("{}: {e}", path.display())))?;
    let certs = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(tls_err)?;
    if certs.is_empty() {
        return Err(TransportError::Tls(format!(
            "{}: no certificates",
            path.display()
        )));
    }
    Ok(certs)
}
