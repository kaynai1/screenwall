//! 自签证书的生成、加载与 TLS 配置。

use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use sha2::{Digest, Sha256};
use tokio_rustls::TlsAcceptor;

pub struct CertMaterial {
    pub certs: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    /// SHA-256 指纹，大写十六进制，冒号分隔。
    pub fingerprint: String,
}

/// 确保配置目录里有证书；没有就生成一张自签证书。
pub fn ensure_cert(dir: &Path) -> Result<CertMaterial> {
    fs::create_dir_all(dir)?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if !cert_path.exists() || !key_path.exists() {
        let generated = rcgen::generate_simple_self_signed(vec![
            "screenwall".to_string(),
            "localhost".to_string(),
        ])?;
        fs::write(&cert_path, generated.cert.pem())?;
        fs::write(&key_path, generated.key_pair.serialize_pem())?;
        tracing::info!("已生成自签证书: {}", cert_path.display());
    }

    let mut cert_reader = BufReader::new(File::open(&cert_path)?);
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(anyhow!("证书文件没有内容: {}", cert_path.display()));
    }

    let mut key_reader = BufReader::new(File::open(&key_path)?);
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or_else(|| anyhow!("私钥文件无效: {}", key_path.display()))?;

    let fingerprint = fingerprint(&certs[0]);
    Ok(CertMaterial {
        certs,
        key,
        fingerprint,
    })
}

pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    digest
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// 一行拿到可直接用于 `run_server` / `direct::run` 的 TLS 接受器。
/// 返回 `(acceptor, fingerprint)`；指纹供主控端首次连接时固定。
pub fn ensure_acceptor(dir: &Path) -> Result<(TlsAcceptor, String)> {
    let material = ensure_cert(dir)?;
    let config = server_config(&material)?;
    Ok((TlsAcceptor::from(config), material.fingerprint))
}

pub fn server_config(material: &CertMaterial) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(material.certs.clone(), material.key.clone_key())?;
    Ok(Arc::new(config))
}
