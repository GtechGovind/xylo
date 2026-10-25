use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use url::Url;
use workbench_core::MAX_IMAGE_BYTES;

pub(super) fn loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
}
pub(super) fn decode_image(data: &str) -> Result<Vec<u8>> {
    ensure!(
        data.len() <= ((MAX_IMAGE_BYTES as usize).div_ceil(3) * 4),
        "Generated image exceeds 8 MiB"
    );
    let bytes = STANDARD
        .decode(data)
        .context("Provider returned invalid image base64")?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_IMAGE_BYTES,
        "Generated image is empty or exceeds 8 MiB"
    );
    Ok(bytes)
}
pub(super) async fn read_bounded(
    mut response: reqwest::Response,
    max: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= max as u64),
        "Provider response exceeds its size limit"
    );
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {biased;_=cancel.cancelled()=>bail!("Image transfer cancelled"),chunk=response.chunk()=>chunk.map_err(|_|anyhow::anyhow!("Image transfer was interrupted"))?};
        let Some(chunk) = chunk else {
            break;
        };
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= max,
            "Provider response exceeds its size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
pub(super) fn decode_data_image(value: &str) -> Result<Vec<u8>> {
    let data = value
        .strip_prefix("data:")
        .context("Provider returned an invalid image data URL")?;
    let (mime, data) = data
        .split_once(";base64,")
        .context("Provider returned an invalid image data URL")?;
    ensure!(
        matches!(
            mime,
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        ),
        "Unsupported generated image type"
    );
    decode_image(data)
}

pub(super) async fn download_image(
    value: &str,
    provider: &Url,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    ensure!(
        value.len() <= 16 * 1024,
        "Provider returned an oversized image URL"
    );
    let url = Url::parse(value).context("Provider returned an invalid image URL")?;
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "Generated image URL must not contain credentials or a fragment"
    );
    let same_origin = url.origin() == provider.origin();
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && same_origin && loopback(provider)),
        "Generated image downloads require HTTPS, or the selected loopback origin"
    );
    let host = url.host_str().context("Generated image URL has no host")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = url
        .port_or_known_default()
        .context("Invalid image download port")?;
    let addresses = tokio::select! {_=cancel.cancelled()=>bail!("Image download cancelled"),addresses=tokio::time::timeout(Duration::from_secs(10),tokio::net::lookup_host((host,port)))=>addresses.context("Image download DNS deadline exceeded")?.context("Cannot resolve image download host")?.collect::<Vec<SocketAddr>>()};
    ensure!(
        !addresses.is_empty() && addresses.len() <= 32,
        "Image download DNS returned no usable address"
    );
    ensure!(
        same_origin || addresses.iter().all(|address| public_ip(address.ip())),
        "Provider output may not redirect image downloads into a private network"
    );
    // This separate client has no provider authentication, cookies or redirects.
    // Pin checked DNS answers so a second DNS resolution cannot change the target.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .resolve_to_addrs(host, &addresses)
        .build()?;
    let response = tokio::select! {_=cancel.cancelled()=>bail!("Image download cancelled"),response=client.get(url).send()=>response.map_err(|_|anyhow::anyhow!("Generated image download failed"))?};
    ensure!(
        response.status().is_success(),
        "Generated image download returned HTTP{}",
        response.status().as_u16()
    );
    read_bounded(response, MAX_IMAGE_BYTES as usize, cancel).await
}
fn public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !address.is_private()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_unspecified()
        && !address.is_multicast()
        && a != 0
        && a < 224
        && !(a == 100 && (64..=127).contains(&b))
        && !(a == 192 && b == 0 && (c == 0 || c == 2))
        && !(a == 198 && (b == 18 || b == 19 || b == 51 && c == 100))
        && !(a == 203 && b == 0 && c == 113)
}
fn public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => public_v4(address),
        IpAddr::V6(address) => address.to_ipv4_mapped().map_or_else(
            || {
                address.segments()[0] & 0xe000 == 0x2000
                    && address.segments()[0] != 0x2002
                    && !(address.segments()[0] == 0x2001
                        && matches!(address.segments()[1], 0 | 2 | 0xdb8))
                    && !address.is_loopback()
                    && !address.is_multicast()
            },
            public_v4,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn download_address_policy_excludes_local_and_metadata_targets() {
        for value in [
            "127.0.0.1",
            "10.2.3.4",
            "192.168.2.3",
            "169.254.169.254",
            "100.64.1.2",
            "::1",
            "fc00::1",
            "fe80::2",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public_ip(value.parse().unwrap()), "{value}");
        }
        assert!(public_ip("8.8.8.8".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
}
