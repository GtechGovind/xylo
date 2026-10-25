//! Separate durable form namespace. Pending IDs are never silently discarded.
use super::*;
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewForm {
    pub selected: Option<Uuid>,
    pub evidence: String,
    pub outcome: Option<ActionResolutionOutcome>,
    pub pending: Option<ActionResolutionRequest>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewFormSnapshot {
    pub scope: ActionReviewScope,
    pub revision: u64,
    pub form: ActionReviewForm,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewFormWrite {
    pub scope: ActionReviewScope,
    pub expected_revision: u64,
    pub form: ActionReviewForm,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewFormSave {
    pub saved: bool,
    pub snapshot: ActionReviewFormSnapshot,
}
fn setup(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS action_review_forms(session_id TEXT PRIMARY KEY,workspace TEXT NOT NULL,revision INTEGER NOT NULL,json TEXT NOT NULL CHECK(length(CAST(json AS BLOB))<=16384));")?;
    Ok(())
}
fn read(conn: &Connection, scope: &ActionReviewScope) -> Result<ActionReviewFormSnapshot> {
    let row: Option<(String, i64, String)> = conn
        .query_row(
            "SELECT workspace,revision,json FROM action_review_forms WHERE session_id=?1",
            [scope.session_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match row {
        Some((workspace, revision, json)) => {
            ensure!(
                std::path::Path::new(&workspace) == scope.workspace.as_path(),
                "Saved review belongs to another project"
            );
            Ok(ActionReviewFormSnapshot {
                scope: scope.clone(),
                revision: revision as u64,
                form: serde_json::from_str(&json)?,
            })
        }
        None => Ok(ActionReviewFormSnapshot {
            scope: scope.clone(),
            revision: 0,
            form: ActionReviewForm::default(),
        }),
    }
}
pub(super) fn pending(
    conn: &Connection,
    scope: &ActionReviewScope,
) -> Result<Option<ActionResolutionRequest>> {
    setup(conn)?;
    Ok(read(conn, scope)?.form.pending)
}
impl Store {
    pub fn action_review_form(&self, input: ActionReviewScope) -> Result<ActionReviewFormSnapshot> {
        let scope = scope(input)?;
        self.call(move |conn| {
            setup(conn)?;
            check_scope(conn, &scope)?;
            read(conn, &scope)
        })
    }
    pub fn save_action_review_form(
        &self,
        mut write: ActionReviewFormWrite,
    ) -> Result<ActionReviewFormSave> {
        write.scope = scope(write.scope)?;
        ensure!(
            write.form.evidence.len() <= 4096 && !write.form.evidence.contains('\0'),
            "Keep the observed outcome within 4096 bytes"
        );
        if let Some(pending) = &write.form.pending {
            validate_request(pending)?;
            ensure!(
                pending.scope == write.scope && write.form.selected == Some(pending.action_id),
                "Pending review belongs to another action, chat or project"
            );
        }
        let json = serde_json::to_string(&write.form)?;
        ensure!(
            json.len() <= 16384,
            "This recovery form is too large to save safely"
        );
        self.call(move|conn|{setup(conn)?;let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;check_scope(&tx,&write.scope)?;let previous=read(&tx,&write.scope)?;
            if previous.revision!=write.expected_revision{return Ok(ActionReviewFormSave{saved:false,snapshot:previous})}
            if let Some(new_pending)=&write.form.pending
                && previous.form.pending.as_ref()!=Some(new_pending)
            {
                if let Some(recorded)=super::receipt(&tx,&write.scope,new_pending.operation_id)? {
                    ensure!(recorded.action_id==new_pending.action_id&&recorded.outcome==new_pending.outcome&&recorded.evidence==new_pending.evidence&&recorded.original_revision==new_pending.expected_revision,"The pending review does not match its recorded outcome");
                }else{
                    let(action,seq)=super::read(&tx,new_pending.action_id,&write.scope)?;
                    ensure!(action.state=="outcome_unknown"&&super::revision(action.intent.id,action.intent.session_id,&action.state,action.updated_at,seq)==new_pending.expected_revision,"This action changed before the review could be saved; refresh it first");
                }
            }
            if let Some(pending)=&previous.form.pending
                && write.form.pending.as_ref()!=Some(pending)
            {
                let recorded=super::receipt(&tx,&write.scope,pending.operation_id)?.context("Check the recorded outcome before replacing or clearing this pending review")?;
                ensure!(recorded.action_id==pending.action_id&&recorded.scope==pending.scope&&recorded.outcome==pending.outcome&&recorded.evidence==pending.evidence&&recorded.original_revision==pending.expected_revision,"The recorded outcome does not match this pending review");
            }
            let count:i64=tx.query_row("SELECT count(*) FROM action_review_forms",[],|r|r.get(0))?;ensure!(previous.revision>0||count<512,"Recovery form storage is full; saved reviews remain preserved");
            let next=previous.revision.checked_add(1).filter(|v|*v<=i64::MAX as u64).context("Review revision limit reached")?;
            tx.execute("INSERT INTO action_review_forms VALUES(?1,?2,?3,?4) ON CONFLICT(session_id) DO UPDATE SET workspace=excluded.workspace,revision=excluded.revision,json=excluded.json",params![write.scope.session_id.to_string(),write.scope.workspace.to_string_lossy(),next as i64,json])?;tx.commit()?;Ok(ActionReviewFormSave{saved:true,snapshot:ActionReviewFormSnapshot{scope:write.scope,revision:next,form:write.form}})
        })
    }
}
