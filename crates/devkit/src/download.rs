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
use ureq::tls::{PemItem, RootCerts, TlsConfig};

fn agent() -> Result<ureq::Agent> {
    let mut tls = TlsConfig::builder();
    // Defaults use the compiled Mozilla roots. An explicit standard PEM bundle
    // is supported for private proxy roots; certificate verification stays on.
    if let Some(path) = std::env::var_os("SSL_CERT_FILE").filter(|v| !v.is_empty()) {
        let bytes = fs::read(&path).context("Cannot read SSL_CERT_FILE")?;
        ensure!(
            bytes.len() <= 8 * 1024 * 1024,
            "SSL_CERT_FILE exceeds its byte budget"
        );
        let mut certificates = Vec::new();
        for item in ureq::tls::parse_pem(&bytes) {
            if let PemItem::Certificate(certificate) = item? {
                certificates.push(certificate);
            }
        }
        ensure!(
            !certificates.is_empty(),
            "SSL_CERT_FILE contains no certificates"
        );
        tls = tls.root_certs(RootCerts::new_with_certs(&certificates));
    }
    Ok(ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(5)
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
    ensure!(
        maximum > 0 && maximum <= crate::archive::MAX_COMPRESSED,
        "Invalid download byte budget"
    );
    let uri: ureq::http::Uri = url.parse().context("Invalid download URL")?;
    ensure!(
        uri.scheme_str() == Some("https")
            && uri.host().is_some()
            && !uri.authority().is_some_and(|a| a.as_str().contains('@')),
        "Downloads require an HTTPS URL without user information"
    );
    runner.check_cancelled()?;
    let mut response = agent()?
        .get(url)
        .header("accept-encoding", "identity")
        .call()
        .context("HTTPS download failed")?;
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
