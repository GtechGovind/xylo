# Application integration patches

1. **Checkpoint observer** — `AgentCallbacks::on_checkpoint` receives the actual
   conversation immediately before a tool batch and after its results. The
   application journals this state durably, independently of the UI.
2. **Tool call identity** — `ToolRegistry::register_with_call_id` lets application
   tools receive the provider tool-call ID without injecting hidden arguments.
   Existing upstream `register` callers retain their original behavior.

These patches contain no alternative agent loop. Updating the pinned engine must
replay cancellation, tool identity, journal and interrupted-action tests.

3. **Native product completion** — `AgentConfig::native_product_finalization`
   defaults false for upstream compatibility. The application enables it to skip
   forced engineering/research presentation retries while retaining the actual
   agent/tool loop, depth, permission, context, journal and cancellation checks.
   Streaming and nonstreaming application tests require one real file-read tool
   plus a natural answer to complete in exactly two provider requests.
4. **Native content replay** — stream assembly retains the application provider's
   `xylo_content_blocks` snapshot on the assistant message. This carries
   Anthropic thinking signatures, Gemini thought signatures, Responses encrypted
   reasoning and Bedrock native content through tool rounds and checkpoints.
5. **Explicit Bedrock credentials and framing** — the Converse provider accepts
   vault-supplied bearer or AWS credentials independently of environment state,
   redacts credential Debug output, configures bounded request deadlines and
   redirect rejection, bounds response/frame sizes, validates AWS stream CRCs,
   requires stream messageStop, compacts native tool indices and retains native
   signed reasoning/tool blocks. HTTP errors avoid reflected response bodies.
   Native tools are serialized for all selected model IDs; model/account support
   remains the service's decision. Actual public model catalog/config/auth logic
   lives in application services, not the upstream curated-model list.

Application HTTP fixtures exercise all five wire protocols and Bedrock binary
stream corruption/truncation. When updating the pin, rerun engine/services tests
and strict Clippy, plus the host's worker JSONL and immediate-resume checks.
External provider/account acceptance remains separate from fixture qualification.

6. **Acknowledged application guidance** — optional
   `AgentCallbacks::on_application_guidance` runs at role-safe boundaries in
   streaming and nonstreaming loops. Identified direct user guidance is committed
   by the application together with its checkpoint and receipt. Guidance arriving
   during final streaming continues the real loop; no interrupt marker, tool
   approval or alternative loop is introduced. Real native-worker fixtures kill
   and resume before/after application and require exactly one guidance message.
   Applied means durable context insertion, not model compliance.
