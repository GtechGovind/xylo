use super::*;
use serde_json::Value;
use xylo_core::{CustomApiPaths, CustomProviderConfig};

#[derive(Clone, Debug)]
pub struct OpenApiImport {
    pub endpoint: String,
    pub custom: CustomProviderConfig,
    pub family: ApiFamily,
    pub warnings: Vec<String>,
}

/// Import routing and authentication metadata only. No references are fetched and
/// no examples, request defaults, or credential values are copied from the document.
pub fn import_openapi(document: &str, current_endpoint: &str) -> Result<OpenApiImport> {
    ensure!(
        document.len() <= 8 * 1024 * 1024,
        "OpenAPI JSON exceeds 8 MiB"
    );
    let doc: Value =
        serde_json::from_str(document).context("Paste a valid OpenAPI JSON document")?;
    let swagger = doc.get("swagger").and_then(Value::as_str) == Some("2.0");
    ensure!(
        swagger
            || doc
                .get("openapi")
                .and_then(Value::as_str)
                .is_some_and(|v| v.starts_with("3.")),
        "Use an OpenAPI 3 or Swagger 2 JSON document"
    );
    let paths = doc
        .get("paths")
        .and_then(Value::as_object)
        .context("OpenAPI document requires a paths object")?;
    let mut warnings = Vec::new();
    let base = if swagger {
        swagger_server(&doc, current_endpoint)?
    } else {
        select_server(doc.get("servers"), current_endpoint, &mut warnings)?
    };
    let mut custom = CustomProviderConfig {
        paths: CustomApiPaths {
            chat_completions: String::new(),
            responses: String::new(),
            messages: String::new(),
            generate_content: String::new(),
            stream_generate_content: String::new(),
            models: String::new(),
        },
        ..Default::default()
    };
    let mut auth = BTreeMap::<String, (String, String)>::new();
    let mut security_shapes = Vec::new();
    for (path, item) in paths {
        let Some(item) = resolve_local(&doc, item) else {
            warnings.push(format!("Skipped an unresolved path reference at {path}; external references are not fetched."));
            continue;
        };
        for method in ["post", "get"] {
            let Some(operation) = item.get(method).filter(|value| value.is_object()) else {
                continue;
            };
            let actions = if method == "post" && path.ends_with("/models/{modelAction}") {
                let actions = documented_model_actions(&doc, item, operation);
                if actions == (false, false) {
                    warnings.push("The modelAction parameter does not document GenerateContent or StreamGenerateContent actions; that Gemini route was left disabled.".into());
                }
                actions
            } else {
                (false, false)
            };
            let routes = recognized_routes(path, method, actions)?;
            if routes.is_empty() {
                continue;
            }
            if let Some(servers) = operation.get("servers").or_else(|| item.get("servers")) {
                let operation_base = select_server(Some(servers), base.as_str(), &mut warnings)?;
                ensure!(
                    same_server(&operation_base, &base),
                    "A supported operation uses a different server; configure separate custom providers for these servers"
                );
            }
            if let Some(schemes) = operation.get("schemes").and_then(Value::as_array) {
                ensure!(
                    schemes.iter().any(|s| s.as_str() == Some(base.scheme())),
                    "A supported operation uses a different URL scheme"
                );
            }
            for (kind, route) in routes {
                let destination = match kind {
                    Route::Chat => &mut custom.paths.chat_completions,
                    Route::Responses => &mut custom.paths.responses,
                    Route::Messages => &mut custom.paths.messages,
                    Route::Generate => &mut custom.paths.generate_content,
                    Route::Stream => &mut custom.paths.stream_generate_content,
                    Route::Models => &mut custom.paths.models,
                };
                ensure!(
                    destination.is_empty() || *destination == route,
                    "Multiple documented routes match the same API; use a document with one route per API"
                );
                *destination = route;
            }
            let security = operation.get("security").or_else(|| doc.get("security"));
            let shape = security.cloned().unwrap_or(Value::Null);
            if !security_shapes.contains(&shape) {
                security_shapes.push(shape);
            }
            read_auth(&doc, security, swagger, &mut auth, &mut warnings)?;
            for parameters in [item.get("parameters"), operation.get("parameters")] {
                if let Some(parameters) = parameters.and_then(Value::as_array) {
                    for parameter in parameters {
                        let Some(parameter) = resolve_local(&doc, parameter) else {
                            warnings.push(
                                "An unresolved parameter reference needs manual review.".into(),
                            );
                            continue;
                        };
                        if parameter.get("required").and_then(Value::as_bool) == Some(true)
                            && parameter.get("in").and_then(Value::as_str) == Some("header")
                            && let Some(name) = parameter.get("name").and_then(Value::as_str)
                        {
                            warnings.push(format!("The API requires header {name}; enter its value manually in headers or the JSON credential record. Document examples are not imported."));
                        }
                    }
                }
            }
        }
    }
    let family = if !custom.paths.chat_completions.is_empty() {
        ApiFamily::OpenAiChat
    } else if !custom.paths.responses.is_empty() {
        ApiFamily::OpenAiResponses
    } else if !custom.paths.messages.is_empty() {
        ApiFamily::Anthropic
    } else if !custom.paths.generate_content.is_empty()
        && !custom.paths.stream_generate_content.is_empty()
    {
        ApiFamily::Gemini
    } else {
        bail!(
            "No supported conversational API was found. Use POST Chat Completions, Responses, Messages, or both Gemini GenerateContent and StreamGenerateContent routes."
        )
    };
    if custom.paths.generate_content.is_empty() != custom.paths.stream_generate_content.is_empty() {
        warnings.push("Only one Gemini operation is documented; Gemini requires both GenerateContent and StreamGenerateContent routes before use.".into());
    }
    if security_shapes.len() > 1 {
        warnings.push("Authentication differs between operations; review which credentials each API and model catalog requires.".into());
    }
    if let Some((name, prefix)) = auth.get("authorization").or_else(|| auth.values().next()) {
        custom.auth_header = name.clone();
        custom.auth_prefix = prefix.clone();
    }
    if auth.len() > 1 {
        warnings.push(format!("Multiple authentication headers are required: {}. Enter a JSON credential record with a headers object containing their complete values; prefixes must be included in those values.", auth.values().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if custom.paths.models.is_empty() {
        warnings.push("No GET models route is documented; model discovery is disabled. Enter a model ID manually.".into());
    }
    let endpoint = base.as_str().trim_end_matches('/').to_owned();
    normalize_provider(&ProviderConfig {
        endpoint: endpoint.clone(),
        custom: Some(custom.clone()),
        family: family.clone(),
        model: "import-validation".into(),
        ..Default::default()
    })
    .context("Imported custom provider configuration is not supported")?;
    warnings.sort();
    warnings.dedup();
    Ok(OpenApiImport {
        endpoint,
        custom,
        family,
        warnings,
    })
}

#[derive(Clone, Copy)]
enum Route {
    Chat,
    Responses,
    Messages,
    Generate,
    Stream,
    Models,
}

fn recognized_routes(
    path: &str,
    method: &str,
    actions: (bool, bool),
) -> Result<Vec<(Route, String)>> {
    let route = path.trim_start_matches('/');
    if method == "get" {
        return Ok(if path.ends_with("/models") {
            vec![(Route::Models, route.into())]
        } else {
            vec![]
        });
    }
    for (suffix, kind) in [
        ("/chat/completions", Route::Chat),
        ("/responses", Route::Responses),
        ("/messages", Route::Messages),
    ] {
        if path.ends_with(suffix) {
            return Ok(vec![(kind, route.into())]);
        }
    }
    if let Some(prefix) = path.strip_suffix("/models/{modelAction}") {
        let prefix = prefix.trim_start_matches('/');
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        let mut routes = Vec::new();
        if actions.0 {
            routes.push((
                Route::Generate,
                format!("{prefix}models/{{model}}:generateContent"),
            ));
        }
        if actions.1 {
            routes.push((
                Route::Stream,
                format!("{prefix}models/{{model}}:streamGenerateContent?alt=sse"),
            ));
        }
        return Ok(routes);
    }
    for (suffix, kind) in [
        (":generateContent", Route::Generate),
        (":streamGenerateContent", Route::Stream),
    ] {
        if let Some(stem) = path.strip_suffix(suffix) {
            let (prefix, parameter) = stem
                .rsplit_once("/models/")
                .context("Gemini routes must contain /models/{model}")?;
            ensure!(
                parameter.starts_with('{')
                    && parameter.ends_with('}')
                    && !parameter[1..parameter.len() - 1].contains(['{', '}', '/']),
                "Gemini routes require a model path parameter"
            );
            let query = if matches!(kind, Route::Stream) {
                "?alt=sse"
            } else {
                ""
            };
            return Ok(vec![(
                kind,
                format!("{prefix}/models/{{model}}{suffix}{query}")
                    .trim_start_matches('/')
                    .to_owned(),
            )]);
        }
    }
    Ok(vec![])
}

fn documented_model_actions(doc: &Value, item: &Value, operation: &Value) -> (bool, bool) {
    // Operation parameters replace matching path-item parameters. An unresolved
    // reference cannot establish which action is available, so fail closed.
    for parameters in [operation.get("parameters"), item.get("parameters")] {
        let Some(parameters) = parameters.and_then(Value::as_array) else {
            continue;
        };
        let Some(parameters) = parameters
            .iter()
            .map(|parameter| resolve_local(doc, parameter))
            .collect::<Option<Vec<_>>>()
        else {
            return (false, false);
        };
        let Some(parameter) = parameters.into_iter().find(|parameter| {
            parameter.get("name").and_then(Value::as_str) == Some("modelAction")
                && parameter.get("in").and_then(Value::as_str) == Some("path")
        }) else {
            continue;
        };
        let schema = match parameter.get("schema") {
            Some(schema) => match resolve_local(doc, schema) {
                Some(schema) => schema,
                None => return (false, false),
            },
            None => parameter, // Swagger 2 puts enum directly on the parameter.
        };
        let strings: Vec<&str> = if let Some(values) = schema.get("enum") {
            // A constrained enum takes precedence over descriptive examples.
            values
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect()
        } else {
            [parameter.get("description"), schema.get("description")]
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect()
        };
        let has_action = |action: &str| {
            strings.iter().any(|value| {
                value
                    .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                    .any(|word| word == action)
            })
        };
        return (
            has_action("generateContent"),
            has_action("streamGenerateContent"),
        );
    }
    (false, false)
}

fn same_server(left: &Url, right: &Url) -> bool {
    left.origin() == right.origin()
        && left.path().trim_end_matches('/') == right.path().trim_end_matches('/')
        && left.query() == right.query()
}

fn select_server(
    servers: Option<&Value>,
    current: &str,
    warnings: &mut Vec<String>,
) -> Result<Url> {
    let Some(servers) = servers else {
        return provider_url(current)
            .context("Enter a base URL because this document has no server URL");
    };
    let servers = servers
        .as_array()
        .context("OpenAPI servers must be an array")?;
    if servers.is_empty() {
        return provider_url(current)
            .context("Enter a base URL because this document has no server URL");
    }
    let mut candidates = Vec::new();
    for server in servers {
        let raw = server
            .get("url")
            .and_then(Value::as_str)
            .context("OpenAPI server requires a URL")?;
        ensure!(
            !raw.contains(['{', '}']),
            "Resolve server URL variables in the OpenAPI document before importing"
        );
        let url = if Url::parse(raw).is_ok() {
            raw.to_owned()
        } else {
            let mut origin = provider_url(current)
                .context("Enter a base URL to resolve this relative server URL")?;
            origin.set_path("/");
            origin.set_query(None);
            origin
                .join(raw)
                .context("Invalid relative server URL")?
                .to_string()
        };
        candidates.push(provider_url(&url)?);
    }
    if let Ok(current) = provider_url(current)
        && let Some(server) = candidates
            .iter()
            .find(|server| same_server(server, &current))
    {
        return Ok(server.clone());
    }
    if candidates.len() > 1 {
        warnings.push("The document lists multiple servers; the first server was selected. Review the base URL.".into());
    }
    Ok(candidates.remove(0))
}

fn swagger_server(doc: &Value, current: &str) -> Result<Url> {
    let current = provider_url(current).ok();
    let mut base = if let Some(host) = doc.get("host").and_then(Value::as_str) {
        let schemes = doc.get("schemes").and_then(Value::as_array);
        let scheme = schemes
            .and_then(|values| values.iter().find(|s| s.as_str() == Some("https")))
            .and_then(Value::as_str)
            .or_else(|| {
                schemes
                    .and_then(|values| values.first())
                    .and_then(Value::as_str)
            })
            .or_else(|| current.as_ref().map(Url::scheme))
            .unwrap_or("https");
        provider_url(&format!("{scheme}://{host}"))?
    } else {
        current.context("Enter a base URL because this Swagger document has no host")?
    };
    if let Some(path) = doc.get("basePath").and_then(Value::as_str) {
        ensure!(
            path.starts_with('/') && !path.contains(['?', '#', '{', '}']),
            "Invalid Swagger basePath"
        );
        base.set_path(path);
    }
    provider_url(base.as_str())
}

fn resolve_local<'a>(doc: &'a Value, mut value: &'a Value) -> Option<&'a Value> {
    for _ in 0..16 {
        let Some(reference) = value.get("$ref") else {
            return Some(value);
        };
        let pointer = reference.as_str()?.strip_prefix('#')?;
        value = doc.pointer(pointer)?;
    }
    None
}

fn read_auth(
    doc: &Value,
    security: Option<&Value>,
    swagger: bool,
    auth: &mut BTreeMap<String, (String, String)>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let Some(security) = security else {
        return Ok(());
    };
    let Some(options) = security.as_array() else {
        warnings
            .push("Unsupported authentication declaration; configure credentials manually.".into());
        return Ok(());
    };
    if options.len() > 1 {
        warnings.push("The document permits alternative authentication methods; review the imported authentication choice.".into());
    }
    if options.is_empty()
        || options
            .iter()
            .any(|option| option.as_object().is_some_and(|o| o.is_empty()))
    {
        return Ok(());
    }
    let schemes = if swagger {
        doc.get("securityDefinitions")
    } else {
        doc.pointer("/components/securitySchemes")
    };
    // Alternatives are OR; schemes within one alternative are AND. Never combine
    // mutually exclusive alternatives into a single set of required credentials.
    for option in options {
        let Some(requirements) = option.as_object() else {
            continue;
        };
        let mut selected = Vec::new();
        let mut supported = true;
        for name in requirements.keys() {
            let scheme = schemes
                .and_then(|schemes| schemes.get(name))
                .and_then(|scheme| resolve_local(doc, scheme));
            let header =
                scheme.and_then(|scheme| match scheme.get("type").and_then(Value::as_str) {
                    Some("http")
                        if scheme
                            .get("scheme")
                            .and_then(Value::as_str)
                            .is_some_and(|s| s.eq_ignore_ascii_case("bearer")) =>
                    {
                        Some(("Authorization".to_owned(), "Bearer ".to_owned()))
                    }
                    Some("apiKey")
                        if scheme.get("in").and_then(Value::as_str) == Some("header") =>
                    {
                        scheme
                            .get("name")
                            .and_then(Value::as_str)
                            .map(|name| (name.to_owned(), String::new()))
                    }
                    _ => None,
                });
            if let Some(header) = header {
                selected.push(header);
            } else {
                supported = false;
            }
        }
        if !supported {
            continue;
        }
        for (name, prefix) in selected {
            let key = name.to_ascii_lowercase();
            if let Some((_, existing)) = auth.get(&key) {
                ensure!(
                    *existing == prefix,
                    "Operations require conflicting authentication prefixes for one header"
                );
            }
            // Reuse the credential validation to reject transport headers even
            // when a secondary authentication header is not the primary field.
            custom::secret_headers(
                &mut HeaderMap::new(),
                &BTreeMap::from([(name.clone(), prefix.clone())]),
            )?;
            auth.insert(key, (name, prefix));
        }
        return Ok(());
    }
    warnings.push("Unsupported or unresolved authentication scheme; configure authentication manually. Only HTTP bearer and header API keys are imported, and external references are not fetched.".into());
    Ok(())
}

#[cfg(test)]
#[path = "tests/openapi.rs"]
mod tests;
