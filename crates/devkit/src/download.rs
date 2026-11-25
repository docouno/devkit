//! HTTPS transport worker. The parent supervises this same ELF so cancellation
//! also interrupts DNS/TLS/read operations without an async runtime or curl.
use crate::{Result, files::*, processes::ProcessRunner};
use anyhow::{Context, ensure};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::Duration,
};
use ureq::tls::{Certificate, PemItem, RootCerts, TlsConfig};

const MAX_CA_BYTES: u64 = 8 * 1024 * 1024;

fn read_ca(path: &Path) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "CA bundle must be a regular PEM file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_CA_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_CA_BYTES,
        "CA bundle exceeds its byte budget"
    );
    Ok(bytes)
}

fn certificates(bytes: &[u8]) -> Result<Vec<Certificate<'static>>> {
    let mut certificates = Vec::new();
    for item in ureq::tls::parse_pem(bytes) {
        if let PemItem::Certificate(certificate) = item? {
            certificates.push(certificate);
        }
    }
    ensure!(
        !certificates.is_empty(),
        "CA bundle contains no certificates"
    );
    Ok(certificates)
}

/// DEVKIT_CA_CERT extends the same pinned Mozilla trust set in native HTTPS
/// and standalone pnpm. Serialize certificates only, never private PEM items.
pub(crate) fn additional_ca_bundle(path: &Path) -> Result<Vec<u8>> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let extra = certificates(&read_ca(path)?)?;
    let mut bytes = Vec::new();
    for der in extra.iter().map(|c| c.der()).chain(
        webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|c| c.as_ref()),
    ) {
        bytes.extend_from_slice(b"-----BEGIN CERTIFICATE-----\n");
        for line in STANDARD.encode(der).as_bytes().chunks(64) {
            bytes.extend_from_slice(line);
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(b"-----END CERTIFICATE-----\n");
    }
    ensure!(
        bytes.len() as u64 <= MAX_CA_BYTES,
        "CA bundle exceeds its byte budget"
    );
    Ok(bytes)
}

pub(crate) fn additional_ca_file_in(
    path: &Path,
    tmp_dir: &Path,
) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("devkit-ca-")
        .tempfile_in(tmp_dir)?;
    file.write_all(&additional_ca_bundle(path)?)?;
    file.flush()?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    Ok(file)
}

fn agent(allow_loopback_http: bool) -> Result<ureq::Agent> {
    let mut tls = TlsConfig::builder();
    // Defaults use the compiled Mozilla roots. An explicit standard PEM bundle
    // is supported for private proxy roots; certificate verification stays on.
    let standard = std::env::var_os("SSL_CERT_FILE").filter(|v| !v.is_empty());
    let bytes = if let Some(path) = standard {
        Some(read_ca(Path::new(&path))?)
    } else if let Some(path) = crate::native::resolve_ca_cert_path()? {
        Some(additional_ca_bundle(&path)?)
    } else {
        None
    };
    if let Some(bytes) = bytes {
        tls = tls.root_certs(RootCerts::new_with_certs(&certificates(&bytes)?));
    }
    Ok(ureq::Agent::config_builder()
        .https_only(!allow_loopback_http)
        .max_redirects(0)
        .max_response_header_size(64 * 1024)
        .timeout_global(Some(Duration::from_secs(90)))
        .timeout_resolve(Some(Duration::from_secs(10)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(60)))
        .timeout_per_read(Some(Duration::from_secs(10)))
        .tls_config(tls.build())
        .build()
        .new_agent())
}

pub fn fetch(url: &str, output: &Path, maximum: u64, runner: &ProcessRunner) -> Result<()> {
    fetch_with_options(url, output, maximum, runner, false)
}

fn validate_url(uri: &ureq::http::Uri, allow_loopback_http: bool) -> Result<()> {
    ensure!(
        uri.scheme_str() == Some("https")
            || (allow_loopback_http
                && uri.scheme_str() == Some("http")
                && matches!(uri.host(), Some("127.0.0.1" | "[::1]" | "localhost"))),
        "Downloads require HTTPS (HTTP is allowed only on explicit loopback fixtures)"
    );
    ensure!(
        uri.host().is_some() && !uri.authority().is_some_and(|a| a.as_str().contains('@')),
        "Download URLs must not contain user information"
    );
    Ok(())
}

fn redirect_uri(current: &ureq::http::Uri, location: &str) -> Result<ureq::http::Uri> {
    // Resolve relative references before validating the next origin. Never use
    // automatic redirects: authentication is scoped separately for every hop.
    let location = location.split('#').next().unwrap_or("");
    if location.starts_with("https:") || location.starts_with("http:") {
        return Ok(location.parse()?);
    }
    let scheme = current.scheme_str().context("Download URL has no scheme")?;
    if location.starts_with("//") {
        return Ok(format!("{scheme}:{location}").parse()?);
    }
    let authority = current
        .authority()
        .context("Download URL has no authority")?;
    let path = if location.is_empty() {
        current
            .path_and_query()
            .map(|v| v.as_str())
            .unwrap_or("/")
            .to_owned()
    } else if location.starts_with('?') {
        format!("{}{location}", current.path())
    } else if location.starts_with('/') {
        location.to_owned()
    } else {
        format!(
            "{}{location}",
            current
                .path()
                .rsplit_once('/')
                .map(|(p, _)| format!("{p}/"))
                .unwrap_or("/".into())
        )
    };
    let (path, query) = path
        .split_once('?')
        .map(|(p, q)| (p, Some(q)))
        .unwrap_or((&path, None));
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            value => segments.push(value),
        }
    }
    let mut path = segments.join("/");
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    if let Some(query) = query {
        path.push('?');
        path.push_str(query);
    }
    Ok(format!("{scheme}://{authority}{path}").parse()?)
}

pub fn fetch_with_options(
    url: &str,
    output: &Path,
    maximum: u64,
    runner: &ProcessRunner,
    allow_loopback_http: bool,
) -> Result<()> {
    ensure!(
        maximum > 0 && maximum <= crate::archive::MAX_COMPRESSED,
        "Invalid download byte budget"
    );
    let uri: ureq::http::Uri = url.parse().context("Invalid download URL")?;
    validate_url(&uri, allow_loopback_http)?;
    runner.check_cancelled()?;
    let primary = agent(allow_loopback_http)?;
    let bearer = std::env::var("DEVKIT_INTERNAL_FETCH_BEARER").ok();
    let basic = std::env::var("DEVKIT_INTERNAL_FETCH_BASIC").ok();
    ensure!(
        bearer.is_none() || basic.is_none(),
        "Ambiguous download authentication"
    );
    let authorization = bearer
        .map(|token| format!("Bearer {token}"))
        .or_else(|| basic.map(|auth| format!("Basic {auth}")))
        .map(|value| {
            let mut header = ureq::http::HeaderValue::from_str(&value)
                .map_err(|_| anyhow::anyhow!("Invalid download authentication value"))?;
            header.set_sensitive(true);
            Ok::<_, anyhow::Error>(header)
        })
        .transpose()?;
    let mut hop = uri.clone();
    let mut redirects = 0;
    let mut response = loop {
        runner.check_cancelled()?;
        let request = |agent: &ureq::Agent| {
            let mut request = agent
                .get(hop.to_string())
                .header("accept-encoding", "identity");
            if hop.scheme() == uri.scheme()
                && hop.authority() == uri.authority()
                && let Some(authorization) = &authorization
            {
                request = request.header("authorization", authorization.clone());
            }
            request.call()
        };
        let response = request(&primary).context("HTTPS download failed")?;
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            break response;
        }
        ensure!(redirects < 5, "Too many download redirects");
        let next = redirect_uri(
            &hop,
            response
                .headers()
                .get("location")
                .context("Missing redirect Location")?
                .to_str()?,
        )?;
        validate_url(&next, allow_loopback_http)?;
        ensure!(
            hop.scheme_str() != Some("https") || next.scheme_str() == Some("https"),
            "Refusing HTTPS downgrade redirect"
        );
        hop = next;
        redirects += 1;
    };
    ensure!(
        response.status().as_u16() == 200,
        "Download did not return a complete HTTP 200 response"
    );
    if let Some(value) = response.headers().get("content-encoding") {
        ensure!(value == "identity", "Unexpected HTTP content encoding");
    }
    let declared = response
        .headers()
        .get("content-length")
        .map(|value| {
            value
                .to_str()?
                .parse::<u64>()
                .context("Invalid download Content-Length")
        })
        .transpose()?;
    ensure!(
        declared.is_none_or(|n| n <= maximum),
        "Download exceeds its byte budget"
    );
    let output = project_root(output)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .mode(0o600)
        .open(&output)?;
    let original = file.metadata()?;
    let result = (|| -> Result<()> {
        let mut body = response.body_mut().as_reader();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            runner.check_cancelled()?;
            let count = body.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .context("Download size overflow")?;
            ensure!(total <= maximum, "Download exceeds its byte budget");
            file.write_all(&buffer[..count])?;
        }
        ensure!(
            declared.is_none_or(|n| n == total),
            "Truncated download body"
        );
        runner.check_cancelled()?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        file.sync_all()?;
        sync_directory(output.parent().context("Download output parent")?)
    })();
    if result.is_err()
        && fs::symlink_metadata(&output)
            .is_ok_and(|m| m.dev() == original.dev() && m.ino() == original.ino())
    {
        let _ = fs::remove_file(&output);
    }
    result
}

#[cfg(test)]
mod trust_tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};

    #[test]
    fn additional_ca_preserves_public_roots_and_only_serializes_certificates() {
        let temporary = tempfile::TempDir::new().unwrap();
        let path = temporary.path().join("extra.pem");
        let extra = webpki_root_certs::TLS_SERVER_ROOT_CERTS[0].as_ref();
        fs::write(&path, format!("# discarded operator comment\n-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", STANDARD.encode(extra))).unwrap();
        let bundle = additional_ca_bundle(&path).unwrap();
        let parsed = certificates(&bundle).unwrap();
        assert_eq!(
            parsed.len(),
            webpki_root_certs::TLS_SERVER_ROOT_CERTS.len() + 1
        );
        assert_eq!(parsed[0].der(), extra);
        for (actual, expected) in parsed[1..]
            .iter()
            .zip(webpki_root_certs::TLS_SERVER_ROOT_CERTS)
        {
            assert_eq!(actual.der(), expected.as_ref());
        }
        assert!(
            !String::from_utf8(bundle)
                .unwrap()
                .contains("operator comment")
        );
        fs::write(&path, b"not a PEM certificate").unwrap();
        assert!(additional_ca_bundle(&path).is_err());
        fs::write(&path, vec![b'x'; MAX_CA_BYTES as usize + 1]).unwrap();
        assert!(
            additional_ca_bundle(&path)
                .unwrap_err()
                .to_string()
                .contains("byte budget")
        );
    }

    #[test]
    fn full_certificates_and_native_trust_anchors_use_the_same_mozilla_release() {
        let lock: toml::Value = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../Cargo.lock"
        )))
        .unwrap();
        let version = |name: &str| {
            lock["package"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["name"].as_str() == Some(name))
                .unwrap()["version"]
                .as_str()
                .unwrap()
        };
        assert_eq!(version("webpki-root-certs"), version("webpki-roots"));
    }
}
