//! Host-only learning controls. These actions are deliberately absent from the
//! agent's tool catalog: an agent cannot manufacture a user review or correction.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use workbench_services::{CorrectionInput, LearningService, Store};
pub use workbench_services::{LearnedSkill, LearningStatus};

pub(crate) async fn execute(
    store: Store,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<Value> {
    let name = name.to_string();
    let args = args.clone();
    tokio::task::spawn_blocking(move||{
        ensure!(!cancel.is_cancelled(),"Learning action cancelled");
        let service=LearningService::new(store);
        let scope=args.get("scope").and_then(Value::as_str).context("Choose a project")?;
        ensure!(scope.len()<=8192,"Project path is too long");
        let skill_name=||args.get("name").and_then(Value::as_str).context("Choose a procedure");
        let version=||args.get("version").and_then(Value::as_i64).context("Choose a procedure version");
        match name.as_str(){
            "learning_snapshot"=>Ok(json!({"scope":scope,"status":service.status(scope)?,"skills":service.skills(scope,100)?,"reuses":service.reuses(scope,100)?})),
            "learning_versions"=>Ok(json!({"scope":scope,"name":skill_name()?,"versions":service.versions(scope,skill_name()?,50)?})),
            "learning_review"=>{
                let evidence=args.get("evidence").and_then(Value::as_str).context("Describe what you checked")?;
                let skill=service.review(scope,skill_name()?,version()?,evidence)?;
                Ok(json!({"scope":scope,"skill":skill}))
            }
            "learning_rollback"=>Ok(json!({"scope":scope,"skill":service.rollback(scope,skill_name()?,version()?)?})),
            "learning_correction"=>{
                let input:CorrectionInput=serde_json::from_value(args.get("correction").cloned().context("Select a saved user message")?)?;
                let skill=service.record_scoped_correction(scope,&input)?;
                Ok(json!({"scope":scope,"skill":skill}))
            }
            _=>bail!("This learning action is not supported"),
        }
    }).await?
}
