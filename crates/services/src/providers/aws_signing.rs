//! SigV4 for the two fixed, read-only Bedrock catalog endpoints. Inference uses
//! the native Converse adapter. No credential process, shell, or env mutation.
use super::*;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

fn encode(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(result, "%{byte:02X}").expect("String writes cannot fail");
        }
    }
    result
}
fn hmac(key: &[u8], bytes: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of every length");
    mac.update(bytes);
    mac.finalize().into_bytes().to_vec()
}
#[allow(clippy::too_many_arguments)]
fn sign_get(
    url: &Url,
    region: &str,
    service: &str,
    access: &str,
    secret: &str,
    token: Option<&str>,
    date: &str,
    mut headers: BTreeMap<String, String>,
) -> Result<HeaderMap> {
    ensure!(
        date.len() == 16 && date.is_ascii(),
        "Invalid AWS signing timestamp"
    );
    let host = url.host_str().context("Missing AWS catalog host")?;
    let host = url
        .port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
    headers.insert("host".into(), host);
    headers.insert("x-amz-date".into(), date.into());
    if let Some(token) = token {
        headers.insert("x-amz-security-token".into(), token.into());
    }
    let mut query = url
        .query_pairs()
        .map(|(key, value)| (encode(&key), encode(&value)))
        .collect::<Vec<_>>();
    query.sort();
    let query = query
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical_headers = headers
        .iter()
        .map(|(key, value)| {
            format!(
                "{key}:{}\n",
                value.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect::<String>();
    let signed_headers = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical = format!(
        "GET\n{}\n{query}\n{canonical_headers}\n{signed_headers}\n{}",
        url.path(),
        hex::encode(Sha256::digest([]))
    );
    let short = &date[..8];
    let scope = format!("{short}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical))
    );
    let date_key = hmac(format!("AWS4{secret}").as_bytes(), short.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, service.as_bytes());
    let signing_key = hmac(&service_key, b"aws4_request");
    let signature = hmac(&signing_key, to_sign.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    headers.insert("authorization".into(),format!("AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"));
    let mut result = HeaderMap::new();
    for (key, value) in headers {
        result.insert(
            HeaderName::from_bytes(key.as_bytes())?,
            HeaderValue::from_str(&value)?,
        );
    }
    Ok(result)
}
pub(super) fn catalog_headers(
    url: &Url,
    region: &str,
    credential: &ProviderCredential,
) -> Result<HeaderMap> {
    ensure!(
        matches!(url.path(), "/foundation-models" | "/inference-profiles"),
        "Unsupported AWS catalog path"
    );
    let ProviderCredential::Aws {
        access_key_id,
        secret_access_key,
        session_token,
    } = credential
    else {
        bail!("AWS signing requires explicit AWS credentials")
    };
    sign_get(
        url,
        region,
        "bedrock",
        access_key_id,
        secret_access_key,
        session_token.as_deref(),
        &chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
        BTreeMap::new(),
    )
}

#[cfg(test)]
mod tests;
