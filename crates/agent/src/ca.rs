//! 自定义 CA 根证书：让自建服务器用私有 CA 签发的证书也能通过校验。
//!
//! 默认情况下被控端连中转、主控端连服务器都用 tokio-tungstenite 内置的公共根证书。
//! 自建中转常用自签证书或私有 CA 签发，公共根自然不认，握手会直接失败。
//! 用户在设置里指定 CA 文件或目录后，这里构造一个**只信任该 CA** 的连接器：
//! 语义与 `curl --cacert` 一致，配置即替代默认信任库，而不是在公共根之外追加。
//! 这样既能连上私有证书，也不会让这台机器的流量信任一堆无关的公共 CA。

use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, RootCertStore};
use tokio_tungstenite::Connector;

/// 目录扫描时认的扩展名。比较前统一转小写，所以 `.PEM`、`.Cer` 也能识别。
const CERT_EXTENSIONS: &[&str] = &["pem", "crt", "cer", "der"];

/// 从文件或目录读取根证书（PEM 可含多张；单张 DER 也支持；目录遍历 .pem/.crt/.cer/.der）。
/// 一张都没读到返回 Err。
pub fn load_roots(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    if path.is_dir() {
        load_dir(path)
    } else {
        // 不存在或不是目录时走这里，read 会给出带路径的 IO 错误。
        load_file(path)
    }
}

/// 用给定根证书（且仅用它们）构造 rustls ClientConfig。
/// `roots` 为空返回 Err —— 调用方不该在没根证书时用它。
pub fn client_config(roots: Vec<CertificateDer<'static>>) -> Result<Arc<ClientConfig>> {
    // 空集合意味着“谁都不信”，连建立连接都毫无意义。多数情况是用户配了路径
    // 但内容没读出来，尽早报错好过让它以 TLS 握手失败的形式出现。
    if roots.is_empty() {
        return Err(anyhow!("没有提供任何根证书，拒绝构造空信任库的 TLS 配置"));
    }

    // 显式指定 ring 加密提供者，不依赖进程级全局默认值：
    // 若调用方从未安装默认 provider，`ClientConfig::builder()` 会 panic，
    // 而这里自己带 provider 就不会受安装顺序影响。
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let mut store = RootCertStore::empty();
    // 任何一张加不进去都要报出来，不能静默跳过：
    // 用户以为自己配好了，结果握手仍然失败，很难排查。
    for (index, cert) in roots.into_iter().enumerate() {
        store
            .add(cert)
            .map_err(|err| anyhow!("根证书第 {} 张无法加入信任库: {err}", index + 1))?;
    }

    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("选择 TLS 协议版本失败")?
        .with_root_certificates(store)
        .with_no_client_auth();

    Ok(Arc::new(config))
}

/// 便捷入口：None 表示用户没配置（返回 Ok(None)，调用方走默认连接）；
/// Some(path) 则读根证书并返回可直接传给
/// `connect_async_tls_with_config(req, None, false, Some(connector))` 的连接器。
pub fn connector_from_path(path: Option<&Path>) -> Result<Option<Connector>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let roots = load_roots(path)?;
    Ok(Some(Connector::Rustls(client_config(roots)?)))
}

/// 读取单个证书文件。
fn load_file(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let bytes =
        fs::read(path).with_context(|| format!("读取 CA 证书文件失败: {}", path.display()))?;
    parse_cert_bytes(&bytes, &path.display().to_string())
}

/// 遍历目录（不递归）下的证书文件。
///
/// 单个文件读不了或解析不了时只警告并跳过——目录里通常放着一批证书，
/// 不该因为其中某个无关文件坏掉就整体失败。但一张都没读到时仍然报错。
fn load_dir(dir: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("读取 CA 证书目录失败: {}", dir.display()))?;

    let mut roots = Vec::new();
    let mut skipped = 0usize;

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::warn!("跳过无法读取的目录项 ({}): {err}", dir.display());
                skipped += 1;
                continue;
            }
        };

        let path = entry.path();
        // 只处理普通文件；目录不递归，避免用户误指到大目录时扫描失控。
        if !path.is_file() {
            continue;
        }

        // 扩展名不区分大小写。没有扩展名或不认识的文件直接略过，不当作错误。
        let is_cert_file = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .is_some_and(|ext| CERT_EXTENSIONS.contains(&ext.as_str()));
        if !is_cert_file {
            continue;
        }

        match load_file(&path) {
            Ok(certs) => {
                tracing::debug!("从 {} 读取到 {} 张 CA 证书", path.display(), certs.len());
                roots.extend(certs);
            }
            Err(err) => {
                tracing::warn!("跳过无法解析的 CA 证书文件 {}: {err:#}", path.display());
                skipped += 1;
            }
        }
    }

    if roots.is_empty() {
        return Err(anyhow!(
            "目录 {} 下没有读到任何可用的证书（跳过 {skipped} 个无法读取/解析的文件）",
            dir.display()
        ));
    }
    Ok(roots)
}

/// 解析一段文件内容：先按 PEM（可能多张）解析，没有 PEM 块时整体当成一张 DER。
fn parse_cert_bytes(bytes: &[u8], origin: &str) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = bytes;
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("{origin} 的 PEM 解析失败"))?;

    if !certs.is_empty() {
        // rustls_pemfile 只做 Base64 解码，不校验证书内容，这里统一验一遍。
        for (index, cert) in certs.iter().enumerate() {
            validate_cert(cert)
                .with_context(|| format!("{origin} 中第 {} 张证书无效", index + 1))?;
        }
        return Ok(certs);
    }

    // 没有 PEM 块：按 DER 单证书处理。DER 文件就是证书的原始字节。
    let der = CertificateDer::from(bytes.to_vec());
    validate_cert(&der).with_context(|| format!("{origin} 既不是 PEM 也不是有效的 DER 证书"))?;
    Ok(vec![der])
}

/// 确认字节真的是一张可用的 X.509 证书。
///
/// `CertificateDer::from` 只是包一层字节，不做任何解析；如果直接收下，
/// 用户把路径指到日志、配置文件时也会“成功”，直到连接时才报一个难懂的错。
/// 借助 rustls 的信任锚解析做一次校验，失败时能明确指出是哪个文件。
fn validate_cert(cert: &CertificateDer<'static>) -> Result<()> {
    RootCertStore::empty()
        .add(cert.clone())
        .map_err(|err| anyhow!("不是有效的 X.509 证书: {err}"))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use base64::Engine as _;

    use super::*;

    /// 固定的一张自签 CA 证书（RSA-2048 / SHA-256，仅用于解析测试，不参与握手）。
    const TEST_CERT_PEM: &str = concat!(
        "-----BEGIN CERTIFICATE-----\n",
        "MIIDXzCCAkegAwIBAgIUbFwbWcvqOFX0hFKcTg3YceI6pXEwDQYJKoZIhvcNAQEL\n",
        "BQAwNzEgMB4GA1UEAwwXU2NyZWVuV2FsbCBUZXN0IFJvb3QgQ0ExEzARBgNVBAoM\n",
        "ClNjcmVlbldhbGwwHhcNMjYxMTI0MTkwODM1WhcNMzYxMTIxMTkwODM1WjA3MSAw\n",
        "HgYDVQQDDBdTY3JlZW5XYWxsIFRlc3QgUm9vdCBDQTETMBEGA1UECgwKU2NyZWVu\n",
        "V2FsbDCCASIwDQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBAJ+kjd19DaBUQ+mg\n",
        "msglneRTO7Ra5FCcVY8QQ9qj9xAJ38RhEMayldNbkk4cLB525qBRlFwNKtlvlfI4\n",
        "DkoWu0och5R/4d0SSbyuSQtiPSv3qhzp3d/Uz9+80waG/feSB8iYRoXslGq89egU\n",
        "zfgVqQrc+f0G/uriiPeqDESE/MT8qxIL1IfA4UEFAcJCFwfkUI2RfoY3RH+NZNc5\n",
        "1kGt+pBMIItiiKO4Un8YKErfRIUB6BlABGYXX8HxpxJ3Smxbk6J13VuTPZMalvXt\n",
        "QUVnN+CRnW7uJwLPF/SznVgM/W04s24iYp+iDxNpBxEbShaLdojDUSLCMEcfU+IS\n",
        "OctI2q8CAwEAAaNjMGEwHQYDVR0OBBYEFJsOQt6jSB7l6U5Fxfr7uMdGa85VMB8G\n",
        "A1UdIwQYMBaAFJsOQt6jSB7l6U5Fxfr7uMdGa85VMA8GA1UdEwEB/wQFMAMBAf8w\n",
        "DgYDVR0PAQH/BAQDAgEGMA0GCSqGSIb3DQEBCwUAA4IBAQAHMK3Nza63I/LdasXA\n",
        "yfvLQlbAKrS0IHthf37EecTpU3y4zA2cFBwzQgvgi9b8vpFCdJPneIGQI3cgJ404\n",
        "JnYSh4nsyxaaI1qfNQ/HUgKR5DgoUdPdBVAy6D1Pt2hFK1uwKafGGJDH8rDxUj47\n",
        "boBKPQpqpYh8DBrM845bHiSkdgN0O3gHXZrDPt5ZijEhRkGkrXdWJ8bnlfAF+ta8\n",
        "y1+tyixVulyHTA+4ge5ruAu8Dsmi+ofLZh3D41UFgZCSYyqon4R3secPGbx7qqTD\n",
        "ty4LKpKy08a8zxKUsAv5b+2JUjlySsexQJtmwNkRSemOP04coBsrRMgv8kvja2af\n",
        "gcfX\n",
        "-----END CERTIFICATE-----\n",
    );

    /// 临时目录，Drop 时递归清理，保证测试不留下垃圾。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时间早于 UNIX_EPOCH")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "screenwall-ca-test-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("创建临时目录失败");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(dir: &Path, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("写测试文件失败");
        path
    }

    /// 从固定 PEM 里剥掉头尾，把 Base64 解码回 DER 字节。
    fn test_cert_der() -> Vec<u8> {
        let base64_body: String = TEST_CERT_PEM
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        base64::engine::general_purpose::STANDARD
            .decode(base64_body)
            .expect("测试证书 Base64 应可解码")
    }

    #[test]
    fn loads_single_pem_file() {
        let dir = TempDir::new("single");
        let path = write(dir.path(), "root.pem", TEST_CERT_PEM);

        let roots = load_roots(&path).expect("应能解析单张 PEM");
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn loads_pem_bundle_with_duplicate_cert() {
        let dir = TempDir::new("bundle");
        let bundle = format!("{TEST_CERT_PEM}{TEST_CERT_PEM}");
        let path = write(dir.path(), "bundle.pem", bundle);

        let roots = load_roots(&path).expect("应能解析包含两张证书的 PEM");
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn loads_der_file() {
        let dir = TempDir::new("der");
        let path = write(dir.path(), "root.der", test_cert_der());

        let roots = load_roots(&path).expect("应能按 DER 解析");
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn loads_directory_with_multiple_files() {
        let dir = TempDir::new("dir");
        let ca_dir = dir.path().join("ca");
        std::fs::create_dir_all(&ca_dir).expect("创建子目录失败");
        // 一张 .pem、一张大写扩展名的 .CRT，都应被读到。
        write(&ca_dir, "a.pem", TEST_CERT_PEM);
        write(&ca_dir, "b.CRT", TEST_CERT_PEM);
        // 扩展名不认识的文件应被忽略，不影响结果。
        write(&ca_dir, "readme.txt", "not a certificate");

        let roots = load_roots(&ca_dir).expect("目录里有两张有效证书");
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn missing_path_errors() {
        let dir = TempDir::new("missing");
        let path = dir.path().join("does-not-exist.pem");

        assert!(load_roots(&path).is_err(), "不存在的路径必须报错");
    }

    #[test]
    fn garbage_file_errors() {
        let dir = TempDir::new("garbage");
        let path = write(
            dir.path(),
            "junk.pem",
            "this is definitely not a certificate\nnor a PEM block\n",
        );

        assert!(load_roots(&path).is_err(), "垃圾内容必须报错而不是静默通过");
    }

    #[test]
    fn client_config_rejects_empty_roots() {
        // 空信任库没有意义，必须报错而不是构造出一个谁都连不上的配置。
        assert!(client_config(Vec::new()).is_err());
    }

    #[test]
    fn client_config_accepts_loaded_root() {
        let cert = CertificateDer::from(test_cert_der());
        let _config = client_config(vec![cert]).expect("有效根证书应能构造配置");
    }

    #[test]
    fn connector_none_without_path() {
        assert!(
            connector_from_path(None).expect("None 不应失败").is_none(),
            "没有配置自定义 CA 时应返回 None"
        );
    }
}
