use super::super::*;
use serde_json::json;

#[test]
fn component_setup_reports_missing_executable_without_execution_claim() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join(".mcp.json"),
        json!({"mcpServers":{"fixture":{"command":"definitely-missing-xylo-runtime"}}}).to_string(),
    )
    .unwrap();
    let service = ExtensionService::new(directory.path().join("installed"));
    let package = service.install_local(&source).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    let status = service
        .component_setup(
            &package.id,
            "mcpServers",
            ComponentSetup {
                workspace: directory.path().to_owned(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(status.readiness, Readiness::NeedsSetup);
    assert!(status.entries.iter().all(|entry| entry.evidence.is_none()));
    assert!(service.list().unwrap()[0].components[0].evidence.is_none());
}

fn runtime_fixture() -> (
    tempfile::TempDir,
    ExtensionService,
    ExtensionPackage,
    std::path::PathBuf,
    crate::Store,
    crate::CredentialVault,
) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(source.join("hooks")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(source.join("hooks/hooks.json"), json!({"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{"type":"command","command":"node \"$CLAUDE_PLUGIN_ROOT/hook.cjs\""}]}]}}).to_string()).unwrap();
    std::fs::write(source.join("hook.cjs"), r#"const fs=require('node:fs'); let input='';process.stdin.on('data',d=>input+=d);process.stdin.on('end',()=>{fs.writeFileSync('hook-input.json',input);console.log(JSON.stringify({hookSpecificOutput:{permissionDecision:'allow'}}));});"#).unwrap();
    std::fs::write(source.join(".mcp.json"), json!({"mcpServers":{"fixture":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/mcp.cjs"],"env":{"TEST_API_KEY":"${UNREAD_AMBIENT_SECRET}"}}}}).to_string()).unwrap();
    std::fs::write(source.join("mcp.cjs"), r#"const readline=require('node:readline');const lines=readline.createInterface({input:process.stdin});lines.on('line',line=>{const m=JSON.parse(line);if(!('id'in m))return;let result={};if(m.method==='initialize')result={protocolVersion:m.params.protocolVersion,capabilities:{tools:{}},serverInfo:{name:'xylo-fixture',version:'1'}};else if(m.method==='tools/list')result={tools:[{name:'fixture_echo',description:'Fixture',inputSchema:{type:'object'},annotations:{readOnlyHint:true}}]};else if(m.method==='tools/call')result={content:[{type:'text',text:`echo ${m.params.arguments.text} ${process.env.TEST_API_KEY}`}],isError:false,structuredContent:{[process.env.TEST_API_KEY]:'keyed'}};console.log(JSON.stringify({jsonrpc:'2.0',id:m.id,result}));});"#).unwrap();
    std::fs::write(source.join(".lsp.json"), json!({"fixture":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/lsp.cjs"],"extensionToLanguage":{".txt":"plaintext"}}}).to_string()).unwrap();
    std::fs::write(source.join("lsp.cjs"), r#"let buffer=Buffer.alloc(0);process.stdin.on('data',data=>{buffer=Buffer.concat([buffer,data]);for(;;){const end=buffer.indexOf('\r\n\r\n');if(end<0)return;const len=Number(buffer.subarray(0,end).toString().match(/Content-Length: (\d+)/i)[1]);if(buffer.length<end+4+len)return;const m=JSON.parse(buffer.subarray(end+4,end+4+len));buffer=buffer.subarray(end+4+len);if(m.method==='exit'){process.exit(0);}if(!('id'in m))continue;let result=m.method==='initialize'?{capabilities:{hoverProvider:true},serverInfo:{name:'fixture-lsp',version:'1'}}:m.method==='shutdown'?null:{contents:'fixture hover'};const response=Buffer.from(JSON.stringify({jsonrpc:'2.0',id:m.id,result}));process.stdout.write(`Content-Length: ${response.length}\r\n\r\n`);process.stdout.write(response);}});"#).unwrap();
    let service = ExtensionService::new(directory.path().join("profile/extensions"));
    let package = service.install_local(&source).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    let store = crate::Store::open(directory.path().join("profile/store.db")).unwrap();
    let vault = crate::CredentialVault::for_tests();
    (directory, service, package, workspace, store, vault)
}

#[tokio::test]
async fn configured_hook_event_executes_in_sandbox_and_records_current_version_evidence() {
    let (_directory, service, package, workspace, store, vault) = runtime_fixture();
    service
        .component_setup(
            &package.id,
            "hooks",
            ComponentSetup {
                workspace: workspace.clone(),
                enabled_events: vec!["PreToolUse".into()],
                ..Default::default()
            },
        )
        .unwrap();
    let before = service
        .run_hooks(
            "PreToolUse",
            json!({"tool_name":"file_write"}),
            &workspace,
            store.clone(),
            vault.clone(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(before.is_empty());
    assert!(!workspace.join("hook-input.json").exists());
    let output = service
        .run_hooks(
            "PreToolUse",
            json!({"tool_name":"file_read","tool_input":{"path":"sample.txt"}}),
            &workspace,
            store,
            vault,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.len(), 1);
    let input: serde_json::Value =
        serde_json::from_slice(&std::fs::read(workspace.join("hook-input.json")).unwrap()).unwrap();
    assert_eq!(input["event"], "PreToolUse");
    assert_eq!(input["tool_name"], "file_read");
    let state = service.component_status(&package.id, "hooks").unwrap();
    assert_eq!(state.readiness, Readiness::ExecutionVerified);
    assert_eq!(state.digest, package.digest);
    service
        .component_setup(
            &package.id,
            "hooks",
            ComponentSetup {
                workspace,
                ..Default::default()
            },
        )
        .unwrap();
    assert_ne!(
        service.list().unwrap()[0]
            .components
            .iter()
            .find(|c| c.name == "hooks")
            .unwrap()
            .readiness,
        Readiness::ExecutionVerified
    );
}

#[tokio::test]
async fn configured_mcp_executes_protocol_and_call_with_explicit_redacted_credentials() {
    let (directory, service, package, workspace, store, vault) = runtime_fixture();
    vault.set("fixture-key", "very-secret-value").unwrap();
    service
        .component_setup(
            &package.id,
            "mcpServers",
            ComponentSetup {
                workspace,
                credential_refs: std::collections::BTreeMap::from([(
                    "TEST_API_KEY".into(),
                    "fixture-key".into(),
                )]),
                ..Default::default()
            },
        )
        .unwrap();
    let verified = service
        .execute_component(
            &package.id,
            "mcpServers",
            "verify",
            json!({}),
            store.clone(),
            vault.clone(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(verified["status"]["readiness"], "execution_verified");
    // Plugin MCP servers go through the same connector broker as configured
    // connectors, so a non-destructive `readOnlyHint` tool is classified a read.
    let tool = &verified["results"][0]["output"]["tools"][0];
    assert_eq!(tool["name"], "fixture_echo");
    assert_eq!(tool["writes"], false);
    let output = service
        .execute_component(
            &package.id,
            "mcpServers",
            "tools/call",
            json!({"entry":"fixture","tool":"fixture_echo","arguments":{"text":"hello"}}),
            store,
            vault,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(output.to_string().contains("hello"));
    assert!(!output.to_string().contains("very-secret-value"));
    assert!(
        !std::fs::read_to_string(directory.path().join("profile/extensions/runtime.json"))
            .unwrap()
            .contains("very-secret-value")
    );
}

#[tokio::test]
async fn configured_lsp_initializes_requests_and_shuts_down() {
    let (_directory, service, package, workspace, store, vault) = runtime_fixture();
    service
        .component_setup(
            &package.id,
            "lspServers",
            ComponentSetup {
                workspace,
                ..Default::default()
            },
        )
        .unwrap();
    let output = service
        .execute_component(
            &package.id,
            "lspServers",
            "verify",
            json!({}),
            store.clone(),
            vault.clone(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output["status"]["readiness"], "execution_verified");
    let output = service
        .execute_component(
            &package.id,
            "lspServers",
            "request",
            json!({"entry":"fixture","method":"workspace/symbol","params":{"query":"fixture"}}),
            store,
            vault,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(output.to_string().contains("fixture hover"));
}

#[tokio::test]
async fn component_environment_cannot_change_launcher_home_path_or_node_startup() {
    use std::collections::BTreeMap;
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let home = std::env::var_os("HOME").unwrap();
    let secret_directory = tempfile::tempdir_in(home).unwrap();
    let secret = secret_directory.path().join("xylo-generated-test-secret");
    std::fs::write(&secret, "xylo-deny-home-sentinel").unwrap();
    let mut prepared = crate::sandbox::prepare(
        "/bin/cat",
        &[secret.to_string_lossy().into_owned()],
        &workspace,
        &[],
        &[],
    )
    .unwrap();
    crate::sandbox::set_child_environment(
        &mut prepared,
        &BTreeMap::from([("HOME".into(), workspace.to_string_lossy().into_owned())]),
    )
    .unwrap();
    let output = prepared.command.output().await.unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("xylo-deny-home-sentinel"));

    let marker = directory.path().join("launcher-preload-marker");
    let preload = workspace.join("preload.cjs");
    std::fs::write(
        &preload,
        format!(
            "require('node:fs').writeFileSync({},'escaped')",
            serde_json::to_string(&marker.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();
    for program in ["sandbox-exec", "bwrap"] {
        let spoof = workspace.join(program);
        std::fs::write(
            &spoof,
            format!("#!/bin/sh\nprintf escaped > '{}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(spoof, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let mut prepared =
        crate::sandbox::prepare("/bin/echo", &["safe".into()], &workspace, &[], &[]).unwrap();
    let environment = BTreeMap::from([
        ("HOME".into(), workspace.to_string_lossy().into_owned()),
        ("PATH".into(), workspace.to_string_lossy().into_owned()),
        (
            "NODE_OPTIONS".into(),
            format!("--require {}", preload.display()),
        ),
        (
            "LD_PRELOAD".into(),
            workspace.join("absent.so").to_string_lossy().into_owned(),
        ),
        (
            "DYLD_INSERT_LIBRARIES".into(),
            workspace
                .join("absent.dylib")
                .to_string_lossy()
                .into_owned(),
        ),
    ]);
    crate::sandbox::set_child_environment(&mut prepared, &environment).unwrap();
    let env = prepared.command.as_std().get_envs().collect::<Vec<_>>();
    assert!(
        !env.iter()
            .any(|(name, _)| name.to_str() == Some("NODE_OPTIONS"))
    );
    let output = prepared.command.output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "Component env executed before the OS sandbox"
    );
}

#[tokio::test]
async fn cancelled_hook_terminates_owned_process_without_recording_execution() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(source.join("hooks")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(source.join("hooks/hooks.json"),json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"printf started > hook-started; sleep 30; printf late > hook-finished"}]}]}}).to_string()).unwrap();
    let service = ExtensionService::new(directory.path().join("profile/extensions"));
    let package = service.install_local(&source).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    service
        .component_setup(
            &package.id,
            "hooks",
            ComponentSetup {
                workspace: workspace.clone(),
                enabled_events: vec!["PreToolUse".into()],
                ..Default::default()
            },
        )
        .unwrap();
    let store = crate::Store::open(directory.path().join("profile/store.db")).unwrap();
    let vault = crate::CredentialVault::for_tests();
    let cancel = tokio_util::sync::CancellationToken::new();
    let worker_service = service.clone();
    let worker_workspace = workspace.clone();
    let worker_cancel = cancel.clone();
    let running = tokio::spawn(async move {
        worker_service
            .run_hooks(
                "PreToolUse",
                json!({"tool_name":"Read"}),
                &worker_workspace,
                store,
                vault,
                worker_cancel,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !workspace.join("hook-started").is_file() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(!workspace.join("hook-finished").exists());
    let state = service.component_status(&package.id, "hooks").unwrap();
    assert_ne!(state.readiness, Readiness::ExecutionVerified);
    assert!(state.entries.iter().all(|entry| entry.evidence.is_none()));
}

#[tokio::test]
async fn old_execution_cannot_verify_reconfigured_or_disabled_component() {
    for disable in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(source.join("hooks")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(source.join("hooks/hooks.json"),json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"printf started > started; while [ ! -f release ]; do sleep 0.01; done; printf '{}'"}]}]}}).to_string()).unwrap();
        let service = ExtensionService::new(directory.path().join("profile/extensions"));
        let package = service.install_local(&source).unwrap();
        service.set_enabled(&package.id, true).unwrap();
        let setup = ComponentSetup {
            workspace: workspace.clone(),
            ..Default::default()
        };
        service
            .component_setup(&package.id, "hooks", setup.clone())
            .unwrap();
        let store = crate::Store::open(directory.path().join("profile/store.db")).unwrap();
        let vault = crate::CredentialVault::for_tests();
        let worker = service.clone();
        let id = package.id.clone();
        let running = tokio::spawn(async move {
            worker
                .execute_component(
                    &id,
                    "hooks",
                    "verify",
                    json!({}),
                    store,
                    vault,
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !workspace.join("started").is_file() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if disable {
            service.set_enabled(&package.id, false).unwrap();
        } else {
            let mut setup = setup;
            setup.environment.insert("MODE".into(), "changed".into());
            service
                .component_setup(&package.id, "hooks", setup)
                .unwrap();
        }
        std::fs::write(workspace.join("release"), "").unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), running)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(
            service.list().unwrap()[0]
                .components
                .iter()
                .all(|component| component.evidence.is_none())
        );
    }
}

#[tokio::test]
async fn configured_deadline_bounds_slow_mcp_and_unterminated_lsp_headers() {
    for lsp in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        let script = if lsp {
            "require('node:fs').writeFileSync('started','yes');process.stdout.write('x'.repeat(1024*1024));setInterval(()=>{},1000);"
        } else {
            "require('node:fs').writeFileSync('started','yes');process.stdin.resume();setInterval(()=>{},1000);"
        };
        std::fs::write(source.join("server.cjs"), script).unwrap();
        let (file, component) = if lsp {
            (".lsp.json", "lspServers")
        } else {
            (".mcp.json", "mcpServers")
        };
        let servers =
            json!({"fixture":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/server.cjs"]}});
        let config = if lsp {
            servers
        } else {
            json!({"mcpServers":servers})
        };
        std::fs::write(source.join(file), config.to_string()).unwrap();
        let service = ExtensionService::new(directory.path().join("profile/extensions"));
        let package = service.install_local(&source).unwrap();
        service.set_enabled(&package.id, true).unwrap();
        service
            .component_setup(
                &package.id,
                component,
                ComponentSetup {
                    workspace: workspace.clone(),
                    timeout_seconds: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        let store = crate::Store::open(directory.path().join("profile/store.db")).unwrap();
        let vault = crate::CredentialVault::for_tests();
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            service.execute_component(
                &package.id,
                component,
                "verify",
                json!({}),
                store,
                vault,
                tokio_util::sync::CancellationToken::new(),
            ),
        )
        .await
        .unwrap();
        let error = result.unwrap_err().to_string();
        assert!(
            workspace.join("started").is_file(),
            "The actual server process must start"
        );
        if lsp {
            assert!(error.contains("LSP headers exceed 8 KiB"), "{error}");
        } else {
            assert!(error.contains("MCP component deadline exceeded"), "{error}");
            assert!(start.elapsed() >= std::time::Duration::from_secs(1));
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        assert!(
            service
                .component_status(&package.id, component)
                .unwrap()
                .entries
                .iter()
                .all(|entry| entry.evidence.is_none())
        );
    }
}

#[tokio::test]
async fn pre_cancelled_component_never_starts_a_hook_process() {
    let (_directory, service, package, workspace, store, vault) = runtime_fixture();
    service
        .component_setup(
            &package.id,
            "hooks",
            ComponentSetup {
                workspace: workspace.clone(),
                ..Default::default()
            },
        )
        .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let result = service
        .execute_component(
            &package.id,
            "hooks",
            "verify",
            json!({}),
            store,
            vault,
            cancel,
        )
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("cancelled before dispatch")
    );
    assert!(!workspace.join("hook-input.json").exists());
    assert!(
        service
            .component_status(&package.id, "hooks")
            .unwrap()
            .entries
            .iter()
            .all(|entry| entry.evidence.is_none())
    );
}

#[tokio::test]
async fn approved_component_binding_cannot_run_new_setup() {
    let (_directory, service, package, workspace, store, vault) = runtime_fixture();
    let mut setup = ComponentSetup {
        workspace: workspace.clone(),
        ..Default::default()
    };
    service
        .component_setup(&package.id, "hooks", setup.clone())
        .unwrap();
    let approved = service.component_status(&package.id, "hooks").unwrap();
    let mut wrong_identity = approved.clone();
    wrong_identity.component = "other-component".into();
    assert!(
        service
            .execute_component_bound(
                &package.id,
                "hooks",
                "verify",
                json!({}),
                store.clone(),
                vault.clone(),
                tokio_util::sync::CancellationToken::new(),
                &wrong_identity
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("changed since approval")
    );
    assert!(!workspace.join("hook-input.json").exists());
    setup.environment.insert("MODE".into(), "changed".into());
    service
        .component_setup(&package.id, "hooks", setup)
        .unwrap();
    let error = service
        .execute_component_bound(
            &package.id,
            "hooks",
            "verify",
            json!({}),
            store,
            vault,
            tokio_util::sync::CancellationToken::new(),
            &approved,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("changed since approval"));
    assert!(!workspace.join("hook-input.json").exists());
}

#[tokio::test]
async fn oversized_hook_input_is_rejected_before_process_start() {
    let (_directory, service, package, workspace, store, vault) = runtime_fixture();
    service
        .component_setup(
            &package.id,
            "hooks",
            ComponentSetup {
                workspace: workspace.clone(),
                ..Default::default()
            },
        )
        .unwrap();
    let input = json!({"large":"x".repeat(1024*1024)});
    let error = service
        .execute_component(
            &package.id,
            "hooks",
            "verify",
            input,
            store,
            vault,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("input exceeds 1 MiB"));
    assert!(!workspace.join("hook-input.json").exists());
}

#[test]
fn uninstall_revokes_runtime_grants_before_same_version_reinstall() {
    let (directory, service, package, workspace, _store, _vault) = runtime_fixture();
    service
        .component_setup(
            &package.id,
            "hooks",
            ComponentSetup {
                workspace: workspace.clone(),
                enabled_events: vec!["PreToolUse".into()],
                allowed_domains: vec!["api.example.com".into()],
                credential_refs: std::collections::BTreeMap::from([(
                    "TOKEN".into(),
                    "old-reference".into(),
                )]),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        service
            .matching_hooks("PreToolUse", &json!({"tool_name":"file_read"}), &workspace)
            .unwrap()
            .len(),
        1
    );
    service.remove(&package.id).unwrap();
    assert!(
        !std::fs::read_to_string(directory.path().join("profile/extensions/runtime.json"))
            .unwrap()
            .contains("old-reference")
    );
    let reinstalled = service
        .install_local(&directory.path().join("source"))
        .unwrap();
    assert_eq!(package.id, reinstalled.id);
    assert_eq!(package.digest, reinstalled.digest);
    service.set_enabled(&reinstalled.id, true).unwrap();
    assert!(
        service
            .component_status(&reinstalled.id, "hooks")
            .unwrap_err()
            .to_string()
            .contains("needs explicit execution setup")
    );
    assert!(
        service
            .matching_hooks("PreToolUse", &json!({"tool_name":"file_read"}), &workspace)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn undelivered_hook_events_are_reported_per_entry_without_disabling_the_component() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(source.join("hooks")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        source.join("hooks/hooks.json"),
        json!({"hooks":{
            "SessionStart":[{"hooks":[{"type":"command","command":"printf started"}]}],
            "PreToolUse":[{"matcher":"Read","hooks":[{"type":"command","command":"printf '{}'"}]}]
        }})
        .to_string(),
    )
    .unwrap();
    let service = ExtensionService::new(directory.path().join("profile/extensions"));
    let package = service.install_local(&source).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    let status = service
        .component_setup(
            &package.id,
            "hooks",
            ComponentSetup {
                workspace: workspace.clone(),
                enabled_events: vec!["PreToolUse".into()],
                ..Default::default()
            },
        )
        .unwrap();
    let session = status
        .entries
        .iter()
        .find(|entry| entry.name.starts_with("SessionStart"))
        .unwrap();
    assert_eq!(session.readiness, Readiness::UnsupportedRequirement);
    assert!(session.requirements[0].contains("SessionStart"));
    assert_ne!(status.readiness, Readiness::UnsupportedRequirement);
    assert!(
        service
            .component_setup(
                &package.id,
                "hooks",
                ComponentSetup {
                    workspace,
                    enabled_events: vec!["SessionStart".into()],
                    ..Default::default()
                },
            )
            .is_err(),
        "An event Xylo never delivers cannot be enabled"
    );
}
