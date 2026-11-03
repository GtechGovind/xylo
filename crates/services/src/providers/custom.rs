use super::*;

pub(super) fn normalize(config: &ProviderConfig) -> Result<NormalizedProvider> {
    let custom = config
        .custom
        .as_ref()
        .context("Missing custom provider configuration")?;
    custom.validate()?;
    validate_auth_header(&custom.auth_header, &custom.auth_prefix)?;
    let base = provider_url(&config.endpoint)?;
    let family = config.effective_family();
    let protocol = protocol_capabilities(&family).protocol;
    let path = match family {
        ApiFamily::OpenAiChat => &custom.paths.chat_completions,
        ApiFamily::OpenAiResponses => &custom.paths.responses,
        ApiFamily::Anthropic => &custom.paths.messages,
        ApiFamily::Gemini => &custom.paths.generate_content,
        _ => bail!(
            "Custom routing supports Chat Completions, Responses, Messages and GenerateContent"
        ),
    };
    let inference_url = route(&base, path, &config.model)?;
    let stream_url = if family == ApiFamily::Gemini {
        route(&base, &custom.paths.stream_generate_content, &config.model)?
    } else {
        inference_url.clone()
    };
    let models_url = if let Some(url) = config
        .discovery_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        Some(provider_url(url)?)
    } else if custom.paths.models.is_empty() {
        None
    } else {
        Some(route(&base, &custom.paths.models, "")?)
    };
    // Validate every configured route up front, including routes not selected yet.
    for path in [
        &custom.paths.chat_completions,
        &custom.paths.responses,
        &custom.paths.messages,
        &custom.paths.generate_content,
        &custom.paths.stream_generate_content,
    ] {
        if !path.is_empty() {
            route(&base, path, "model")?;
        }
    }
    Ok(NormalizedProvider {
        base_url: base,
        inference_url,
        stream_url,
        models_url,
        protocol,
        aws_region: None,
    })
}

fn route(base: &Url, template: &str, model: &str) -> Result<Url> {
    ensure!(
        !template.trim().is_empty(),
        "The selected API route is disabled"
    );
    ensure!(
        !template.contains("://") && !template.starts_with("//") && !template.contains('\\'),
        "Custom API routes must be relative to the provider base URL"
    );
    let encoded: String = url::form_urlencoded::byte_serialize(model.as_bytes()).collect();
    let path = template
        .trim_start_matches('/')
        .replace("{model}", &encoded);
    ensure!(
        !path.contains(['{', '}']),
        "Only the {{model}} route placeholder is supported"
    );
    let mut root = base.clone();
    root.set_path(&format!("{}/", base.path().trim_end_matches('/')));
    let mut result = root.join(&path).context("Invalid custom API route")?;
    ensure!(
        result.origin() == base.origin() && result.path().starts_with(root.path()),
        "Custom API routes must stay under the provider base URL"
    );
    if result.query().is_none() {
        result.set_query(base.query());
    }
    provider_url(result.as_str())
}

fn validate_auth_header(name: &str, prefix: &str) -> Result<()> {
    let parsed =
        HeaderName::from_bytes(name.as_bytes()).context("Invalid authentication header name")?;
    ensure!(
        name.len() <= 128 && prefix.len() <= 128,
        "Authentication header configuration is too long"
    );
    ensure!(
        !TRANSPORT_HEADERS.contains(&parsed.as_str()) && !parsed.as_str().starts_with("proxy-"),
        "Authentication cannot override transport headers"
    );
    HeaderValue::from_str(prefix).context("Invalid authentication prefix")?;
    Ok(())
}

pub(super) fn secret_headers(
    headers: &mut HeaderMap,
    values: &BTreeMap<String, String>,
) -> Result<()> {
    ensure!(values.len() <= 16, "Use at most 16 credential headers");
    for (name, value) in values {
        validate_auth_header(name, "")?;
        ensure!(value.len() <= 16 * 1024, "Credential header is too long");
        let mut value = HeaderValue::from_str(value).context("Invalid credential header value")?;
        value.set_sensitive(true);
        headers.insert(HeaderName::from_bytes(name.as_bytes())?, value);
    }
    Ok(())
}

impl ProviderCredential {
    /// Authentication is a connection concern, independent of a model's wire protocol.
    pub fn headers_for(&self, config: &ProviderConfig) -> Result<HeaderMap> {
        let Some(custom) = &config.custom else {
            return self.headers(&config.family, &config.headers);
        };
        validate_auth_header(&custom.auth_header, &custom.auth_prefix)?;
        let mut headers = Self::None.headers(&config.effective_family(), &config.headers)?;
        match self {
            Self::ApiKey(secret) | Self::Bearer(secret) => {
                let values = BTreeMap::from([(
                    custom.auth_header.clone(),
                    format!("{}{secret}", custom.auth_prefix),
                )]);
                secret_headers(&mut headers, &values)?;
            }
            Self::Headers(values) => secret_headers(&mut headers, values)?,
            Self::None => {}
            _ => bail!(
                "Custom routing requires an API key, bearer token or credential-header record"
            ),
        }
        Ok(headers)
    }
}
