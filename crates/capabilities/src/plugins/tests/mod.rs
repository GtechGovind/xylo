use super::*;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;
use xylo_core::{
    NativePluginManifest, PluginBudgets, PluginCatalog, PluginInspection, PluginManagementResult,
};

const ECHO: &str = include_str!("../fixtures/echo.wat");
const READ: &str = r#"(component
  (import "read-text" (func $read (param "path" string) (result (result string (error string)))))
  (core module $memory
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 1024))
  (core instance $mem (instantiate $memory))
  (core func $read-core (canon lower (func $read)
    (memory (core memory $mem "memory")) (realloc (core func $mem "realloc"))))
  (core module $wrapper
    (import "host" "read" (func $read (param i32 i32 i32)))
    (func (export "run") (param i32 i32) (result i32)
      local.get 0 local.get 1 i32.const 16 call $read i32.const 16))
  (core instance $wrapper-instance (instantiate $wrapper
    (with "host" (instance (export "read" (func $read-core))))))
  (func (export "run") (param "input" string) (result (result string (error string)))
    (canon lift (core func $wrapper-instance "run")
      (memory (core memory $mem "memory")) (realloc (core func $mem "realloc")))))"#;
const WRITE: &str = r#"(component
  (import "write-text" (func $write (param "name" string) (param "text" string) (result (result string (error string)))))
  (core module $memory
    (memory (export "memory") 1)
    (data (i32.const 512) "plugin-result")
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 2048))
  (core instance $mem (instantiate $memory))
  (core func $write-core (canon lower (func $write)
    (memory (core memory $mem "memory")) (realloc (core func $mem "realloc"))))
  (core module $wrapper
    (import "host" "write" (func $write (param i32 i32 i32 i32 i32)))
    (func (export "run") (param i32 i32) (result i32)
      i32.const 512 i32.const 13 local.get 0 local.get 1 i32.const 16 call $write i32.const 16))
  (core instance $wrapper-instance (instantiate $wrapper
    (with "host" (instance (export "write" (func $write-core))))))
  (func (export "run") (param "input" string) (result (result string (error string)))
    (canon lift (core func $wrapper-instance "run")
      (memory (core memory $mem "memory")) (realloc (core func $mem "realloc")))))"#;

struct Fixture {
    _directory: tempfile::TempDir,
    config: CapabilityConfig,
    package: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let workspace = root.join("project");
        let artifacts = root.join("data/artifacts");
        let management = root.join("data/native-management");
        let package = root.join("selected-package");
        for path in [&workspace, &artifacts, &management, &package] {
            std::fs::create_dir_all(path).unwrap();
        }
        Self {
            _directory: directory,
            config: CapabilityConfig {
                workspace,
                artifacts,
                management_root: Some(management),
                ..Default::default()
            },
            package,
        }
    }
    fn package(&self, wat: &str, version: &str, capabilities: &[&str]) -> PathBuf {
        self.package_with_budgets(wat, version, capabilities, PluginBudgets::default())
    }
    fn package_with_budgets(
        &self,
        wat: &str,
        version: &str,
        capabilities: &[&str],
        budgets: PluginBudgets,
    ) -> PathBuf {
        std::fs::write(self.package.join("component.wat"), wat).unwrap();
        let manifest = NativePluginManifest {
            format: 1,
            id: "fixture.echo".into(),
            name: "Fixture component".into(),
            version: version.into(),
            entrypoint: "component.wat".into(),
            sha256: hex::encode(Sha256::digest(wat.as_bytes())),
            capabilities: capabilities
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            budgets,
        };
        let path = self.package.join("plugin.json");
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        path
    }
    async fn list(&self) -> PluginCatalog {
        serde_json::from_value(
            control(&self.config, "helper_plugin_list", json!({}))
                .await
                .unwrap(),
        )
        .unwrap()
    }
    async fn inspect(&self, path: &std::path::Path) -> PluginInspection {
        serde_json::from_value(
            control(
                &self.config,
                "helper_plugin_inspect",
                json!({"manifest_path":path}),
            )
            .await
            .unwrap(),
        )
        .unwrap()
    }
    async fn install(
        &self,
        path: &std::path::Path,
        grants: &[&str],
        replace: bool,
    ) -> PluginManagementResult {
        let inspection = self.inspect(path).await;
        serde_json::from_value(control(&self.config, "helper_plugin_install", json!({"operation_id":Uuid::new_v4(),"expected_revision":self.list().await.revision,"manifest_path":path,"expected_package_sha256":inspection.package_sha256,"grants":grants,"enabled":true,"replace":replace})).await.unwrap()).unwrap()
    }
    async fn run(&self, input: &str) -> Result<ToolObservation> {
        let pin = self.list().await.plugins[0].package_sha256.clone();
        execute_tool(
            &self.config,
            "plugin_run_installed",
            &json!({"id":"fixture.echo","package_sha256":pin,"input":input}),
            CancellationToken::new(),
        )
        .await
    }
}
async fn control(config: &CapabilityConfig, name: &str, args: Value) -> Result<Value> {
    tokio::time::timeout(
        Duration::from_secs(20),
        execute_management(config, name, &args, CancellationToken::new()),
    )
    .await?
}

type WriteGate = (PathBuf, tokio::sync::oneshot::Sender<()>);
static WRITE_GATE: std::sync::Mutex<Option<WriteGate>> = std::sync::Mutex::new(None);

#[cfg(unix)]
type ReadHook = (PathBuf, Box<dyn FnOnce() -> Result<()> + Send>);
#[cfg(unix)]
static READ_HOOKS: std::sync::Mutex<Vec<ReadHook>> = std::sync::Mutex::new(Vec::new());

#[cfg(unix)]
pub(super) fn before_workspace_read_open(workspace: &std::path::Path) -> Result<()> {
    let hook = {
        let mut hooks = READ_HOOKS.lock().unwrap();
        hooks
            .iter()
            .position(|(expected, _)| expected == workspace)
            .map(|index| hooks.remove(index).1)
    };
    if let Some(hook) = hook {
        hook()?;
    }
    Ok(())
}

pub(super) fn before_write_commit(
    path: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let gate = {
        let mut gate = WRITE_GATE.lock().unwrap();
        if gate.as_ref().is_some_and(|(expected, _)| expected == path) {
            gate.take()
        } else {
            None
        }
    };
    if let Some((_, entered)) = gate {
        let _ = entered.send(());
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        security::checked(cancel)?;
        anyhow::bail!("Test write gate was not cancelled");
    }
    Ok(())
}

#[tokio::test]
async fn installed_echo_survives_restart_and_original_package_removal() {
    let fixture = Fixture::new();
    let path = fixture.package(ECHO, "1.0", &[]);
    let installed = fixture.install(&path, &[], false).await;
    assert!(installed.changed);
    std::fs::remove_dir_all(&fixture.package).unwrap();
    let response = fixture.run("native installed echo").await.unwrap();
    let output: Value = serde_json::from_str(&response.summary).unwrap();
    assert_eq!(output["output"], "native installed echo");
    assert_eq!(
        output["plugin"]["package_sha256"],
        installed.catalog.plugins[0].package_sha256
    );
}

#[tokio::test]
async fn review_pin_hash_and_catalog_revision_are_enforced() {
    let fixture = Fixture::new();
    let path = fixture.package(ECHO, "1.0", &[]);
    let review = fixture.inspect(&path).await;
    fixture.package(ECHO, "2.0", &[]);
    let args = json!({"operation_id":Uuid::new_v4(),"expected_revision":fixture.list().await.revision,"manifest_path":path,"expected_package_sha256":review.package_sha256,"grants":[],"enabled":true,"replace":false});
    assert!(
        control(&fixture.config, "helper_plugin_install", args)
            .await
            .unwrap_err()
            .to_string()
            .contains("changed since inspection")
    );
    let installed = fixture.install(&path, &[], false).await;
    let bad_pin = "0".repeat(64);
    assert!(
        execute_tool(
            &fixture.config,
            "plugin_run_installed",
            &json!({"id":"fixture.echo","package_sha256":bad_pin}),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    let stale = json!({"operation_id":Uuid::new_v4(),"expected_revision":"stale","id":"fixture.echo","enabled":false});
    assert!(
        control(&fixture.config, "helper_plugin_set_enabled", stale)
            .await
            .unwrap_err()
            .to_string()
            .contains("catalog changed")
    );
    let override_args = json!({"id":"fixture.echo","package_sha256":installed.catalog.plugins[0].package_sha256,"capabilities":["read_workspace"]});
    assert!(
        execute_tool(
            &fixture.config,
            "plugin_run_installed",
            &override_args,
            CancellationToken::new()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn explicit_update_permissions_enablement_and_removal() {
    let fixture = Fixture::new();
    let path = fixture.package(READ, "1.0", &["read_workspace"]);
    let installed = fixture.install(&path, &["read_workspace"], false).await;
    let old_package = catalog::root(&fixture.config)
        .unwrap()
        .join("packages")
        .join(&installed.catalog.plugins[0].package_sha256);
    std::fs::write(
        fixture.config.workspace.join("fact.txt"),
        "Workspace evidence",
    )
    .unwrap();
    #[cfg(any(unix, windows))]
    assert!(
        fixture
            .run("fact.txt")
            .await
            .unwrap()
            .summary
            .contains("Workspace evidence")
    );
    #[cfg(not(any(unix, windows)))]
    assert!(
        fixture
            .run("fact.txt")
            .await
            .unwrap_err()
            .to_string()
            .contains("unavailable on this platform")
    );
    let result = control(&fixture.config, "helper_plugin_set_grants", json!({"operation_id":Uuid::new_v4(),"expected_revision":fixture.list().await.revision,"id":"fixture.echo","grants":[]})).await.unwrap();
    assert!(result["changed"].as_bool().unwrap());
    assert!(
        fixture
            .run("fact.txt")
            .await
            .unwrap_err()
            .to_string()
            .contains("not granted")
    );
    let path = fixture.package(READ, "2.0", &["read_workspace"]);
    let updated = fixture.install(&path, &["read_workspace"], true).await;
    assert!(
        !old_package.exists(),
        "An update reclaims the old unreferenced package"
    );
    let new_package = catalog::root(&fixture.config)
        .unwrap()
        .join("packages")
        .join(&updated.catalog.plugins[0].package_sha256);
    assert_eq!(fixture.list().await.plugins[0].manifest.version, "2.0");
    control(&fixture.config, "helper_plugin_set_enabled", json!({"operation_id":Uuid::new_v4(),"expected_revision":fixture.list().await.revision,"id":"fixture.echo","enabled":false})).await.unwrap();
    assert!(
        fixture
            .run("fact.txt")
            .await
            .unwrap_err()
            .to_string()
            .contains("disabled")
    );
    control(&fixture.config, "helper_plugin_remove", json!({"operation_id":Uuid::new_v4(),"expected_revision":fixture.list().await.revision,"id":"fixture.echo"})).await.unwrap();
    assert!(fixture.list().await.plugins.is_empty());
    assert!(
        !new_package.exists(),
        "Remove reclaims the unreferenced package bytes"
    );
}

#[tokio::test]
async fn failed_update_preserves_installed_authority_and_reclaims_crash_orphans() {
    let fixture = Fixture::new();
    let path = fixture.package(ECHO, "1.0", &[]);
    let installed = fixture.install(&path, &[], false).await;
    let root = catalog::root(&fixture.config).unwrap();
    let orphan = root.join("packages").join("f".repeat(64));
    std::fs::create_dir(&orphan).unwrap();
    std::fs::write(orphan.join("component.wasm"), b"orphaned package").unwrap();
    let path = fixture.package(ECHO, "1.0", &["read_workspace"]);
    let review = fixture.inspect(&path).await;
    assert!(control(&fixture.config, "helper_plugin_install", json!({"operation_id":Uuid::new_v4(),"expected_revision":installed.catalog.revision,"manifest_path":path,"expected_package_sha256":review.package_sha256,"grants":[],"enabled":true,"replace":true})).await.unwrap_err().to_string().contains("different version"));
    assert!(!orphan.exists());
    assert_eq!(
        fixture.list().await.plugins[0],
        installed.catalog.plugins[0]
    );
    assert!(
        fixture
            .run("old package still works")
            .await
            .unwrap()
            .summary
            .contains("old package still works")
    );
}

#[tokio::test]
async fn active_run_lease_blocks_revoke_update_and_remove() {
    let fixture = Fixture::new();
    let path = fixture.package(ECHO, "1.0", &[]);
    let installed = fixture.install(&path, &[], false).await;
    let loaded = catalog::load(
        &fixture.config,
        "fixture.echo",
        &installed.catalog.plugins[0].package_sha256,
        &CancellationToken::new(),
    )
    .unwrap();
    let args = json!({"operation_id":Uuid::new_v4(),"expected_revision":fixture.list().await.revision,"id":"fixture.echo","grants":[]});
    let error = control(&fixture.config, "helper_plugin_set_grants", args.clone())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("busy"),
        "An actual held run lease must report contention: {error:#}"
    );
    drop(loaded);
    control(&fixture.config, "helper_plugin_set_grants", args)
        .await
        .unwrap();
}

#[tokio::test]
async fn management_is_idempotent_and_rejects_reused_operation_id() {
    let fixture = Fixture::new();
    let path = fixture.package(ECHO, "1.0", &[]);
    fixture.install(&path, &[], false).await;
    let operation_id = Uuid::new_v4();
    let mut args = json!({"operation_id":operation_id,"expected_revision":fixture.list().await.revision,"id":"fixture.echo","enabled":false});
    let first = control(&fixture.config, "helper_plugin_set_enabled", args.clone())
        .await
        .unwrap();
    assert_eq!(
        first,
        control(&fixture.config, "helper_plugin_set_enabled", args.clone())
            .await
            .unwrap()
    );
    args["enabled"] = json!(true);
    assert!(
        control(&fixture.config, "helper_plugin_set_enabled", args)
            .await
            .unwrap_err()
            .to_string()
            .contains("reused")
    );
}

#[tokio::test]
async fn installation_replay_does_not_require_original_selected_files() {
    let fixture = Fixture::new();
    let path = fixture.package(ECHO, "1.0", &[]);
    let review = fixture.inspect(&path).await;
    let args = json!({"operation_id":Uuid::new_v4(),"expected_revision":fixture.list().await.revision,"manifest_path":path,"expected_package_sha256":review.package_sha256,"grants":[],"enabled":true,"replace":false});
    let first = control(&fixture.config, "helper_plugin_install", args.clone())
        .await
        .unwrap();
    std::fs::remove_dir_all(&fixture.package).unwrap();
    assert_eq!(
        first,
        control(&fixture.config, "helper_plugin_install", args)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn installed_reads_reject_private_data_and_workspace_escape() {
    let fixture = Fixture::new();
    let path = fixture.package(READ, "1.0", &["read_workspace"]);
    fixture.install(&path, &["read_workspace"], false).await;
    let secret = fixture
        .config
        .management_root
        .as_ref()
        .unwrap()
        .parent()
        .unwrap()
        .join("credentials.json");
    std::fs::write(&secret, "private profile configuration").unwrap();
    assert!(fixture.run(secret.to_str().unwrap()).await.is_err());
    let mut broad = fixture.config.clone();
    broad.workspace = fixture._directory.path().canonicalize().unwrap();
    let pin = fixture.list().await.plugins[0].package_sha256.clone();
    let response = execute_tool(
        &broad,
        "plugin_run_installed",
        &json!({"id":"fixture.echo","package_sha256":pin,"input":secret.strip_prefix(&broad.workspace).unwrap()}),
        CancellationToken::new(),
    )
    .await;
    let error = response.unwrap_err().to_string();
    #[cfg(any(unix, windows))]
    assert!(error.contains("Application data files"));
    #[cfg(not(any(unix, windows)))]
    assert!(error.contains("unavailable on this platform"));
}

#[cfg(unix)]
#[tokio::test]
async fn file_swap_after_directory_validation_cannot_read_app_private_text() {
    let fixture = Fixture::new();
    let path = fixture.package(READ, "1.0", &["read_workspace"]);
    fixture.install(&path, &["read_workspace"], false).await;
    let fact = fixture.config.workspace.join("fact.txt");
    std::fs::write(&fact, "authorized workspace evidence").unwrap();
    let secret = fixture
        .config
        .management_root
        .as_ref()
        .unwrap()
        .parent()
        .unwrap()
        .join("private.txt");
    std::fs::write(&secret, "private profile secret").unwrap();
    READ_HOOKS.lock().unwrap().push((
        fixture.config.workspace.clone(),
        Box::new(move || {
            std::fs::remove_file(&fact)?;
            std::os::unix::fs::symlink(&secret, &fact)?;
            Ok(())
        }),
    ));
    let result = fixture.run("fact.txt").await;
    assert!(
        result.is_err(),
        "A substituted symlink must not be followed"
    );
    assert!(
        !result
            .unwrap_err()
            .to_string()
            .contains("private profile secret")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ancestor_swap_after_validation_reads_only_the_original_directory_handle() {
    let fixture = Fixture::new();
    let path = fixture.package(READ, "1.0", &["read_workspace"]);
    fixture.install(&path, &["read_workspace"], false).await;
    let parent = fixture.config.workspace.join("subdir");
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(parent.join("fact.txt"), "authorized original directory").unwrap();
    let outside = fixture
        .config
        .management_root
        .as_ref()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::write(outside.join("fact.txt"), "private profile secret").unwrap();
    let moved = fixture.config.workspace.join("moved-original");
    READ_HOOKS.lock().unwrap().push((
        fixture.config.workspace.clone(),
        Box::new(move || {
            std::fs::rename(&parent, &moved)?;
            std::os::unix::fs::symlink(&outside, &parent)?;
            Ok(())
        }),
    ));
    let result = fixture.run("subdir/fact.txt").await.unwrap();
    let output: Value = serde_json::from_str(&result.summary).unwrap();
    assert_eq!(output["output"], "authorized original directory");
    assert!(!result.summary.contains("private profile secret"));
}

#[cfg(unix)]
#[tokio::test]
async fn hard_link_aliases_and_named_pipes_cannot_be_read_as_workspace_text() {
    let fixture = Fixture::new();
    let path = fixture.package(READ, "1.0", &["read_workspace"]);
    fixture.install(&path, &["read_workspace"], false).await;
    let private = fixture
        .config
        .management_root
        .as_ref()
        .unwrap()
        .parent()
        .unwrap()
        .join("private.txt");
    std::fs::write(&private, "private profile secret").unwrap();
    std::fs::hard_link(private, fixture.config.workspace.join("alias.txt")).unwrap();
    assert!(
        fixture
            .run("alias.txt")
            .await
            .unwrap_err()
            .to_string()
            .contains("without links")
    );
    use std::os::unix::ffi::OsStrExt;
    let pipe = std::ffi::CString::new(fixture.config.workspace.join("pipe").as_os_str().as_bytes())
        .unwrap();
    // SAFETY: pipe is a live NUL-terminated path within this fixture directory.
    assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), fixture.run("pipe"))
            .await
            .unwrap()
            .is_err()
    );
}

#[cfg(not(any(unix, windows)))]
#[tokio::test]
async fn workspace_read_import_fails_closed_until_native_handle_access_is_qualified() {
    let fixture = Fixture::new();
    let path = fixture.package(READ, "1.0", &["read_workspace"]);
    fixture.install(&path, &["read_workspace"], false).await;
    std::fs::write(
        fixture.config.workspace.join("fact.txt"),
        "valid workspace evidence",
    )
    .unwrap();
    assert!(
        fixture
            .run("fact.txt")
            .await
            .unwrap_err()
            .to_string()
            .contains("unavailable on this platform")
    );
}

#[tokio::test]
async fn artifact_write_is_no_clobber_and_returns_committed_artifact() {
    let fixture = Fixture::new();
    let path = fixture.package(WRITE, "1.0", &["write_artifacts"]);
    fixture.install(&path, &["write_artifacts"], false).await;
    let output = fixture.run("preserved native artifact").await.unwrap();
    assert_eq!(output.artifacts.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&output.artifacts[0]).unwrap(),
        "preserved native artifact"
    );
    assert!(fixture.run("replacement").await.is_err());
    assert_eq!(
        std::fs::read_to_string(&output.artifacts[0]).unwrap(),
        "preserved native artifact"
    );
    assert_eq!(
        std::fs::read_dir(&fixture.config.artifacts)
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn cancelled_admitted_artifact_write_never_publishes_partial_output() {
    let fixture = Fixture::new();
    let path = fixture.package(WRITE, "1.0", &["write_artifacts"]);
    let installed = fixture.install(&path, &["write_artifacts"], false).await;
    let (entered, waiting) = tokio::sync::oneshot::channel();
    *WRITE_GATE.lock().unwrap() = Some((fixture.config.artifacts.clone(), entered));
    let config = fixture.config.clone();
    let pin = installed.catalog.plugins[0].package_sha256.clone();
    let cancel = CancellationToken::new();
    let caller_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        execute_tool(
            &config,
            "plugin_run_installed",
            &json!({"id":"fixture.echo","package_sha256":pin,"input":"unpublished bytes"}),
            caller_cancel,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_dir(&fixture.config.artifacts)
            .unwrap()
            .count(),
        0
    );
    control(&fixture.config, "helper_plugin_set_grants", json!({"operation_id":Uuid::new_v4(),"expected_revision":fixture.list().await.revision,"id":"fixture.echo","grants":[]})).await.unwrap();
}

fn repeated_writer(payload: &str) -> String {
    WRITE.replace("(data (i32.const 512) \"plugin-result\")", &format!("(data (i32.const 512) \"plugin-result-00\") (data (i32.const 4096) \"{payload}\")"))
        .replace("(memory (export \"memory\") 1)", "(memory (export \"memory\") 8)")
        .replace("(core module $wrapper", "(core module $wrapper (import \"host\" \"memory\" (memory 1))")
        .replace("(export \"write\" (func $write-core))", "(export \"write\" (func $write-core)) (export \"memory\" (memory $mem \"memory\"))")
        .replace("(func (export \"run\") (param i32 i32) (result i32)\n      i32.const 512 i32.const 13 local.get 0 local.get 1 i32.const 16 call $write i32.const 16)", r#"(func (export "run") (param i32 i32) (result i32) (local $i i32)
          (loop $again
            i32.const 526 i32.const 48 local.get $i i32.const 10 i32.rem_u i32.add i32.store8
            i32.const 525 i32.const 48 local.get $i i32.const 10 i32.div_u i32.add i32.store8
            i32.const 512 i32.const 15 i32.const 4096 i32.const 123456 i32.const 16 call $write
            local.get $i i32.const 1 i32.add local.tee $i i32.const 40 i32.lt_u br_if $again)
          i32.const 16)"#)
        .replace("i32.const 123456", &format!("i32.const {}", payload.len()))
}

#[tokio::test]
async fn repeated_guest_writes_cannot_exhaust_artifact_storage() {
    let fixture = Fixture::new();
    let repeated = repeated_writer("small bounded output");
    assert_ne!(repeated, WRITE);
    let path = fixture.package(&repeated, "1.0", &["write_artifacts"]);
    fixture.install(&path, &["write_artifacts"], false).await;
    let result = fixture.run("small bounded output").await.unwrap();
    assert_eq!(result.status, "failed");
    assert_eq!(result.artifacts.len(), 32);
    assert_eq!(
        std::fs::read_dir(&fixture.config.artifacts)
            .unwrap()
            .count(),
        32
    );
    for path in result.artifacts {
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "small bounded output"
        );
    }
}

#[tokio::test]
async fn repeated_guest_writes_respect_the_aggregate_byte_budget() {
    let fixture = Fixture::new();
    let payload = "a".repeat(150_000);
    let path = fixture.package(&repeated_writer(&payload), "1.0", &["write_artifacts"]);
    fixture.install(&path, &["write_artifacts"], false).await;
    let result = fixture.run("").await.unwrap();
    assert_eq!(result.status, "failed");
    assert_eq!(result.artifacts.len(), (4 * 1024 * 1024) / payload.len());
    assert!(result.summary.contains("aggregate artifact byte budget"));
    let bytes: u64 = result
        .artifacts
        .iter()
        .map(|path| std::fs::metadata(path).unwrap().len())
        .sum();
    assert!(bytes <= 4 * 1024 * 1024);
    assert_eq!(
        std::fs::read_dir(&fixture.config.artifacts)
            .unwrap()
            .count(),
        result.artifacts.len()
    );
}

#[tokio::test]
async fn multiple_linear_memories_cannot_multiply_the_declared_budget() {
    let fixture = Fixture::new();
    let component = ECHO.replace("(core instance $instance", "(core module $other (memory (export \"memory\") 1)) (core instance $second (instantiate $other)) (core instance $instance");
    let path = fixture.package(&component, "1.0", &[]);
    fixture.install(&path, &[], false).await;
    assert!(
        fixture.run("x").await.is_err(),
        "The initial ABI permits only one linear memory"
    );
}

#[tokio::test]
async fn guest_error_results_obey_the_same_byte_budget_as_success_results() {
    let fixture = Fixture::new();
    let error_component = ECHO.replace(
        "i32.const 16 i32.const 0 i32.store",
        "i32.const 16 i32.const 1 i32.store",
    );
    let path = fixture.package(&error_component, "1.0", &[]);
    let mut manifest: NativePluginManifest =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest.budgets.max_output_bytes = 8;
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    fixture.install(&path, &[], false).await;
    assert!(
        fixture
            .run("far more than eight bytes")
            .await
            .unwrap_err()
            .to_string()
            .contains("result exceeds its byte budget")
    );
}

#[tokio::test]
async fn cancellation_fuel_limits_and_ungranted_imports_fail_closed() {
    let fixture = Fixture::new();
    let path = fixture.package(ECHO, "1.0", &[]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        execute_management(
            &fixture.config,
            "helper_plugin_inspect",
            &json!({"manifest_path":path}),
            cancel
        )
        .await
        .is_err()
    );
    assert!(fixture.list().await.plugins.is_empty());
    let loop_wat = ECHO.replace(
        "i32.const 16 i32.const 0 i32.store",
        "(loop $forever br $forever)",
    );
    let path = fixture.package_with_budgets(
        &loop_wat,
        "1.0",
        &[],
        PluginBudgets {
            fuel: 1000,
            ..PluginBudgets::default()
        },
    );
    fixture.install(&path, &[], false).await;
    assert!(fixture.run("x").await.is_err());
    control(&fixture.config, "helper_plugin_remove", json!({"operation_id":Uuid::new_v4(),"expected_revision":fixture.list().await.revision,"id":"fixture.echo"})).await.unwrap();
    let path = fixture.package(
        "(component (import \"wasi:cli/run\" (instance)))",
        "2.0",
        &[],
    );
    let inspection = fixture.inspect(&path).await;
    assert!(control(&fixture.config, "helper_plugin_install", json!({"operation_id":Uuid::new_v4(),"expected_revision":fixture.list().await.revision,"manifest_path":path,"expected_package_sha256":inspection.package_sha256,"grants":[],"enabled":true,"replace":false})).await.is_err());
    assert!(fixture.list().await.plugins.is_empty());
}

#[tokio::test]
async fn corrupt_packages_grant_escalation_and_symlink_entrypoints_are_rejected() {
    let fixture = Fixture::new();
    let path = fixture.package(ECHO, "1.0", &[]);
    let review = fixture.inspect(&path).await;
    assert!(control(&fixture.config, "helper_plugin_install", json!({"operation_id":Uuid::new_v4(),"expected_revision":fixture.list().await.revision,"manifest_path":path,"expected_package_sha256":review.package_sha256,"grants":["read_workspace"],"enabled":true,"replace":false})).await.is_err());
    fixture.install(&path, &[], false).await;
    let storage = catalog::root(&fixture.config)
        .unwrap()
        .join("packages")
        .join(review.package_sha256)
        .join("component.wasm");
    std::fs::write(storage, b"tampered").unwrap();
    assert!(
        fixture
            .run("x")
            .await
            .unwrap_err()
            .to_string()
            .contains("checksum")
    );
    std::fs::write(fixture.package.join("component.wat"), b"changed").unwrap();
    assert!(
        control(
            &fixture.config,
            "helper_plugin_inspect",
            json!({"manifest_path":path})
        )
        .await
        .is_err()
    );
    #[cfg(unix)]
    {
        fixture.package(ECHO, "1.0", &[]);
        std::fs::remove_file(fixture.package.join("component.wat")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", fixture.package.join("component.wat")).unwrap();
        assert!(
            control(
                &fixture.config,
                "helper_plugin_inspect",
                json!({"manifest_path":path})
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("symbolic")
        );
    }
}

#[test]
fn https_grants_are_exact_and_management_is_not_discovered_by_agents() {
    let fixture = Fixture::new();
    for url in [
        "http://example.com",
        "https://sub.example.com/",
        "https://example.com:444/",
        "https://user:password@example.com/",
        "https://example.com/#secret",
    ] {
        assert!(
            runtime::validate_url_for_test(fixture.config.clone(), &["https:example.com"], url)
                .is_err(),
            "{url}"
        );
    }
    assert!(
        runtime::validate_url_for_test(
            fixture.config.clone(),
            &["https:example.com"],
            "https://example.com/path"
        )
        .is_ok()
    );
    assert!(
        definitions()
            .iter()
            .all(|definition| !definition.name.starts_with("helper_"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn substituted_management_symlink_is_rejected_before_creating_children() {
    let fixture = Fixture::new();
    let outside = fixture._directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(
        &outside,
        fixture
            .config
            .management_root
            .as_ref()
            .unwrap()
            .join("plugins"),
    )
    .unwrap();
    assert!(
        control(&fixture.config, "helper_plugin_list", json!({}))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
}

#[tokio::test]
async fn model_supplied_capabilities_never_grant_plugin_authority() {
    let fixture = Fixture::new();
    std::fs::write(fixture.config.workspace.join("component.wat"), READ).unwrap();
    for name in ["plugin_run", "plugin_run_wasm"] {
        let error = execute_tool(
            &fixture.config,
            name,
            &json!({"path":"component.wat","input":"secret.txt","capabilities":["read_workspace","https:example.com"]}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("Unknown plugin tool"),
            "{error:#}"
        );
    }
    assert!(definitions().iter().all(|tool| tool.name != "plugin_run"));

    let path = fixture.package(READ, "1.0", &["read_workspace", "https:example.com"]);
    fixture.install(&path, &[], false).await;
    let pin = fixture.list().await.plugins[0].package_sha256.clone();
    let escalation = json!({"id":"fixture.echo","package_sha256":pin,"input":"notes.txt","capabilities":["read_workspace","https:example.com"]});
    assert!(
        execute_tool(
            &fixture.config,
            "plugin_run_installed",
            &escalation,
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("Installed runs accept only")
    );
    let presentation =
        approval_presentation(&fixture.config, &escalation, CancellationToken::new())
            .await
            .unwrap();
    assert_eq!(presentation["effective_grants"], json!([]));
    assert_eq!(presentation["plugin"]["package_sha256"], json!(pin));
    std::fs::write(fixture.config.workspace.join("notes.txt"), "private").unwrap();
    let output = fixture.run("notes.txt").await.unwrap_err();
    assert!(
        output
            .to_string()
            .contains("read_workspace capability not granted"),
        "{output:#}"
    );
}

#[tokio::test]
async fn an_unreadable_catalog_is_replaced_and_grants_nothing() {
    let fixture = Fixture::new();
    let root = catalog::root(&fixture.config).unwrap();
    std::fs::write(
        root.join("catalog.json"),
        br#"{"format":0,"plugins":{"old":{}}}"#,
    )
    .unwrap();
    assert!(fixture.list().await.plugins.is_empty());
    let path = fixture.package(ECHO, "1.0", &[]);
    fixture.install(&path, &[], false).await;
    assert_eq!(fixture.list().await.plugins.len(), 1);
    assert!(fixture.run("hello").await.is_ok());
}
