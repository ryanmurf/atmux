//! Bounded, non-redirecting transport for platform clients.
use anyhow::{Result, bail, ensure};
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, body::Bytes};
use hyper_util::rt::TokioIo;
use std::{net::IpAddr, sync::Arc, time::Duration};

pub(crate) fn endpoint(url: &str, allow_http_hosts: &[String]) -> Result<url::Url> {
    ensure!(url.len() <= 2048, "endpoint too long");
    let url = url::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid endpoint"))?;
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "endpoint must not contain credentials, query or fragment"
    );
    ensure!(url.host_str().is_some(), "endpoint missing host");
    ensure!(
        url.scheme() == "https"
            || (url.scheme() == "http"
                && allow_http_hosts
                    .iter()
                    .any(|h| Some(h.as_str()) == url.host_str())),
        "endpoint requires HTTPS or an explicit private HTTP allowlist"
    );
    ensure!(
        url.scheme() != "http"
            || url
                .host_str()
                .is_none_or(|host| host.parse::<IpAddr>().map_or(true, private)),
        "HTTP address is not private"
    );
    Ok(url)
}
fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.segments()[0] & 0xfe00 == 0xfc00,
    }
}

pub(crate) async fn request(
    url: &url::Url,
    method: &str,
    headers: &[(&str, String)],
    body: Vec<u8>,
    timeout: u64,
    limit: usize,
) -> Result<(u16, Vec<u8>)> {
    ensure!(
        body.len() <= 512 * 1024 && (1..=300).contains(&timeout),
        "invalid HTTP request bounds"
    );
    tokio::time::timeout(Duration::from_secs(timeout), async {
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("missing host"))?;
        let tls = url.scheme() == "https";
        let addresses: Vec<_> =
            tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(443)))
                .await
                .map_err(|_| anyhow::anyhow!("endpoint DNS failed"))?
                .take(32)
                .collect();
        ensure!(
            !addresses.is_empty() && (tls || addresses.iter().all(|a| private(a.ip()))),
            "HTTP host resolved outside private addresses"
        );
        let stream = tokio::net::TcpStream::connect(addresses.as_slice())
            .await
            .map_err(|_| anyhow::anyhow!("endpoint connection failed"))?;
        let mut builder = Request::builder()
            .method(method)
            .uri(url.path())
            .header(
                "Host",
                &url[url::Position::BeforeHost..url::Position::AfterPort],
            )
            .header("Connection", "close");
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        let request = builder
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| anyhow::anyhow!("invalid HTTP headers"))?;
        let response = if tls {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let connector = tokio_rustls::TlsConnector::from(Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            ));
            let name = rustls::pki_types::ServerName::try_from(host.to_owned())
                .map_err(|_| anyhow::anyhow!("invalid TLS host"))?;
            let stream = connector
                .connect(name, stream)
                .await
                .map_err(|_| anyhow::anyhow!("endpoint TLS failed"))?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            sender.send_request(request).await
        } else {
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            sender.send_request(request).await
        }
        .map_err(|_| anyhow::anyhow!("endpoint request failed"))?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        let mut body = response.into_body();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame
                .map_err(|_| anyhow::anyhow!("endpoint body failed"))?
                .into_data()
            {
                if bytes.len() + data.len() > limit {
                    bail!("endpoint response exceeded bound");
                }
                bytes.extend_from_slice(&data);
            }
        }
        Ok((status, bytes))
    })
    .await
    .map_err(|_| anyhow::anyhow!("endpoint request timed out"))?
}

pub(crate) async fn json(
    url: &url::Url,
    headers: &[(&str, String)],
    value: &serde_json::Value,
    timeout: u64,
) -> Result<serde_json::Value> {
    let mut headers = headers.to_vec();
    headers.push(("Content-Type", "application/json".into()));
    let (status, bytes) = request(
        url,
        "POST",
        &headers,
        serde_json::to_vec(value)?,
        timeout,
        2 * 1024 * 1024,
    )
    .await?;
    ensure!(
        (200..300).contains(&status),
        "platform HTTP status {status}"
    );
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid platform JSON"))
}
