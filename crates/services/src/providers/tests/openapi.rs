use super::*;
use serde_json::{Value, json};

fn import(value: Value, current: &str) -> Result<OpenApiImport> {
    import_openapi(&value.to_string(), current)
}

#[test]
fn imports_gateway_routes_and_compound_header_auth_without_copying_examples() {
    let result = import(json!({
        "openapi": "3.0.3", "servers": [{"url": "https://gateway.example/api/llm"}],
        "security": [{"Access": [], "Application": []}],
        "components": {"securitySchemes": {
            "Access": {"type": "http", "scheme": "bearer"},
            "Application": {"type": "apiKey", "in": "header", "name": "X-Application-Key", "example": "private-secret"}
        }},
        "paths": {
            "/chat/completions": {"post": {}}, "/responses": {"post": {}},
            "/v1/messages": {"post": {}}, "/v1beta/models/{modelAction}": {"post": {"parameters": [{"name":"modelAction", "in":"path", "description":"Use model:generateContent or model:streamGenerateContent."}]}},
            "/models": {"get": {}}, "/images/generations": {"post": {}}
        }
    }), "").unwrap();
    assert_eq!(result.endpoint, "https://gateway.example/api/llm");
    assert_eq!(result.custom.paths.chat_completions, "chat/completions");
    assert_eq!(result.custom.paths.responses, "responses");
    assert_eq!(result.custom.paths.messages, "v1/messages");
    assert_eq!(
        result.custom.paths.generate_content,
        "v1beta/models/{model}:generateContent"
    );
    assert_eq!(
        result.custom.paths.stream_generate_content,
        "v1beta/models/{model}:streamGenerateContent?alt=sse"
    );
    assert_eq!(result.custom.paths.models, "models");
    assert_eq!(result.custom.auth_header, "Authorization");
    assert_eq!(result.custom.auth_prefix, "Bearer ");
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("JSON") && w.contains("X-Application-Key"))
    );
    assert!(
        !serde_json::to_string(&result.custom)
            .unwrap()
            .contains("private-secret")
    );
    assert!(result.custom.request_defaults.is_empty());
    normalize_provider(&ProviderConfig {
        endpoint: result.endpoint,
        custom: Some(result.custom),
        family: result.family,
        ..Default::default()
    })
    .unwrap();
}

#[test]
fn disables_undocumented_routes_and_ignores_wrong_http_methods() {
    let result = import(
        json!({"openapi":"3.1.0", "paths": {
            "/v2/messages": {"post": {}}, "/models": {"post": {}}, "/responses": {"get": {}}
        }}),
        "https://gateway.example/root",
    )
    .unwrap();
    assert_eq!(result.family, ApiFamily::Anthropic);
    assert_eq!(result.custom.paths.messages, "v2/messages");
    assert!(result.custom.paths.chat_completions.is_empty());
    assert!(result.custom.paths.responses.is_empty());
    assert!(result.custom.paths.generate_content.is_empty());
    assert!(result.custom.paths.stream_generate_content.is_empty());
    assert!(result.custom.paths.models.is_empty());
}

#[test]
fn resolves_relative_servers_and_preserves_matching_server_selection() {
    let doc = json!({"openapi":"3.0.0", "servers":[{"url":"https://one.example/api"},{"url":"https://two.example/api"}], "paths":{"/chat/completions":{"post":{}}}});
    assert_eq!(
        import(doc, "https://two.example/api/").unwrap().endpoint,
        "https://two.example/api"
    );
    let doc = json!({"openapi":"3.0.0", "servers":[{"url":"/gateway/v1"}], "paths":{"/responses":{"post":{}}}});
    assert_eq!(
        import(doc, "https://two.example/old").unwrap().endpoint,
        "https://two.example/gateway/v1"
    );
}

#[test]
fn imports_swagger_base_path_and_local_security_reference() {
    let result = import(
        json!({"swagger":"2.0", "host":"gateway.example", "basePath":"/api/v2", "schemes":["https"],
            "securityDefinitions":{"Key":{"$ref":"#/definitions/HeaderAuth"}},
            "definitions":{"HeaderAuth":{"type":"apiKey","in":"header","name":"X-Service-Key"}},
            "security":[{"Key":[]}], "paths":{"/chat/completions":{"post":{}}}
        }),
        "",
    )
    .unwrap();
    assert_eq!(result.endpoint, "https://gateway.example/api/v2");
    assert_eq!(result.custom.auth_header, "X-Service-Key");
    assert_eq!(result.custom.auth_prefix, "");
}

#[test]
fn refuses_ambiguous_routes_incompatible_servers_and_unsafe_endpoints() {
    for doc in [
        json!({"openapi":"3.0.0","paths":{"/v1/responses":{"post":{}},"/v2/responses":{"post":{}}}}),
        json!({"openapi":"3.0.0","paths":{"/responses":{"post":{"servers":[{"url":"https://different.example"}]}}}}),
        json!({"openapi":"3.0.0","servers":[{"url":"https://gateway.example?token=secret"}],"paths":{"/responses":{"post":{}}}}),
        json!({"openapi":"3.0.0","paths":{"/projects/{project}/responses":{"post":{}}}}),
        json!({"openapi":"3.0.0","paths":{"/models":{"get":{}}}}),
        json!({"hello":"world"}),
    ] {
        assert!(import(doc, "https://gateway.example/api").is_err());
    }
    assert!(import_openapi("not JSON", "").is_err());
}

#[test]
fn warns_about_unsupported_security_without_reading_external_refs() {
    let result = import(json!({"openapi":"3.0.0", "security":[{"External":[]}],
        "components":{"securitySchemes":{"External":{"$ref":"https://unavailable.invalid/auth.json"}}},
        "paths":{"/responses":{"post":{}}}
    }), "https://gateway.example").unwrap();
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.to_lowercase().contains("authentication"))
    );
}

#[test]
fn imports_explicit_gemini_model_parameter_names() {
    let result = import(
        json!({"openapi":"3.1.0", "paths":{
            "/v1/models/{modelId}:generateContent":{"post":{}},
            "/v1/models/{modelId}:streamGenerateContent":{"post":{}},
            "/v1/models":{"get":{}}
        }}),
        "https://gateway.example",
    )
    .unwrap();
    assert_eq!(result.family, ApiFamily::Gemini);
    assert_eq!(
        result.custom.paths.generate_content,
        "v1/models/{model}:generateContent"
    );
    assert_eq!(
        result.custom.paths.stream_generate_content,
        "v1/models/{model}:streamGenerateContent?alt=sse"
    );
}

#[test]
fn imports_gemini_routes_when_models_is_the_first_path_segment() {
    for paths in [
        json!({"/models/{model}:generateContent":{"post":{}}, "/models/{model}:streamGenerateContent":{"post":{}}}),
        json!({"/models/{modelAction}":{"post":{"parameters":[{"name":"modelAction","in":"path","schema":{"enum":["model:generateContent","model:streamGenerateContent"]}}]}}}),
    ] {
        let result = import(
            json!({"openapi":"3.0.0","paths":paths}),
            "https://gateway.example/v1beta",
        )
        .unwrap();
        assert_eq!(
            result.custom.paths.generate_content,
            "models/{model}:generateContent"
        );
        assert_eq!(
            result.custom.paths.stream_generate_content,
            "models/{model}:streamGenerateContent?alt=sse"
        );
    }
}

#[test]
fn chooses_one_supported_auth_alternative_and_honors_operation_override() {
    let result = import(
        json!({"openapi":"3.0.0", "security":[{"Bad":[]}],
            "components":{"securitySchemes":{
                "Bad":{"type":"apiKey","in":"query","name":"key"},
                "One":{"type":"apiKey","in":"header","name":"X-One-Key"},
                "Two":{"type":"http","scheme":"bearer"}
            }},
            "paths":{"/responses":{"post":{"security":[{"One":[]},{"Two":[]}]}}}
        }),
        "https://gateway.example",
    )
    .unwrap();
    assert_eq!(result.custom.auth_header, "X-One-Key");
    assert!(
        !result
            .warnings
            .iter()
            .any(|w| w.contains("Multiple authentication headers"))
    );
}

#[test]
fn generic_model_action_enables_only_the_documented_action() {
    for (parameter, generate, stream) in [
        (
            json!({"schema":{"enum":["model:generateContent"]}, "description":"An action, such as generateContent or streamGenerateContent."}),
            true,
            false,
        ),
        (
            json!({"description":"Use model:streamGenerateContent."}),
            false,
            true,
        ),
    ] {
        let mut parameter = parameter;
        parameter["name"] = json!("modelAction");
        parameter["in"] = json!("path");
        let result = import(
            json!({"openapi":"3.0.0", "paths":{
                "/responses":{"post":{}},
                "/models/{modelAction}":{"post":{"parameters":[parameter]}}
            }}),
            "https://gateway.example/v1beta",
        )
        .unwrap();
        assert_eq!(!result.custom.paths.generate_content.is_empty(), generate);
        assert_eq!(
            !result.custom.paths.stream_generate_content.is_empty(),
            stream
        );
    }
}

#[test]
fn generic_model_action_without_documented_actions_stays_disabled() {
    for parameter in [
        json!({"name":"modelAction","in":"path","schema":{"type":"string"}}),
        json!({"name":"modelAction","in":"path","description":"Use othergenerateContent or streamGenerateContentElse."}),
        json!({"$ref":"https://unavailable.invalid/parameter.json"}),
    ] {
        let result = import(
            json!({"openapi":"3.0.0", "paths":{
                "/responses":{"post":{}},
                "/models/{modelAction}":{"post":{"parameters":[parameter]}}
            }}),
            "https://gateway.example/v1beta",
        )
        .unwrap();
        assert!(result.custom.paths.generate_content.is_empty());
        assert!(result.custom.paths.stream_generate_content.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("modelAction"))
        );
    }
    assert!(
        import(
            json!({"openapi":"3.0.0","paths":{"/models/{modelAction}":{"post":{}}}}),
            "https://gateway.example"
        )
        .is_err()
    );
}

#[test]
fn generic_model_action_resolves_local_parameter_and_schema_refs_with_overrides() {
    let result = import(json!({"openapi":"3.0.0", "components":{
        "parameters":{"Action":{"name":"modelAction","in":"path","schema":{"$ref":"#/components/schemas/Action"}}},
        "schemas":{"Action":{"enum":["model:generateContent","model:streamGenerateContent"]}}
    }, "paths":{
        "/models/{modelAction}":{"parameters":[{"$ref":"#/components/parameters/Action"}],"post":{}}
    }}), "https://gateway.example/v1beta").unwrap();
    assert_eq!(result.family, ApiFamily::Gemini);
    let result = import(json!({"openapi":"3.0.0", "paths":{
        "/responses":{"post":{}},
        "/models/{modelAction}":{
            "parameters":[{"name":"modelAction","in":"path","description":"generateContent and streamGenerateContent"}],
            "post":{"parameters":[{"name":"modelAction","in":"path","schema":{"enum":["model:generateContent"]}}]}
        }
    }}), "https://gateway.example/v1beta").unwrap();
    assert!(!result.custom.paths.generate_content.is_empty());
    assert!(result.custom.paths.stream_generate_content.is_empty());
}
