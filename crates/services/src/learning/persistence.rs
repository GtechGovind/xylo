use super::*;
use std::collections::HashSet;

fn load_skill(conn: &Connection, name: &str, version: i64) -> Result<LearnedSkill> {
    let json: String = conn.query_row(
        "SELECT json FROM learning_candidates WHERE name=?1 AND version=?2",
        params![name, version],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&json)?)
}
fn render(skill: &LearnedSkill) -> String {
    let guidance = skill
        .correction_guidance
        .as_ref()
        .map_or_else(String::new, |guidance| {
            format!("User correction: {guidance}\n")
        });
    let mut text = format!(
        "{}\n{guidance}Advisory procedure; use current task inputs and existing approvals.\n",
        skill.title
    );
    for (index, step) in skill.steps.iter().enumerate() {
        text.push_str(&format!(
            "{}. {} {}. {}{}\n",
            index + 1,
            step.tool,
            step.arguments,
            if step.requires_approval {
                "Requires current approval. "
            } else {
                ""
            },
            step.verification
        ));
    }
    text
}
fn insert_version(conn: &Connection, mut skill: LearnedSkill) -> Result<LearnedSkill> {
    skill.skill.version = conn.query_row(
        "SELECT COALESCE(MAX(version),0)+1 FROM skills WHERE name=?1",
        [&skill.skill.name],
        |row| row.get(0),
    )?;
    skill.skill.content = render(&skill);
    ensure!(
        skill.skill.content.len() <= 16_384,
        "Learned procedure exceeds its content budget"
    );
    conn.execute(
        "INSERT INTO skills(name,version,json) VALUES(?1,?2,?3)",
        params![
            skill.skill.name,
            skill.skill.version,
            serde_json::to_string(&skill.skill)?
        ],
    )?;
    conn.execute("INSERT INTO learning_candidates(name,version,scope,workflow_key,state,json) VALUES(?1,?2,?3,?4,?5,?6)",params![skill.skill.name,skill.skill.version,skill.scope,skill.workflow_key,skill.state,serde_json::to_string(&skill)?])?;
    Ok(skill)
}
fn save(conn: &Connection, skill: &LearnedSkill) -> Result<()> {
    conn.execute(
        "UPDATE skills SET json=?1 WHERE name=?2 AND version=?3",
        params![
            serde_json::to_string(&skill.skill)?,
            skill.skill.name,
            skill.skill.version
        ],
    )?;
    conn.execute("UPDATE learning_candidates SET workflow_key=?1,state=?2,json=?3 WHERE name=?4 AND version=?5",params![skill.workflow_key,skill.state,serde_json::to_string(skill)?,skill.skill.name,skill.skill.version])?;
    conn.execute(
        "DELETE FROM learning_fts WHERE name=?1 AND version=?2",
        params![skill.skill.name, skill.skill.version],
    )?;
    if skill.state == "promoted" {
        conn.execute(
            "INSERT INTO learning_fts(name,version,scope,content) VALUES(?1,?2,?3,?4)",
            params![
                skill.skill.name,
                skill.skill.version,
                skill.scope,
                format!("{} {}", skill.triggers.join(" "), skill.skill.content)
            ],
        )?;
    }
    Ok(())
}
fn promote(conn: &Connection, skill: &mut LearnedSkill) -> Result<()> {
    ensure!(
        skill
            .evaluation
            .as_ref()
            .is_some_and(|evaluation| evaluation.passed),
        "Learning evaluation has not passed"
    );
    conn.execute("UPDATE learning_candidates SET state='superseded',json=json_set(json,'$.state','superseded') WHERE name=?1 AND version!=?2 AND state='promoted'",params![skill.skill.name,skill.skill.version])?;
    conn.execute(
        "DELETE FROM learning_fts WHERE name=?1",
        [&skill.skill.name],
    )?;
    conn.execute(
        "INSERT INTO skill_promotions(name,version,created_at) VALUES(?1,?2,?3)",
        params![skill.skill.name, skill.skill.version, now_ms()],
    )?;
    skill.state = "promoted".into();
    Ok(())
}
fn completed_case(conn: &Connection, session: Uuid, turn: Uuid) -> Result<Option<ObservedCase>> {
    let conversation: String = conn.query_row(
        "SELECT json FROM conversations WHERE id=?1",
        [session.to_string()],
        |row| row.get(0),
    )?;
    let conversation: Conversation = serde_json::from_str(&conversation)?;
    let scope = conversation.workspace.to_string_lossy().into_owned();
    let ordinal: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM completed_turns WHERE session_id=?1 AND turn_id=?2",
            params![session.to_string(), turn.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let user_id = Uuid::new_v5(&turn, b"user-message");
    let user: Option<String> = conn
        .query_row(
            "SELECT json FROM messages WHERE id=?1 AND session_id=?2",
            params![user_id.to_string(), session.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let user = user
        .map(|json| serde_json::from_str::<ChatMessage>(&json))
        .transpose()?;
    let (count,max_size):(i64,i64)=conn.query_row("SELECT count(*),COALESCE(MAX(length(intent)),0) FROM actions WHERE session_id=?1 AND turn_id=?2",params![session.to_string(),turn.to_string()],|row|Ok((row.get(0)?,row.get(1)?)))?;
    ensure!(
        count <= 64 && max_size <= 65536,
        "Turn evidence exceeds the automatic learning budget"
    );
    let mut statement=conn.prepare("SELECT intent,state,CASE WHEN length(result)<=262144 THEN result ELSE NULL END,error,updated_at FROM actions WHERE session_id=?1 AND turn_id=?2 AND length(intent)<=65536 ORDER BY rowid LIMIT 65")?;
    let rows = statement.query_map(params![session.to_string(), turn.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    let mut actions = Vec::new();
    for row in rows {
        let (intent, state, result, error, updated_at) = row?;
        let intent: ActionIntent = serde_json::from_str(&intent)?;
        if sanitize::ignored_tool(&intent.name) {
            continue;
        }
        actions.push(JournalAction {
            intent,
            state,
            result: result.map(|json| serde_json::from_str(&json)).transpose()?,
            error,
            updated_at,
        });
        if actions.len() > MAX_STEPS {
            break;
        }
    }
    if actions.is_empty() {
        return Ok(None);
    }
    ensure!(
        !actions
            .iter()
            .any(|action| matches!(action.state.as_str(), "pending" | "running")),
        "Learning needs a terminal turn"
    );
    let (steps, safe) = sanitize::build_steps(&actions, &scope);
    let source_terms = user
        .as_ref()
        .filter(|user| user.role == "user" && user.content.len() <= 32_768)
        .map(|user| sanitize::terms(&user.content))
        .unwrap_or_default();
    let eligible = ordinal.is_some()
        && user.is_some()
        && !source_terms.is_empty()
        && safe
        && actions.len() <= MAX_STEPS
        && actions.iter().all(sanitize::result_verified);
    let reason = if ordinal.is_none() {
        "No completed-turn receipt"
    } else if !safe {
        "External write or unrecognized execution needs manual evaluation"
    } else if source_terms.is_empty() {
        "Missing or sensitive user source"
    } else if actions.len() > MAX_STEPS {
        "Workflow exceeds the automatic learning budget"
    } else if !actions.iter().all(sanitize::result_verified) {
        "Failed, uncertain, missing or oversized result evidence"
    } else {
        "Verified completed action results"
    };
    let workflow_key = digest(&steps)?;
    let evidence = LearningEvidence {
        session_id: session,
        turn_id: turn,
        action_ids: actions.iter().map(|action| action.intent.id).collect(),
        result_digests: actions
            .iter()
            .map(|action| digest(&action.result))
            .collect::<Result<_>>()?,
        completed_ordinal: ordinal,
    };
    Ok(Some(ObservedCase {
        scope,
        terms: source_terms,
        workflow_key,
        steps,
        evidence,
        eligible,
        reason: reason.into(),
    }))
}
fn case_is_current(conn: &Connection, case: &ObservedCase) -> Result<bool> {
    if !case.eligible {
        return Ok(false);
    }
    let receipt: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM completed_turns WHERE session_id=?1 AND turn_id=?2)",
        params![
            case.evidence.session_id.to_string(),
            case.evidence.turn_id.to_string()
        ],
        |row| row.get(0),
    )?;
    if !receipt {
        return Ok(false);
    }
    let ids = serde_json::to_string(&case.evidence.action_ids)?;
    let valid=conn.query_row("SELECT count(*) FROM actions WHERE id IN (SELECT value FROM json_each(?1)) AND state='succeeded' AND result IS NOT NULL",[ids],|row|crate::store::row_usize(row,0))?;
    Ok(valid == case.evidence.action_ids.len())
}
fn evaluate(conn: &Connection, skill: &mut LearnedSkill) -> Result<()> {
    let Some(key) = &skill.workflow_key else {
        return Ok(());
    };
    let mut statement=conn.prepare("SELECT json FROM learning_cases WHERE scope=?1 AND workflow_key=?2 AND id>=?3 ORDER BY id DESC LIMIT ?4")?;
    let rows = statement.query_map(
        params![skill.scope, key, skill.after_case_id, MAX_CASES as i64],
        |row| row.get::<_, String>(0),
    )?;
    let mut cases = Vec::new();
    let mut proofs = Vec::new();
    let mut distinct = HashSet::new();
    let mut held_out = false;
    let mut negative:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM learning_cases WHERE scope=?1 AND workflow_key=?2 AND json_extract(json,'$.eligible')=0 AND id>=?3)",params![skill.scope,key,skill.after_case_id],|row|row.get(0))?;
    for row in rows {
        let case: ObservedCase = serde_json::from_str(&row?)?;
        let later = case
            .evidence
            .completed_ordinal
            .is_some_and(|ordinal| ordinal > skill.after_ordinal);
        if skill.correction_message_id.is_some() && case.eligible && !later {
            continue;
        }
        let passed = case_is_current(conn, &case)?;
        if passed {
            distinct.insert((case.evidence.session_id, case.evidence.turn_id));
            held_out |= later;
            if proofs.len() < 4 {
                proofs.push(case.evidence.clone());
            }
        } else {
            negative = true;
        }
        cases.push(EvaluationCase {
            session_id: case.evidence.session_id,
            turn_id: case.evidence.turn_id,
            passed,
            reason: case.reason,
        });
    }
    let guidance_verified = skill.correction_message_id.is_none()
        || skill.correction_command.is_some()
        || skill.review.is_some();
    let passed = distinct.len() >= 2 && held_out && !negative && guidance_verified;
    skill.provenance = proofs;
    let evaluation=LearningEvaluation{evaluator:"historical-outcome-template/v1".into(),passed,score:if passed{1.0}else{0.0},checks:vec![format!("{} distinct successful completed turns; require at least 2",distinct.len()),format!("Independent later evidence: {held_out}"),format!("Failed/unknown/external-write counterexamples: {negative}"),format!("Correction semantics checked or explicitly reviewed: {guidance_verified}"),"Exact workspace, typed procedure and current durable action states verified; no tool is executed by evaluation".into()],cases,evaluated_at:now_ms(),limitation:"Checks historical outcome/template consistency. It does not prove task-quality improvement, causal reuse, command sandbox safety, or benchmark/model training results.".into()};
    skill.evaluation = Some(evaluation);
    Ok(())
}
/// Attempts per job step before a busy database defers the job to a later pass.
const PROCESS_ATTEMPTS: u32 = 3;
const PROCESS_BACKOFF: Duration = Duration::from_millis(250);
/// Lock contention from another connection or a full database queue. Both clear
/// on their own, unlike corrupt evidence or a missing source conversation.
pub(super) fn transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|error| {
                matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                )
            })
            || cause.to_string().starts_with("The database is busy")
    })
}
/// Runs `operation` up to `attempts` times, sleeping with exponential backoff
/// between attempts that fail transiently. Other errors return immediately.
/// Blocking: call from a blocking thread, never an async worker.
pub(super) fn retry_transient<T>(
    attempts: u32,
    backoff: Duration,
    mut operation: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut delay = backoff;
    for attempt in 1..attempts.max(1) {
        match operation() {
            Err(error) if transient(&error) => {
                tracing::debug!(attempt,error=%error,"Retrying busy learning database step");
                std::thread::sleep(delay);
                delay = delay.saturating_mul(2);
            }
            result => return result,
        }
    }
    operation()
}
impl LearningService {
    pub(super) fn active_impl(&self, scope: &str, name: &str) -> Result<Option<LearnedSkill>> {
        let scope = scope.to_owned();
        let name = name.to_owned();
        self.store.call(move|conn|{let json:Option<String>=conn.query_row("SELECT c.json FROM learning_candidates c JOIN skill_promotions p ON p.name=c.name AND p.version=c.version WHERE c.scope=?1 AND c.name=?2 AND c.state='promoted' AND p.id=(SELECT MAX(id) FROM skill_promotions WHERE name=c.name)",params![scope,name],|row|row.get(0)).optional()?;json.map(|json|serde_json::from_str(&json).map_err(Into::into)).transpose()})
    }
    pub(super) fn queue_latest_impl(&self, session: Uuid) -> Result<Option<Uuid>> {
        self.store.call(move|conn|{let tx=crate::store::write_transaction(conn)?;let turn:Option<String>=tx.query_row("SELECT turn_id FROM completed_turns WHERE session_id=?1 ORDER BY rowid DESC LIMIT 1",[session.to_string()],|row|row.get(0)).optional()?;if let Some(turn)=&turn {tx.execute("INSERT INTO learning_jobs(session_id,turn_id,state,created_at,updated_at) VALUES(?1,?2,'pending',?3,?3) ON CONFLICT(session_id,turn_id) DO UPDATE SET state=CASE WHEN state='processing' THEN state ELSE 'pending' END,updated_at=excluded.updated_at",params![session.to_string(),turn,now_ms()])?;}tx.commit()?;turn.map(|turn|Uuid::parse_str(&turn).map_err(Into::into)).transpose()})
    }
    pub(super) fn process_one(&self) -> Result<Option<LearningBatch>> {
        let owner = self.owner.to_string();
        let claim_owner = owner.clone();
        let job=self.store.call(move|conn|{
            let tx=crate::store::write_transaction(conn)?;
            let row:Option<(i64,String,String)>=tx.query_row("SELECT id,session_id,turn_id FROM learning_jobs WHERE state='pending' OR (state='processing' AND lease_until<?1) ORDER BY id LIMIT 1",[now_ms()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
            if let Some((id,_,_))=&row{tx.execute("UPDATE learning_jobs SET state='processing',owner=?1,lease_until=?2,updated_at=?3 WHERE id=?4",params![claim_owner,now_ms()+60_000,now_ms(),id])?;}
            tx.commit()?;Ok(row)
        })?;
        let Some((id, session, turn)) = job else {
            return Ok(None);
        };
        let result = retry_transient(PROCESS_ATTEMPTS, PROCESS_BACKOFF, || {
            self.process_job(id, owner.clone(), session.clone(), turn.clone())
        });
        match result {
            Ok(batch) => Ok(Some(batch)),
            // Contention outlasted every bounded attempt. Keep the lease so a later
            // pass resumes the job once it expires; busy is never a permanent failure.
            Err(error) if transient(&error) => {
                tracing::warn!(job_id=id,error=%error,"Procedural learning job deferred while the database is busy");
                Ok(Some(LearningBatch::default()))
            }
            Err(error) => {
                tracing::warn!(job_id=id,error=%error,"Procedural learning job failed");
                let failed_jobs = match self.mark_failed(id) {
                    Ok(marked) => usize::from(marked),
                    // The job stays leased; it is retried after the lease expires.
                    Err(error) => {
                        tracing::warn!(job_id=id,error=%error,"Could not record the failed learning job");
                        0
                    }
                };
                Ok(Some(LearningBatch {
                    failed_jobs,
                    ..Default::default()
                }))
            }
        }
    }
    /// Records a permanent failure only while this service still owns the lease;
    /// a job reclaimed by another owner after expiry is left to that owner.
    pub(super) fn mark_failed(&self, id: i64) -> Result<bool> {
        let owner = self.owner.to_string();
        retry_transient(PROCESS_ATTEMPTS, PROCESS_BACKOFF, || {
            let owner = owner.clone();
            self.store.call(move|conn|Ok(conn.execute("UPDATE learning_jobs SET state='failed',owner=NULL,lease_until=0,error='Evidence processing failed; retry after reviewing the source',updated_at=?1 WHERE id=?2 AND owner=?3 AND state='processing'",params![now_ms(),id,owner])?==1))
        })
    }
    fn process_job(
        &self,
        id: i64,
        owner: String,
        session: String,
        turn: String,
    ) -> Result<LearningBatch> {
        self.store.call(move|conn|{
            let tx=crate::store::write_transaction(conn)?;
            let owned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM learning_jobs WHERE id=?1 AND owner=?2 AND state='processing')",params![id,owner],|row|row.get(0))?;
            ensure!(owned,"Learning job lease changed");
            let mut batch=LearningBatch::default();
            if let Some(case)=completed_case(&tx,Uuid::parse_str(&session)?,Uuid::parse_str(&turn)?)?{
                tx.execute("INSERT INTO learning_cases(session_id,turn_id,scope,workflow_key,json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session_id,turn_id) DO UPDATE SET scope=excluded.scope,workflow_key=excluded.workflow_key,json=excluded.json",params![session,turn,case.scope,case.workflow_key,serde_json::to_string(&case)?])?;
                let case_id:i64=tx.query_row("SELECT id FROM learning_cases WHERE session_id=?1 AND turn_id=?2",params![session,turn],|row|row.get(0))?;
                // The bounded evaluation batch must never leave an older promoted
                // version retrievable after matching counterevidence.
                if !case.eligible {
                    batch.suspended+=tx.execute("UPDATE learning_candidates SET state='suspended',json=json_set(json,'$.state','suspended') WHERE scope=?1 AND workflow_key=?2 AND state='promoted' AND json_extract(json,'$.after_case_id')<=?3",params![case.scope,case.workflow_key,case_id])?;
                    tx.execute("DELETE FROM learning_fts WHERE scope=?1 AND name IN (SELECT name FROM learning_candidates WHERE scope=?1 AND workflow_key=?2 AND state='suspended')",params![case.scope,case.workflow_key])?;
                }
                let existing:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM learning_candidates WHERE scope=?1 AND workflow_key=?2 AND json_extract(json,'$.correction_message_id') IS NULL)",params![case.scope,case.workflow_key],|row|row.get(0))?;
                if case.eligible && !existing && (case.steps.len()>=2 || case.steps.iter().any(|step|step.tool=="run_command")) {
                    let scope_key=digest(&case.scope)?;
                    let name=format!("learned-{}-{}",&scope_key[..10],&case.workflow_key[..16]);
                    let title=format!("Verified {} workflow",case.steps.iter().take(3).map(|step|step.tool.as_str()).collect::<Vec<_>>().join(" → "));
                    insert_version(&tx,LearnedSkill{skill:SkillVersion{name,version:0,content:String::new(),created_at:now_ms()},scope:case.scope.clone(),title,state:"candidate".into(),triggers:case.terms.clone(),steps:case.steps.clone(),provenance:vec![case.evidence.clone()],correction_message_id:None,correction_guidance:None,correction_command:None,review:None,evaluation:None,workflow_key:Some(case.workflow_key.clone()),after_ordinal:case.evidence.completed_ordinal.unwrap_or(0),after_case_id:case_id,expected_tools:Vec::new()})?;batch.candidates+=1;
                }
                let candidates={let mut statement=tx.prepare("SELECT json FROM learning_candidates WHERE scope=?1 AND (workflow_key=?2 OR workflow_key IS NULL) AND state IN ('candidate','promoted','suspended') ORDER BY rowid DESC LIMIT 16")?;statement.query_map(params![case.scope,case.workflow_key],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?};
                for candidate in candidates {
                    let mut skill:LearnedSkill=serde_json::from_str(&candidate)?;
                    if skill.workflow_key.is_none() && case.eligible && case.evidence.completed_ordinal.is_some_and(|ordinal|ordinal>skill.after_ordinal) {
                        let tools=case.steps.iter().map(|step|step.tool.clone()).collect::<Vec<_>>();
                        let terms_match=skill.triggers.iter().filter(|term|case.terms.contains(term)).count()>=2;
                        let command_matches=skill.correction_command.as_ref().is_none_or(|expected|case.steps.iter().any(|step|step.tool=="run_command" && &step.arguments==expected));
                        if command_matches && ((skill.expected_tools.is_empty() && terms_match)||(!skill.expected_tools.is_empty() && skill.expected_tools==tools)){skill.workflow_key=Some(case.workflow_key.clone());skill.steps=case.steps.clone();skill.skill.content=render(&skill);}
                    }
                    if skill.workflow_key.as_deref()!=Some(case.workflow_key.as_str()){continue;}
                    evaluate(&tx,&mut skill)?;
                    if skill.evaluation.as_ref().is_some_and(|evaluation|evaluation.passed) {
                        if skill.state=="candidate" {promote(&tx,&mut skill)?;batch.promoted+=1;}
                    }else if skill.state=="promoted" {skill.state="suspended".into();batch.suspended+=1;}
                    save(&tx,&skill)?;
                }
                tx.execute("UPDATE learning_retrievals SET outcome=CASE WHEN EXISTS(SELECT 1 FROM learning_candidates c WHERE c.name=learning_retrievals.name AND c.version=learning_retrievals.version AND c.workflow_key=?1) THEN ?2 ELSE 'retrieved_not_followed' END WHERE session_id=?3 AND turn_id=?4",params![case.workflow_key,if case.eligible{"verified_reuse"}else{"failed_or_unknown"},session,turn])?;
            }
            tx.execute("UPDATE learning_jobs SET state='done',owner=NULL,lease_until=0,error=NULL,updated_at=?1 WHERE id=?2",params![now_ms(),id])?;
            tx.commit()?;Ok(batch)
        })
    }
    pub(super) fn backfill_impl(
        &self,
        before: Option<i64>,
        limit: usize,
    ) -> Result<(Option<i64>, bool)> {
        self.store.call(move|conn|{let tx=crate::store::write_transaction(conn)?;let mut statement=tx.prepare("SELECT rowid,session_id,turn_id FROM completed_turns WHERE (?1 IS NULL OR rowid<?1) ORDER BY rowid DESC LIMIT ?2")?;let mut rows=statement.query_map(params![before,(limit.clamp(1,200)+1) as i64],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;drop(statement);let more=rows.len()>limit.clamp(1,200);rows.truncate(limit.clamp(1,200));for(_,session,turn)in rows.iter().rev(){tx.execute("INSERT OR IGNORE INTO learning_jobs(session_id,turn_id,state,created_at,updated_at) VALUES(?1,?2,'pending',?3,?3)",params![session,turn,now_ms()])?;}let before=rows.last().map(|row|row.0);tx.commit()?;Ok((before,more))})
    }
    pub(super) fn status_impl(&self, scope: &str) -> Result<LearningStatus> {
        let scope = scope.to_owned();
        self.store.call(move|conn|Ok(LearningStatus{pending_jobs:conn.query_row("SELECT count(*) FROM learning_jobs WHERE state IN ('pending','processing')",[],|row|crate::store::row_usize(row,0))?,failed_jobs:conn.query_row("SELECT count(*) FROM learning_jobs WHERE state='failed'",[],|row|crate::store::row_usize(row,0))?,candidates:conn.query_row("SELECT count(*) FROM learning_candidates WHERE scope=?1 AND state='candidate'",[&scope],|row|crate::store::row_usize(row,0))?,promoted:conn.query_row("SELECT count(*) FROM learning_candidates WHERE scope=?1 AND state='promoted'",[&scope],|row|crate::store::row_usize(row,0))?,verified_reuses:conn.query_row("SELECT count(*) FROM learning_retrievals WHERE scope=?1 AND outcome='verified_reuse'",[&scope],|row|crate::store::row_usize(row,0))?}))
    }
    pub(super) fn list_impl(
        &self,
        scope: &str,
        name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LearnedSkill>> {
        let scope = scope.to_owned();
        let name = name.map(str::to_owned);
        self.store.call(move|conn|{let mut statement=conn.prepare("SELECT json FROM learning_candidates WHERE scope=?1 AND (?2 IS NULL OR name=?2) ORDER BY rowid DESC LIMIT ?3")?;let rows=statement.query_map(params![scope,name,limit.clamp(1,200) as i64],|row|row.get::<_,String>(0))?;rows.map(|row|Ok(serde_json::from_str(&row?)?)).collect()})
    }
    pub(super) fn correction_impl(
        &self,
        input: CorrectionInput,
        expected_scope: Option<String>,
    ) -> Result<LearnedSkill> {
        ensure!(
            !input.guidance.trim().is_empty()
                && input.guidance.len() <= 2048
                && !sanitize::sensitive(&input.guidance),
            "Correction needs bounded guidance without credentials or private account data"
        );
        ensure!(
            input.expected_tools.len() <= MAX_STEPS
                && input
                    .expected_tools
                    .iter()
                    .all(|tool| sanitize::valid_tool(tool) && !sanitize::ignored_tool(tool)),
            "Invalid correction tool sequence"
        );
        self.store.call(move|conn|{let tx=crate::store::write_transaction(conn)?;let message:String=tx.query_row("SELECT json FROM messages WHERE id=?1",[input.user_message_id.to_string()],|row|row.get(0))?;let message:ChatMessage=serde_json::from_str(&message)?;ensure!(message.role=="user" && message.content.contains(&input.guidance),"Correction must quote a persisted user message");let conversation:String=tx.query_row("SELECT json FROM conversations WHERE id=?1",[message.session_id.to_string()],|row|row.get(0))?;let conversation:Conversation=serde_json::from_str(&conversation)?;let scope=conversation.workspace.to_string_lossy().into_owned();
            ensure!(expected_scope.as_ref().is_none_or(|expected|expected==&scope),"Correction source is outside the selected workspace");
            let after:i64=tx.query_row("SELECT COALESCE(MAX(rowid),0) FROM completed_turns",[],|row|row.get(0))?;let after_case:i64=tx.query_row("SELECT COALESCE(MAX(id),0) FROM learning_cases",[],|row|row.get(0))?;
            let name=if let Some(previous)=&input.replaces{let old=load_skill(&tx,&previous.name,previous.version)?;ensure!(old.scope==scope,"Correction cannot replace another workspace's skill");tx.execute("UPDATE learning_candidates SET state='suspended',json=json_set(json,'$.state','suspended') WHERE name=?1 AND state='promoted'",[&previous.name])?;tx.execute("DELETE FROM learning_fts WHERE name=?1",[&previous.name])?;previous.name.clone()}else{let key=digest(&(scope.as_str(),input.user_message_id,input.guidance.as_str()))?;format!("correction-{}",&key[..24])};
            let duplicate:Option<String>=tx.query_row("SELECT json FROM learning_candidates WHERE name=?1 AND json_extract(json,'$.correction_message_id')=?2 AND json_extract(json,'$.correction_guidance')=?3 ORDER BY version DESC LIMIT 1",params![name,input.user_message_id.to_string(),input.guidance],|row|row.get(0)).optional()?;
            if let Some(duplicate)=duplicate {return Ok(serde_json::from_str(&duplicate)?);}
            let skill=insert_version(&tx,LearnedSkill{skill:SkillVersion{name,version:0,content:format!("User correction: {}",input.guidance),created_at:now_ms()},scope:scope.clone(),title:"Sourced user correction".into(),state:"candidate".into(),triggers:sanitize::terms(&input.guidance),steps:Vec::new(),provenance:Vec::new(),correction_message_id:Some(input.user_message_id),correction_command:sanitize::correction_command(&input.guidance,&scope),correction_guidance:Some(input.guidance),review:None,evaluation:None,workflow_key:None,after_ordinal:after,after_case_id:after_case+1,expected_tools:input.expected_tools})?;tx.commit()?;Ok(skill)})
    }
    pub(super) fn relevant_impl(
        &self,
        scope: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LearnedSkill>> {
        let terms = sanitize::terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let query = terms
            .iter()
            .take(12)
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        let scope = scope.to_owned();
        self.store.call(move|conn|{let mut statement=conn.prepare("SELECT c.json FROM learning_fts f JOIN learning_candidates c ON c.name=f.name AND c.version=CAST(f.version AS INTEGER) JOIN skill_promotions p ON p.name=c.name AND p.version=c.version JOIN (SELECT name,MAX(id) id FROM skill_promotions GROUP BY name) latest ON latest.id=p.id WHERE learning_fts MATCH ?1 AND f.scope=?2 AND c.scope=?2 AND c.state='promoted' ORDER BY bm25(learning_fts),c.name LIMIT ?3")?;let rows=statement.query_map(params![query,scope,limit.clamp(1,4) as i64],|row|row.get::<_,String>(0))?;rows.map(|row|Ok(serde_json::from_str(&row?)?)).collect()})
    }
    pub(super) fn retrieval_impl(
        &self,
        session: Uuid,
        turn: Uuid,
        skills: &[LearnedSkill],
    ) -> Result<()> {
        let skills = skills
            .iter()
            .take(4)
            .map(|skill| {
                (
                    skill.skill.name.clone(),
                    skill.skill.version,
                    skill.scope.clone(),
                )
            })
            .collect::<Vec<_>>();
        self.store.call(move|conn|{let tx=crate::store::write_transaction(conn)?;let scope:String=tx.query_row("SELECT json_extract(json,'$.workspace') FROM conversations WHERE id=?1",[session.to_string()],|row|row.get(0))?;for(name,version,skill_scope)in skills{ensure!(scope==skill_scope,"Skill retrieval cannot cross workspaces");let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM learning_candidates WHERE name=?1 AND version=?2 AND scope=?3 AND state='promoted')",params![name,version,scope],|row|row.get(0))?;ensure!(active,"Skill is no longer promoted");tx.execute("INSERT OR IGNORE INTO learning_retrievals(session_id,turn_id,name,version,scope,outcome,created_at) VALUES(?1,?2,?3,?4,?5,'retrieved',?6)",params![session.to_string(),turn.to_string(),name,version,scope,now_ms()])?;}tx.commit()?;Ok(())})
    }
    pub(super) fn reuses_impl(&self, scope: &str, limit: usize) -> Result<Vec<SkillReuse>> {
        let scope = scope.to_owned();
        self.store.call(move|conn|{let mut statement=conn.prepare("SELECT session_id,turn_id,name,version,outcome FROM learning_retrievals WHERE scope=?1 ORDER BY rowid DESC LIMIT ?2")?;let rows=statement.query_map(params![scope,limit.clamp(1,200) as i64],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,i64>(3)?,row.get::<_,String>(4)?)))?;rows.map(|row|{let(session,turn,name,version,outcome)=row?;Ok(SkillReuse{session_id:Uuid::parse_str(&session)?,turn_id:Uuid::parse_str(&turn)?,skill:SkillRef{name,version},outcome})}).collect()})
    }
    pub(super) fn review_impl(
        &self,
        scope: &str,
        name: &str,
        version: i64,
        evidence: &str,
    ) -> Result<LearnedSkill> {
        ensure!(
            (16..=2048).contains(&evidence.len()) && !sanitize::sensitive(evidence),
            "Review needs bounded, non-sensitive evidence"
        );
        let scope = scope.to_owned();
        let name = name.to_owned();
        let evidence = evidence.to_owned();
        self.store.call(move |conn| {
            let tx = crate::store::write_transaction(conn)?;
            let mut skill = load_skill(&tx, &name, version)?;
            ensure!(skill.scope == scope, "Review cannot cross workspaces");
            skill.review = Some(SkillReview {
                evidence,
                reviewed_at: now_ms(),
            });
            evaluate(&tx, &mut skill)?;
            promote(&tx, &mut skill)?;
            save(&tx, &skill)?;
            tx.commit()?;
            Ok(skill)
        })
    }
    pub(super) fn rollback_impl(
        &self,
        scope: &str,
        name: &str,
        version: i64,
    ) -> Result<LearnedSkill> {
        let scope = scope.to_owned();
        let name = name.to_owned();
        self.store.call(move |conn| {
            let tx = crate::store::write_transaction(conn)?;
            let mut skill = load_skill(&tx, &name, version)?;
            ensure!(skill.scope == scope, "Rollback cannot cross workspaces");
            evaluate(&tx, &mut skill)?;
            promote(&tx, &mut skill)?;
            save(&tx, &skill)?;
            tx.commit()?;
            Ok(skill)
        })
    }
}
