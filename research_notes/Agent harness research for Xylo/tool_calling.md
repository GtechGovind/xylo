# Improving LLM Tool Calling in Agent Harnesses (for Xylo)

Scope: harness-level techniques (no model fine-tuning) that make tool calls more accurate, cheaper and faster, for a Rust desktop harness that has built-in tools, MCP servers, plugins/skills, user approval of risky calls, a sandbox, and both cloud models (Claude/GPT/Gemini) and local models (Ollama/llama.cpp) whose native function calling may be weak or missing. Long-term memory and general context compaction are out of scope except where they concern tool definitions or tool outputs.

How to read this file:
- All arXiv IDs marked "verified" were checked against the arXiv export API during this research session (title, first author and date matched). Items marked "(snippet only)" were seen only in search-result snippets, so their numbers are self-reported and were not read in full. Items marked "(not re-fetched this session)" come from well-known primary sources that I did not re-open during this session; the report writer may want to spot-check them.
- "Harness note" lines are my own implementation inferences for Xylo, not claims from the source.
- Date context: today is 2026-10-31. Benchmark leaderboards from 2025 papers have since been overtaken by newer models. The numbers below describe the paper-time snapshot.

---

## Q1. Tool selection at scale: how does accuracy degrade with dozens or hundreds of tools, and what fixes it?

### Takeaway
Accuracy and cost both get worse as tools are added. Tool definitions alone can take 50K–134K tokens, and long-context function-calling studies report accuracy drops of up to 85% as tool count grows. The best-supported fix is deferred, on-demand tool loading through a search/retrieval step: Anthropic's Tool Search Tool cut definition tokens by 85% and raised MCP-eval accuracy from 49% to 74% (Opus 4); RAG-MCP roughly tripled selection accuracy. Off-the-shelf retrievers are mediocre at finding tools (ToolRet), so retrieval quality, tool naming/namespacing and caching-safe insertion are the engineering levers that matter.

### Cited Findings

**Evidence that accuracy degrades with tool count**
- **LongFuncEval: Measuring the effectiveness of long context models for function calling** — IBM Research, 2025, arXiv [2505.10570](https://arxiv.org/abs/2505.10570). Performance dropped **7% to 85% as the number of tools increased**, **7% to 91%** in answer retrieval as tool-response length grew, and **13% to 40%** as multi-turn conversations got longer. The authors conclude that long-context models still struggle with tool calling. — [arXiv 2505.10570](https://arxiv.org/abs/2505.10570)
  - Harness note: tool count, tool-output length and conversation length are three separate degradation axes. Xylo should budget all three, not only tool count.
- **Microsoft Research, "Tool-space interference in the MCP era"** (Fourney, Payne, Murad, Amershi; Sept 11, 2025). Survey of **1,470 MCP servers**: most servers have **≤4 tools**, the largest has **256**, and 10 others have >100. GitHub MCP has **91 tools** and Playwright-MCP has **29**. **775 tools share names** with tools on other servers; `search` appears on **32 distinct servers**, `get_user` and `execute_query` on 11 each. OpenAI caps functions at **128** and suggests "fewer than 20" at a time. **226 servers** declare that their tool lists can change at any time. — [Microsoft Research blog](https://www.microsoft.com/en-us/research/blog/tool-space-interference-in-the-mcp-era-designing-for-agent-compatibility-at-scale/)
  - The same post recommends that client builders cache tool schemas, use them as an index for RAG-style tool selection, and **generate namespaces from server names** to resolve collisions. — [Microsoft Research blog](https://www.microsoft.com/en-us/research/blog/tool-space-interference-in-the-mcp-era-designing-for-agent-compatibility-at-scale/)
- **Anthropic, "Introducing advanced tool use"** (Nov 2025). Real-server definition costs: GitHub (35 tools, ~26K tokens), Slack (11 tools, ~21K), Jira (~17K). Five servers totaled ~55K tokens, and Anthropic has seen definitions reach **134K tokens** before optimization. — [Anthropic engineering](https://www.anthropic.com/engineering/advanced-tool-use)
- **MCP tool-description quality**: "Model Context Protocol (MCP) Tool Descriptions Are Smelly!" (Hasan et al., Feb 2026, arXiv [2602.14878](https://arxiv.org/abs/2602.14878)) builds a 6-component rubric for tool descriptions, defines "description smells", repairs them with an LLM and evaluates on MCP-Universe. Secondhand summaries report that **97.1%** of tools have at least one smell, a **median +5.85 pp** task-success gain from augmented descriptions, but **+67.46% more execution steps** and regressions in **16.67%** of cases. I could not confirm these numbers against the paper. — [arXiv 2602.14878](https://arxiv.org/abs/2602.14878) (numbers via [search summary](https://mcpplaygroundonline.com/blog/mcp-tool-description-quality), unverified)
- "From Docs to Descriptions: Smell-Aware Evaluation of MCP Server Descriptions" (arXiv [2602.18914](https://arxiv.org/abs/2602.18914)) reports **73%** of 10,831 servers have repeated tool names (snippet only). — [arXiv 2602.18914](https://arxiv.org/html/2602.18914v1)

**Fixes: deferred loading and tool search (production systems)**
- **Anthropic Tool Search Tool** (`defer_loading: true`, beta header `advanced-tool-use-2025-11-20`):
  - Tokens: the traditional approach used ~77K tokens before work began (~72K for 50+ MCP tools). With tool search it used ~8.7K (~500 upfront plus ~3K for 3–5 discovered tools), an **85% reduction** that preserves 191,300 vs 122,800 tokens of context.
  - Accuracy on MCP evals: **Opus 4 rose from 49% to 74%**, and **Opus 4.5 from 79.5% to 88.1%**.
  - Search: built-in **regex** and **BM25** variants, with custom search (for example embeddings) also supported. Whole MCP servers can be deferred, and Anthropic recommends keeping the **3–5 most-used tools always loaded**.
  - When to use: definitions >10K tokens, multiple MCP servers, or 10+ tools. It is less useful for <10 tools or compact definitions.
  - Caching: tool search **does not break prompt caching**, because deferred tools are excluded from the initial prompt and appended only after a search.
  - — [Anthropic engineering](https://www.anthropic.com/engineering/advanced-tool-use)
- **Claude Code** now defers MCP tool definitions by default: only names load at session start, and schemas load on lookup. Secondary sources describe an `ENABLE_TOOL_SEARCH` setting with `auto` (defer when MCP tool definitions exceed **10%** of the context window), a custom `auto:N` threshold, and `false`. These are secondary sources, not official docs. — [mcp.directory blog](https://mcp.directory/blog/mcp-context-bloat-fix-2026-tool-search-code-mode-progressive-disclosure); [DEV post (single-user measurement of a ~40K-token first-request drop)](https://dev.to/rulestack/tool-search-hid-40170-tokens-from-our-first-request-auto5-put-them-all-back-2jin)
- OpenAI's Codex CLI reportedly also defers MCP schemas when servers advertise tool search (v0.142.2, June 2026, per a secondary blog). I did not find the official Responses API `tool_search` documentation. — [codex.danielvaughan.com](https://codex.danielvaughan.com/2026/06/25/codex-cli-mcp-tool-search-default-livemcpbench-retrieval-bottleneck-scalable-discovery/)
- **Manus**, "Context Engineering for AI Agents" (Yichao 'Peak' Ji, July 18, 2025). Manus recommends against adding or removing tools mid-task. Tool definitions sit near the front of the context, so changing them invalidates the KV cache for everything after them. Removing tools also confuses the model when earlier turns reference tools that no longer exist, which causes schema violations and hallucinated actions. Instead, Manus keeps the tool list stable and **masks token logits** with a state machine. Its prefill modes are Auto, Required (prefill up to the tool-call token) and Specified (prefill up to the start of the function name). Consistent name prefixes (`browser_`, `shell_`) let it restrict whole groups. Cached input costs **$0.30/MTok vs $3/MTok** uncached on Claude Sonnet (10x), with a ~100:1 input:output ratio. — [Manus blog](https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus)
  - Conflict to note: Anthropic's approach is to *append* discovered tools after the cached prefix, which is cache-safe. Manus's approach is to *mask* a fixed list. Both agree that you should not rewrite the tool block at the top of the prompt mid-task.

**Fixes: tool retrieval research (all arXiv IDs verified)**
- **RAG-MCP: Mitigating Prompt Bloat in LLM Tool Selection via Retrieval-Augmented Generation** — Tiantian Gan et al., May 2025, arXiv [2505.03275](https://arxiv.org/abs/2505.03275). Semantic retrieval over an external index of MCP descriptions; only the top-k are given to the model. Results: **>50% prompt-token reduction**, and tool-selection accuracy **43.13% vs 13.62%** baseline on an MCP stress test. A critical review notes that retrieval precision falls sharply past roughly 100 candidate MCPs. — [arXiv](https://arxiv.org/abs/2505.03275); [review](https://pith.science/paper/2505.03275)
- **MCP-Zero: Active Tool Discovery for Autonomous LLM Agents** — Xiang Fei et al., June 2025, arXiv [2506.01056](https://arxiv.org/abs/2506.01056). The model itself emits a structured *tool request* (server and tool intent) when it notices a capability gap, and a hierarchical retriever (server, then tool) answers it. The corpus is **308 MCP servers / 2,797 tools (~248.1K tokens)**, with a **98% token reduction on APIBank**. A reviewer notes that the only retrieval baseline is the authors' own query retrieval (65–72%). — [arXiv](https://arxiv.org/abs/2506.01056); [review](https://pith.science/paper/2506.01056)
- **Retrieval Models Aren't Tool-Savvy: Benchmarking Tool Retrieval for LLMs (ToolRet)** — Zhengliang Shi et al., Mar 2025, arXiv [2503.01763](https://arxiv.org/abs/2503.01763) (ACL 2025 Findings, venue not re-verified). It has ~7.6K retrieval tasks over a ~43K-tool corpus; a secondary listing says 7,961 queries / 44,453 tools, probably a version difference. Strong IR models score poorly; one listing gives NV-Embed-v1 **42.71 NDCG@10**. The paper also releases >200K training instances. — [arXiv](https://arxiv.org/abs/2503.01763); [search summary](https://github.com/rafae-gill/mcp-tool-rag)
- **Toolshed: Scale Tool-Equipped Agents with Advanced RAG-Tool Fusion and Tool Knowledge Bases** — Elias Lumer et al. (PwC), Oct 2024, arXiv [2410.14594](https://arxiv.org/abs/2410.14594). Tool documents are enriched with synthetic questions, key topics and parameters, and retrieval uses pre-retrieval enrichment, intra-retrieval query decomposition/rewriting and post-retrieval reranking. **Absolute Recall@5 gains of 46%, 56% and 47%** on ToolE single-tool, ToolE multi-tool and Seal-Tools. — [arXiv](https://arxiv.org/abs/2410.14594)
- **Re-Invoke: Tool Invocation Rewriting for Zero-Shot Tool Retrieval** — Yanfei Chen et al. (Google), Aug 2024, arXiv [2408.01875](https://arxiv.org/abs/2408.01875). Unsupervised: at index time it generates synthetic queries per tool, and at query time an LLM extracts intents and ranks multi-view similarity. **+20% relative nDCG@5 (single-tool) and +39% (multi-tool)** on ToolE. — [arXiv](https://arxiv.org/abs/2408.01875)
- **AnyTool: Self-Reflective, Hierarchical Agents for Large-Scale API Calls** — Yu Du et al., Feb 2024, arXiv [2402.04253](https://arxiv.org/abs/2402.04253). Hierarchical retrieval (category, then tool, then API) over 16K+ APIs, with self-reflection when a solution fails. **+35.4% average pass rate over ToolLLM** on ToolBench. — [arXiv](https://arxiv.org/abs/2402.04253)
- **ToolLLM** — Yujia Qin et al. (Tsinghua et al.), July 2023, arXiv [2307.16789](https://arxiv.org/abs/2307.16789) (ICLR 2024). ToolBench has **16,464 RESTful APIs in 49 categories** from RapidAPI. The paper introduced a neural API retriever and DFSDT (depth-first decision-tree search). — [arXiv](https://arxiv.org/abs/2307.16789)
- **Gorilla: LLM Connected with Massive APIs** — Shishir Patil et al. (UC Berkeley), May 2023, arXiv [2305.15334](https://arxiv.org/abs/2305.15334) (NeurIPS 2024). APIBench spans HuggingFace, TorchHub and TensorHub, with retriever-aware training so the model can use retrieved docs at test time. This is fine-tuning, but it established that retrieved documentation lets the model adapt to API changes. — [arXiv](https://arxiv.org/abs/2305.15334)
- **ScaleMCP: Dynamic and Auto-Synchronizing MCP Tools for LLM Agents** — Elias Lumer et al., May 2025, arXiv [2505.06416](https://arxiv.org/abs/2505.06416). The agent gets a retrieval tool it can call to add MCP tools to its own memory, and the tool index auto-syncs with MCP servers (CRUD). It was tested on **5,000 financial-metric MCP servers**, 10 LLMs, 5 embedding models and 5 retriever types. — [arXiv](https://arxiv.org/abs/2505.06416)
- **TheMCPCompany: Creating General-purpose Agents with Task-specific Tools** — Reza Esfandiarpoor et al., Oct 2025, arXiv [2510.19286](https://arxiv.org/abs/2510.19286). A benchmark with **18,000+ tools** that require retrieval-based tool discovery. — [arXiv](https://arxiv.org/abs/2510.19286)
- **LiveMCPBench: Can Agents Navigate an Ocean of MCP Tools?** — arXiv [2508.01780](https://arxiv.org/abs/2508.01780). A large-toolset MCP benchmark; I did not extract its numbers. — [arXiv](https://arxiv.org/pdf/2508.01780)
- **Try, Check and Retry: A Divide-and-Conquer Framework for Boosting Long-context Tool-Calling Performance** — Kunfeng Chen et al., Mar 2026, arXiv [2603.11495](https://arxiv.org/abs/2603.11495). It splits a large tool set into groups, tries each, checks, then retries. **Up to +25.10% average gain** on BFCL and ACEBench. I did not confirm whether the reported gain needs training. — [arXiv](https://arxiv.org/abs/2603.11495)

**Small/edge models (especially relevant to local Ollama/llama.cpp)**
- **TinyAgent: Function Calling at the Edge** — Lutfi Eren Erdogan et al. (UC Berkeley), Sept 2024, arXiv [2409.00608](https://arxiv.org/abs/2409.00608). Uses a "Tool RAG" classifier so a 1.1B/7B on-device model sees only the relevant tools, which significantly cuts prompt length. It combines this with LLMCompiler-style planning. The models are fine-tuned, but the Tool RAG step is harness-level. — [arXiv](https://arxiv.org/abs/2409.00608); [search summary](https://liner.com/review/tinyagent-function-calling-at-edge)
- **Less is More: Optimizing Function Calling for LLM Execution on Edge Devices** — Paramanayakam et al., Nov 2024, arXiv [2411.15399](https://arxiv.org/abs/2411.15399). Fine-tuning-free: the LLM first names the tools it thinks it needs, then a controller supplies only the most relevant tools before the real call. A secondary summary attributes **up to 70% lower execution time and up to 40% lower power** to it; I did not confirm these. — [arXiv](https://arxiv.org/abs/2411.15399); [review](https://themoonlight.io/review/less-is-more-optimizing-function-calling-for-llm-execution-on-edge-devices)

**Selection is manipulable by descriptions (security-relevant for third-party MCP)**
- **Tool Preferences in Agentic LLMs are Unreliable** — Kazem Faghih et al., May 2025, arXiv [2505.18135](https://arxiv.org/abs/2505.18135). Editing only a tool's description (for example with assertive cues) produced **>10x more usage** for GPT-4.1 and Qwen2.5-7B; 17 models were tested. — [arXiv](https://arxiv.org/abs/2505.18135)
- **ToolTweak: An Attack on Tool Selection in LLM-based Agents** — Jonathan Sneh et al., Oct 2025, arXiv [2510.02554](https://arxiv.org/abs/2510.02554). Adversarially optimized names and descriptions raised selection from **~20% to as high as 81%**. — [arXiv](https://arxiv.org/abs/2510.02554)

### Inferences
- For Xylo, the default should be: always load built-in core tools plus 3–5 pinned or most-used tools, and defer everything else behind a built-in `tool_search` tool. Turn deferral on automatically once definitions pass ~10K tokens or ~10% of the model's context, whichever is smaller. Local models with 8K–32K windows will hit this almost immediately. In Microsoft's survey, 56 of the measured MCP tools return enough output in a single call to overflow a 32K window (Qwen3), and 93 overflow a 16K window (Phi-4), before any tool definitions are counted.
- Implement retrieval in two layers. (a) BM25 over `server name + tool name + description + parameter names`. This is cheap and runs in pure Rust (e.g. `tantivy`), and Anthropic ships BM25 and regex variants. (b) Optionally, a local embedding model, with Toolshed/Re-Invoke-style index-time enrichment: generate 3–5 synthetic user queries per tool once, when an MCP server connects, and cache them alongside the schema hash. ToolRet shows that generic embedders alone are weak, so the enrichment is where the gains come from.
- Namespace every MCP tool as `<server>__<tool>`, with a deterministic, sanitized server slug. Keep the original name for display and approval prompts. Detect collisions at registration, since 775 collisions were observed in the wild.
- Keep caching safe. Never reorder or rewrite the tool block mid-session. Append newly discovered schemas after the cached prefix, as Anthropic's deferred tools do. For llama.cpp, where KV prefix reuse matters a lot on slow local hardware, keep the tool list stable and use grammar/prefill restriction (Manus "mask, don't remove") rather than editing the prompt.
- For weak local models, use an explicit two-stage "select, then fill" loop (the Less-is-More, NLT and TinyAgent pattern):
  - Stage 1: show only names and one-line descriptions, and ask the model which tools it needs (or none).
  - Stage 2: show the full schemas of 1–5 chosen tools and constrain the argument JSON.
- Treat third-party MCP descriptions as untrusted input that can bias selection (ToolTweak, Faghih et al.). Show users the effective description, consider normalizing descriptions (strip superlatives and "always use this tool" instructions), and keep built-in tools ranked first for overlapping capabilities.

### Gaps
- No head-to-head study compares Anthropic's BM25/regex tool search, RAG-MCP, MCP-Zero and embedding retrieval on the same MCP corpus. The numbers come from different setups and should not be compared directly.
- I found no peer-reviewed curve of accuracy vs. tool count specifically for small local models (≤14B) under Ollama/llama.cpp chat templates. LongFuncEval covers long-context models in general.
- I could not confirm the "MCP descriptions are smelly" numbers or the "Less is More" 70%/40% numbers against the papers themselves.
- I did not find official OpenAI documentation for a Responses API `tool_search` feature, only a secondary blog about Codex CLI.

---

## Q2. Tool interface / description design (ACI): does tool design change performance?

### Takeaway
Yes, and by large margins. In SWE-agent, interface choices alone moved SWE-bench Lite from 11.0% (shell only) to 18.0%. Individual ACI choices were worth 2–8 points each: linting guardrails, a 100-line file window, summarized search, and pruning old observations. Anthropic reports that tool-description refinements contributed to a SOTA SWE-bench Verified result, and that adding usage examples raised complex-parameter accuracy from 72% to 90%. Concise, unambiguous, consolidated tools with readable identifiers and actionable errors are the consistent recommendation.

### Cited Findings
- **SWE-agent: Agent-Computer Interfaces Enable Automated Software Engineering** — John Yang, Carlos Jimenez et al. (Princeton), 2024, arXiv [2405.15793](https://arxiv.org/abs/2405.15793) (NeurIPS 2024). — [arXiv HTML](https://arxiv.org/html/2405.15793)
  - Headline: **12.47%** on full SWE-bench with GPT-4 Turbo (286 of 2,294; the extraction tool garbled this to "286 of 294"), vs **1.31%** for the prior RAG baseline. HumanEvalFix pass@1 is ~88%.
  - ACI ablations on SWE-bench Lite (GPT-4 Turbo), default = **18.0%**:
    - **Shell-only baseline: 11.0%**, or 7.33% without a demonstration.
    - Edit **without linting: 15.0**; **no edit command: 10.3**.
    - Search: **iterative 12.0**; **no search 15.7**; summarized (default) 18.0.
    - File viewer: **30-line window 14.3**; **full file 12.7**; 100-line window (default) 18.0.
    - Context: **full history 15.0** vs last-5-observations (default) 18.0; **no demonstration 16.3**.
  - Four ACI principles: (1) keep actions simple, with few options and concise docs; (2) keep actions compact, consolidating common operations; (3) make feedback informative but concise; (4) build in guardrails, such as a linter that rejects syntactically invalid edits.
- **Anthropic, "Writing effective tools for agents — with agents"** (2025). — [Anthropic engineering](https://www.anthropic.com/engineering/writing-tools-for-agents)
  - **Namespacing**: prefix-vs-suffix naming had "non-trivial effects" on tool-use evals, and the best choice varies by model.
  - **Identifiers**: resolving UUIDs to natural-language names, or using 0-indexed IDs, "significantly improves Claude's precision in retrieval tasks" by reducing hallucination.
  - **`response_format` enum**: a detailed Slack response cost **206 tokens** vs **72 tokens** concise (~1/3).
  - Claude Code caps tool responses at **25,000 tokens** by default.
  - **Errors** should be actionable and show correctly formatted inputs, not tracebacks.
  - Description refinements helped Claude Sonnet 3.5 reach **SOTA on SWE-bench Verified**. Fixing a search tool's description stopped Claude from appending "2025" to queries.
  - Use unambiguous parameter names (`user_id`, not `user`).
  - **Consolidate**: `schedule_event` instead of list_users + list_events + create_event, `search_logs` instead of `read_logs`, `get_customer_context` instead of three getters.
  - Track accuracy, runtime, number of tool calls, tokens and tool errors in evals. Many redundant calls suggest pagination problems; frequent invalid-parameter errors suggest the descriptions need examples.
  - Claude-optimized tools beat expert-written Slack/Asana MCP servers on held-out tests (figures shown only in charts).
- **Anthropic Tool Use Examples** (`input_examples`, 1–5 realistic examples per tool): complex-parameter-handling accuracy improved from **72% to 90%** in internal testing. Recommended for nested structures, many optional parameters and domain conventions; not for simple tools. — [Anthropic engineering](https://www.anthropic.com/engineering/advanced-tool-use)
- **Microsoft MCP survey**: across **12,643 tools**, mean max schema depth is **2.24** and the maximum is **20 levels**. The post cites Composio's finding that **flattening parameters improved tool-calling performance by 47%** (secondhand). It also cites Anthropic's report that a tool-testing agent rewriting defective MCP tool descriptions cut task completion time by **40%**. — [Microsoft Research blog](https://www.microsoft.com/en-us/research/blog/tool-space-interference-in-the-mcp-era-designing-for-agent-compatibility-at-scale/)
- **EASYTOOL: Enhancing LLM-based Agents with Concise Tool Instruction** — Siyu Yuan et al. (Fudan / Microsoft Research Asia), Jan 2024, arXiv [2401.06201](https://arxiv.org/abs/2401.06201) (NAACL 2025; venue not re-verified). It converts diverse, lengthy tool documentation into a unified, concise "tool instruction" (description, parameters, examples). The abstract reports reduced token consumption and better tool use but gives no numbers. — [arXiv](https://arxiv.org/abs/2401.06201)
- **Tool Documentation Enables Zero-Shot Tool-Usage with LLMs** — Cheng-Yu Hsieh et al., Aug 2023, arXiv [2308.00675](https://arxiv.org/abs/2308.00675). Across 6 tasks, documentation alone (zero-shot) performs on par with few-shot demonstrations. With docs, LLMs composed vision tools (GroundingDINO, SAM, XMem, Stable Diffusion) to re-create Grounded-SAM and Track Anything. — [arXiv](https://arxiv.org/abs/2308.00675)
- **PLAY2PROMPT: Zero-shot Tool Instruction Optimization for LLM Agents via Tool Play** — Wei Fang et al., Mar 2025, arXiv [2503.14432](https://arxiv.org/abs/2503.14432). The model "plays" with each tool by trial and error to iteratively refine its documentation and generate usage examples, with no labeled data. The abstract gives no numbers. — [arXiv](https://arxiv.org/abs/2503.14432)
- **DRAFT: From Exploration to Mastery: Enabling LLMs to Master Tools via Self-Driven Interactions** — Changle Qu et al., Oct 2024, arXiv [2410.08197](https://arxiv.org/abs/2410.08197) (ICLR 2025; venue not re-verified). An explore-analyze-rewrite loop revises tool docs based on feedback from real calls. The abstract gives no numbers. — [arXiv](https://arxiv.org/abs/2410.08197)
- **Anthropic, "Building effective agents"** (Dec 2024; not re-fetched this session). For their SWE-bench agent, Anthropic says they spent more time optimizing tools than the overall prompt. For example, switching to absolute file paths eliminated a class of relative-path errors ("poka-yoke" your tools). — [Anthropic](https://www.anthropic.com/engineering/building-effective-agents)

### Inferences
- Xylo's built-in tools should follow the SWE-agent and Anthropic principles directly:
  - Windowed file viewing (~100 lines with line numbers and a "N more lines" footer).
  - A search tool that returns a *summary* (match counts per file, then lines on request) rather than raw dumps.
  - Edit tools that run a fast syntax check (tree-sitter parse in Rust) and **reject** edits that introduce parse errors, with a clear message.
  - Absolute paths everywhere.
  - Human-readable identifiers in outputs, with any UUID mapping kept harness-side.
- Add a harness-side "description rewriter" pass for MCP tools: on first connect, an LLM (cloud, or a local model offline) rewrites smelly descriptions into an EASYTOOL-style concise form, plus 1–3 `input_examples`. Cache the result by schema hash, keep the original for audit, and A/B both on a small eval set. Note the smelly-MCP paper's warning that augmented descriptions raised step counts (~+67%) and regressed ~17% of cases, so this needs evaluation per model.
- Flatten deeply nested MCP schemas when presenting them to weak models, for example by turning dotted keys into flat parameters, then re-nest before dispatch. This is a direct application of the reported +47% flattening effect.
- Use the eval metrics Anthropic recommends (tool-error rate, redundant calls, tokens) as Xylo telemetry, so tool and description regressions are visible per model.

### Gaps
- EASYTOOL, PLAY2PROMPT and DRAFT give no quantitative results in their abstracts, and I did not extract them from the full papers in this session.
- The Composio "+47% from flattening" figure is secondhand (cited by Microsoft). I did not find the primary measurement.
- Anthropic's held-out Slack/Asana tool comparison is shown only in charts, with no numbers in the text.

---

## Q3. Tool output handling: truncate, summarize, or offload large results; return handles instead of raw data

### Takeaway
Tool outputs are heavy-tailed. Across 1,312 real MCP tools the median output was 98 tokens but the mean was 4,431, and some tools averaged more than 500K tokens. Long outputs measurably degrade accuracy, by up to 91% in LongFuncEval. Best practice is to cap per-call output (Claude Code uses 25K tokens), paginate or window, offer concise vs. detailed formats, and offload bulky data to files or a sandbox with a reference left in context. Code execution lets filtering happen before data reaches the model at all.

### Cited Findings
- **Microsoft MCP survey**: **2,443** successful calls across **1,312** unique tools. **Median output 98 tokens, mean 4,431**, and the largest tool averaged **557,766 tokens**; **16 tools averaged >128K tokens**.
  - Tools that would overflow the context window after one call (and after 6–10 calls): GPT-4.1 (1M) 0 (11); GPT-5 (400K) 1 (25); GPT-4o / Llama 3.1 (128K) 16 (40); **Qwen 3 (32K) 56 (90); Phi-4 (16K) 93 (109)**.
  - Only **4 tools** returned a `resource_link`, so servers almost never return handles on their own.
  - The post recommends summarizing or paginating long results.
  - — [Microsoft Research blog](https://www.microsoft.com/en-us/research/blog/tool-space-interference-in-the-mcp-era-designing-for-agent-compatibility-at-scale/)
- **LongFuncEval**: **7–91%** degradation in answer retrieval as tool-response length grows. — [arXiv 2505.10570](https://arxiv.org/abs/2505.10570)
- **SWE-agent**: showing the **full file** instead of a 100-line window dropped Lite from **18.0 to 12.7**. **Summarized search** beat iterative search (**18.0 vs 12.0**). Collapsing old observations (keeping the last 5) beat full history (**18.0 vs 15.0**). — [arXiv 2405.15793](https://arxiv.org/html/2405.15793)
- **Anthropic tool design**: a **25,000-token** default cap in Claude Code, plus pagination, range selection, filtering and truncation with sensible defaults. When truncating, add steering text, for example telling the agent to prefer many targeted searches over one broad one. `response_format` concise (72 tokens) vs detailed (206 tokens). — [Anthropic engineering](https://www.anthropic.com/engineering/writing-tools-for-agents)
- **Manus**: use the file system as unlimited external context, and make compression **restorable**. A web page's content can be dropped "as long as the URL is preserved", and a document's content can be dropped if its sandbox path remains. — [Manus blog](https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus)
- **Anthropic code execution with MCP**: intermediate results stay in the execution environment, and the agent sees **5 rows instead of 10,000**. Intermediate state can be written to files, and the MCP client can tokenize PII so raw values never enter the model's context. — [Anthropic engineering](https://www.anthropic.com/engineering/code-execution-with-mcp)
- **Anthropic Programmatic Tool Calling**: a 20-employee expense example with 2,000+ line items (50KB+) yields ~1KB in context. The article elsewhere cites "200KB", an internal inconsistency. Average tokens on complex research tasks fell from **43,588 to 27,297 (−37%)**. — [Anthropic engineering](https://www.anthropic.com/engineering/advanced-tool-use)
- **Anthropic context editing** (Sept 2025; not re-fetched this session): automatic clearing of stale tool results. Reported a **84%** token reduction in a 100-turn web-search eval, and a **39%** improvement when combined with a memory tool. — [Anthropic news](https://www.anthropic.com/news/context-management)

### Inferences
- Implement a single **ToolResultPolicy** in Xylo's dispatcher for every tool (built-in, MCP, plugin):
  1. Count tokens with the target model's tokenizer, or an estimate.
  2. If the result is over budget, set budget = min(25K, ~10% of the model's context; for 8K–32K local models aim for ~1–3K tokens). Write the full result to `sandbox/tool_outputs/<call_id>.<ext>`.
  3. Return a head/tail preview, total size and line count, the file path, and explicit instructions for paging with `read_file(offset, limit)` / `grep`. This is the Manus restorable-compression pattern, implemented harness-side because servers rarely emit `resource_link`.
  4. For JSON, return the schema shape, row count and first N items rather than a raw byte cut, so JSON is never cut mid-structure.
- Add a `response_format: concise|detailed` convention to built-in tools, and default to concise for local models.
- Old tool results can be elided to a stub such as "[result of call X stored at path, 12,431 tokens]" after K turns. This is the SWE-agent "last 5 observations" ablation, and it also matches Anthropic's tool-result clearing. Do it at a cache breakpoint boundary to limit cache invalidation.
- Mark tool results with `isError` reliably (see Q7). Error text must never be truncated away.

### Gaps
- I found no controlled study comparing "truncate" vs "LLM-summarize" vs "offload-to-file with handle" on the same benchmark. The evidence is per-system (SWE-agent ablations, vendor blogs).
- The Anthropic context-editing numbers were not re-fetched this session.

---

## Q4. Code as action (CodeAct, code execution with MCP, Code Mode, smolagents)

### Takeaway
Having the model write code that calls tools, instead of emitting one JSON call per turn, consistently reduces round-trips and tokens. The token effect is dramatic when intermediate data is large: Anthropic reports 150K to 2K tokens (−98.7%) in one workflow and −37% on average for Programmatic Tool Calling. CodeAct reported up to 20% higher success across 17 LLMs. Accuracy gains on frontier models are modest (+3–5 pp in Anthropic's internal benchmarks). It requires a secure sandbox, and the evidence for small local models is thin.

### Cited Findings
- **Executable Code Actions Elicit Better LLM Agents (CodeAct)** — Xingyao Wang et al. (UIUC; with Apple), Feb 2024, arXiv [2402.01030](https://arxiv.org/abs/2402.01030) (ICML 2024). Executable Python is the unified action space, run multi-turn with an interpreter that feeds back results and errors. Across **17 LLMs** on API-Bank and the new M3ToolEval, CodeAct achieved **up to 20% higher success rate** than JSON/text action formats. It also released CodeActInstruct (7K multi-turn trajectories), which is fine-tuning data and out of scope. — [arXiv](https://arxiv.org/abs/2402.01030)
- **Anthropic, "Code execution with MCP: Building more efficient agents"** (Nov 4, 2025). MCP servers are presented as a filesystem of code modules (`./servers/<server>/<tool>.ts`). The agent lists directories or calls `search_tools` (with detail levels) to load only what it needs, then writes code. Token usage fell from **150,000 to 2,000 (−98.7%)** in the example workflow. Listed benefits: progressive disclosure, filtering in the sandbox, loops and conditionals as code, privacy (PII tokenization), state persistence, and saving reusable code as Skills. Caveat: it needs sandboxing, resource limits and monitoring, which add operational overhead. — [Anthropic engineering](https://www.anthropic.com/engineering/code-execution-with-mcp)
- **Anthropic Programmatic Tool Calling** (`code_execution_20250825` plus `allowed_callers` on tools):
  - Average tokens fell **43,588 to 27,297 (−37%)**.
  - Internal knowledge retrieval rose **25.6% to 28.5%**, and GIA benchmarks **46.5% to 51.2%**.
  - A 20+ call workflow eliminates 19+ inference passes; no wall-clock latency was reported.
  - Use it for 3+ dependent calls, large data where only aggregates matter, or parallel fan-out. Avoid it for single calls or when the model must reason over every intermediate result.
  - Document tool *return* formats, because the model writes parsing code against them.
  - — [Anthropic engineering](https://www.anthropic.com/engineering/advanced-tool-use)
- **Cloudflare, "Code Mode: the better way to use MCP"** (Kenton Varda & Sunil Pai, Sept 26, 2025).
  - Approach: the MCP schema is converted to a TypeScript API with doc comments, and the agent gets one "execute TypeScript" tool. Code runs in a fresh **V8 isolate** per execution (Worker Loader API) that starts in milliseconds with a few MB of memory.
  - Security: no `fetch()` or `connect()`; MCP access only through **bindings**; the supervisor injects API keys, so the code never sees credentials.
  - Rationale: LLMs have seen far more real TypeScript than tool-call tokens, which are often synthetic, and chaining calls in code avoids copying outputs through the LLM.
  - **No accuracy or token benchmarks in the post.**
  - — [Cloudflare blog](https://blog.cloudflare.com/code-mode/)
- **Hugging Face smolagents** (Dec 2024; not re-fetched this session). The `CodeAgent` writes Python actions and is the library's recommended default. The launch post cites CodeAct-style evidence that code actions use about 30% fewer steps (LLM calls) than JSON tool calls and perform better on hard benchmarks. — [HF blog](https://huggingface.co/blog/smolagents)
- **ReWOO** and **LLMCompiler** (see Q5) are earlier "plan the whole program first" variants of the same idea, using plans or DAGs instead of general code.

### Inferences
- Xylo already has a sandbox, so it can offer a "code mode" tool for capable models:
  - Generate a typed stub module per connected MCP server (TypeScript or Python, from the cached schemas, with doc comments from the possibly rewritten descriptions).
  - Expose one `run_code` tool. Calls from inside the sandbox go back through the harness dispatcher (Cloudflare's bindings model), so **approval policy, auditing and secrets injection still apply per inner call**.
  - Inner calls to tools flagged destructive must still trigger user approval. Batch-approve by showing the planned script, or pause on each risky call.
- Make code mode opt-in per model or task type. Enable it for strong cloud models and data-heavy or many-call tasks such as spreadsheets, logs and bulk operations. Keep direct JSON tool calls as the default for small local models and simple single-call turns, which is consistent with Anthropic's "avoid for single invocations" guidance.
- Return only `print`/`console.log` output, and apply the Q3 output policy to it as well.
- Code mode also improves privacy, because raw data stays in the sandbox and can be tokenized. This fits a desktop app handling local files.

### Gaps
- I found no independent, peer-reviewed benchmark of Cloudflare Code Mode or Anthropic's MCP-as-code approach vs native tool calling across several models. The figures are vendor-internal.
- I found no rigorous evidence on whether **small local models (≤14B)** benefit from code-as-action vs JSON tool calls. Their code reliability may erase the gains. This is a key open question for Xylo's local path.
- I did not re-verify the smolagents "30% fewer steps" claim, or CodeAct's turn-reduction figure (not in the abstract).

---

## Q5. Parallel and efficient execution (parallel calls, DAG planning, plan-without-observation, async/speculative execution)

### Takeaway
Serial "think, call one tool, wait, think" loops are the main latency bottleneck. The following all report substantial speedups without retraining: planning independent calls as a DAG and executing them in parallel (LLMCompiler: up to 3.7x faster, 6.7x cheaper, ~9% more accurate than ReAct), decoupling planning from observations (ReWOO: 5x fewer tokens), asynchronous function calling (1.6–5.4x lower latency), and a 2026 wave of speculative tool execution (PASTE: −48.5% tool time; TomasuLLM: ~1.3x on SWE-bench Verified and Terminal-Bench). Speculation is only safe for read-only or idempotent tools. Prefix/KV-cache stability is the largest cost lever (10x).

### Cited Findings
- **An LLM Compiler for Parallel Function Calling (LLMCompiler)** — Sehoon Kim et al. (UC Berkeley), Dec 2023, arXiv [2312.04511](https://arxiv.org/abs/2312.04511) (ICML 2024). A Function-Calling Planner emits a DAG of tasks with dependencies, a Task-Fetching Unit dispatches ready tasks, and an Executor runs them in parallel. Vs ReAct: **latency speedup up to 3.7x, cost savings up to 6.7x, accuracy up to ~9% higher**. It also reports up to **1.35x** latency gain over OpenAI's native parallel function calling; this figure was not in the abstract fetch and is from memory. — [arXiv](https://arxiv.org/abs/2312.04511)
- **ReWOO: Decoupling Reasoning from Observations** — Binfeng Xu et al., May 2023, arXiv [2305.18323](https://arxiv.org/abs/2305.18323). A Planner writes the full plan with placeholders (#E1, #E2), Workers fill them, and a Solver answers. **5x token efficiency and +4% accuracy on HotpotQA**, more robust when tools fail, and reasoning can be offloaded from a 175B model to a 7B LLaMA. — [arXiv](https://arxiv.org/abs/2305.18323)
- **Asynchronous LLM Function Calling (AsyncLM)** — In Gim et al., Dec 2024, arXiv [2412.07017](https://arxiv.org/abs/2412.07017). The LLM keeps generating while function calls execute, and an interrupt mechanism injects results when they finish. End-to-end latency is **1.6x–5.4x lower** than synchronous calling on BFCL tasks. — [arXiv](https://arxiv.org/abs/2412.07017)
- **Act While Thinking / PASTE: Pattern-Aware Speculative Tool Execution** — arXiv [2603.18897](https://arxiv.org/abs/2603.18897) (2026). Middleware predicts likely next tool calls from recurring patterns and runs them while the LLM is still generating. **−48.5% tool execution time, 1.8x tool throughput** (snippet only). — [arXiv HTML](https://arxiv.org/html/2603.18897v1)
- **TomasuLLM: Out-of-Order Speculative Execution for LLM Agents** — arXiv [2609.38201](https://arxiv.org/abs/2609.38201) (2026). Runs tool calls out of trajectory order while preserving correctness. **1.31x on SWE-bench Verified, 1.35x on Terminal-Bench 2.0**, 1.27x matched progress on SWE-Marathon (snippet only). — [arXiv](https://arxiv.org/abs/2609.38201)
- Other 2026 speculative-execution work (snippet only, not read in full):
  - **Speculate with Memory**, arXiv [2607.12236](https://arxiv.org/abs/2607.12236): speculator accuracy rises from ~28% to >50% with accumulated experience.
  - **AOSpec**, arXiv [2608.00881](https://arxiv.org/abs/2608.00881): co-speculates actions and observations for stateful tools, and notes that sandboxes make speculating on side-effecting tools feasible.
  - **Speculative Interaction Agents**, arXiv [2605.13360](https://arxiv.org/abs/2605.13360): launches calls from partial user input.
  - **Dynamic Speculative Agent Planning**, arXiv [2509.01920](https://arxiv.org/abs/2509.01920).
  - — [search results](https://arxiv.org/abs/2609.38201)
- **Anthropic Programmatic Tool Calling**: tools that "can run in parallel" and "operations that are safe to retry" are the recommended candidates. 20+ calls in one code block avoid 19+ inference passes. — [Anthropic engineering](https://www.anthropic.com/engineering/advanced-tool-use)
- **KV/prompt caching** as an efficiency lever: Manus treats KV-cache hit rate as the single most important production metric. Cached tokens cost **10x less** ($0.30 vs $3/MTok on Sonnet). Keep prefixes stable (no timestamps at the top), keep context append-only, and serialize JSON deterministically. — [Manus blog](https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus)

### Inferences
- Xylo's dispatcher should execute all tool calls emitted in one assistant turn **concurrently** (tokio `join_all`). The exceptions:
  - Calls that touch the same resource with writes run serially.
  - Calls that need approval are batched into **one approval prompt** listing all pending risky calls, rather than N sequential modal prompts.
  - Results are returned in the original call order.
- Use MCP tool annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint` in the MCP spec; not re-verified this session) together with Xylo's own built-in tool metadata to decide what can be parallelized, speculated or auto-approved. Unknown means treat as destructive.
- Speculation in Xylo should be limited to read-only, idempotent, local, cheap tools: file reads, directory listings, grep, `git status`, and MCP tools explicitly marked read-only. Results are discarded if unused. Never speculate a tool that requires approval or has side effects outside the sandbox.
- For local models, where each extra LLM turn costs tens of seconds (Docker measured ~84–142 s per interaction for Qwen3 8B/14B on an M4 Max), ReWOO/LLMCompiler-style "plan all calls, then execute" saves the most wall-clock time. Have the model emit a plan as a list of calls with `$ref` placeholders, then execute without returning to the LLM between calls.
- Deterministic JSON serialization (sorted keys) of tool definitions and results in Rust (`serde_json` with ordered maps) keeps prefix-cache hits for both cloud prompt caching and llama.cpp prompt-cache reuse.

### Gaps
- Most 2026 speculative-execution papers were seen only via snippets; their methods and safety assumptions need full reading before adoption.
- No source measured parallel execution combined with interactive per-call user approval, which is Xylo's specific situation.

---

## Q6. Function calling for small/local models: benchmarks, prompt-based calling, constrained decoding, JSON repair, text formats vs native

### Takeaway
Small open models now call tools reasonably well on simple tasks. In Docker's test, Qwen3-14B Q4 scored 0.971 F1 vs GPT-4's 0.974, and quantization barely mattered. They still degrade sharply with more tools, longer contexts and multi-step chains, and they show eager invocation, wrong-tool selection and malformed arguments. The most promising harness-level levers:
- Let the model reason in free text, then constrain only the tool-call span (lazy grammars, or draft-then-constrain). Hard schema constraints can sharply cut answer accuracy in small models (the "Constraint Tax": 19.7% to 11.0%).
- Separate tool *selection* from argument generation. Natural Language Tools reported +18.4 pp with this approach.
- Present few tools at a time.
- Validate and repair JSON, with error feedback to the model.

### Cited Findings

**Benchmarks (what to evaluate Xylo against)**
- **Berkeley Function Calling Leaderboard (BFCL)** — Patil et al. (UC Berkeley Gorilla). The paper "The Berkeley Function Calling Leaderboard (BFCL): From Tool Use to Agentic Evaluation of LLMs" was presented at ICML 2025.
  - Categories: simple, multiple, parallel, parallel-multiple, irrelevance/relevance detection, multi-turn (v3), and agentic web search and memory (v4). It uses AST-based checking.
  - It scores models in both **native "FC" mode and "Prompt" mode** (tools described in the prompt and parsed from text), which is directly relevant for models without native tool support.
  - Leaderboard structure is from memory; not re-fetched this session.
  - — [BFCL leaderboard](https://gorilla.cs.berkeley.edu/leaderboard.html); [ICML talk listing](https://slideslive.com/39040425/the-berkeley-function-calling-leaderboard-bfcl-from-tool-use-to-agentic-evaluation-of-large-language-models)
- **τ-bench** — Shunyu Yao et al. (Sierra), June 2024, arXiv [2406.12045](https://arxiv.org/abs/2406.12045). Tool-agent-user conversations with domain policies. SOTA agents succeeded on **<50%** of tasks, and **pass^8 was <25% in retail**. It introduced the pass^k reliability metric. — [arXiv](https://arxiv.org/abs/2406.12045)
- **τ²-Bench: Conversational Agents in a Dual-Control Environment** — Victor Barres et al. (Sierra), June 2025, arXiv [2506.07982](https://arxiv.org/abs/2506.07982). The user also has tools (telecom domain). — [arXiv](https://arxiv.org/abs/2506.07982)
- **NESTFUL: Nested Sequences of API Calls** — Kinjal Basu et al. (IBM), Sept 2024, arXiv [2409.03797](https://arxiv.org/abs/2409.03797). 1,800+ nested sequences; the best model (GPT-4o) reached only **28% full-sequence match** and a 60% win rate. — [arXiv](https://arxiv.org/abs/2409.03797)
- **ACEBench** — Chen Chen et al., Jan 2025, arXiv [2501.12851](https://arxiv.org/abs/2501.12851). Normal, Special (ambiguous or flawed instructions) and Agent categories. — [arXiv](https://arxiv.org/abs/2501.12851)
- **ToolSandbox** — Jiarui Lu et al. (Apple), Aug 2024, arXiv [2408.04682](https://arxiv.org/abs/2408.04682). Stateful, conversational tool-use evaluation. — [arXiv](https://arxiv.org/abs/2408.04682)
- **MCP-native benchmarks** (paper-time results, since superseded by newer models):
  - **MCP-Bench** (Zhenting Wang et al., Aug 2025, arXiv [2508.20453](https://arxiv.org/abs/2508.20453)): 28 live MCP servers, 250 tools, 20 LLMs. — [arXiv](https://arxiv.org/abs/2508.20453)
  - **MCP-Universe** (Salesforce; arXiv [2508.14704](https://arxiv.org/abs/2508.14704); NeurIPS 2025): best was **GPT-5 at 43.72%**, then Grok-4 at 33.33%; the best open model was GLM-4.5 at 24.68%. — [arXiv](https://arxiv.org/pdf/2508.14704)
  - **MCPMark** (arXiv [2509.24002](https://arxiv.org/abs/2509.24002)): best was **gpt-5-medium at 52.56% pass@1 and 33.86% pass^4**; Claude Sonnet 4 and o3 were <30% pass@1. — [arXiv](https://arxiv.org/pdf/2509.24002)
  - **Toolathlon** (arXiv [2510.25726](https://arxiv.org/abs/2510.25726)): best was **Claude 4.5 Sonnet at 38.6%**; the best open model was DeepSeek-V3.2-Exp at 20.1%. — [arXiv](https://arxiv.org/pdf/2510.25726)
  - **MCP-Atlas** (arXiv [2602.00933](https://arxiv.org/abs/2602.00933)): 1,000 tasks; **Claude Opus 4.5 at 62.3%**. — [arXiv](https://arxiv.org/html/2602.00933v1)
- **NexusRaven-V2** (Nexusflow, Dec 2023; not re-fetched this session): an open 13B function-calling model reported to beat GPT-4 by up to 7% on human-generated nested and composite function-calling cases, and an early sign that small specialized open models can be competitive. — [Nexusflow blog](https://nexusflow.ai/blogs/ravenv2)

**Local model evidence**
- **Docker, "Tool Calling with Local LLMs: A Practical Evaluation"** (Ignasi Lopez Luna, June 30, 2025). 21 models, **3,570 test cases**, 5 simple shopping-cart tools, up to 5 rounds, run on an M4 Max with 128GB. — [Docker blog](https://www.docker.com/blog/local-llm-tool-calling-a-practical-evaluation/)
  - Tool-selection F1:
    - Hosted: gpt-4 **0.974**, claude-3-haiku 0.933, gpt-4o 0.857.
    - Qwen3: **qwen3:14B-Q4_K_M 0.971**, qwen3:8B-F16 0.933, qwen3:8B-Q4_K_M 0.919.
    - Llama: llama3.1:8B-F16 0.835, llama3.1:8B-Q4_K_M 0.793, llama3.2:3B 0.727, **llama3.3:70B-Q4_K_M 0.607**.
    - Others: qwen2.5:7B-Q4 0.753, gemma3:4B 0.733, llama-xlam:8B 0.570, watt-tool:8B 0.484.
  - Latency: ~**84 s** (qwen3 8B) and ~**142 s** (qwen3 14B) per interaction, vs <5 s for gpt-4 and 3.56 s for claude-3-haiku.
  - **No significant difference between quantized and non-quantized** variants.
  - Failure modes: **eager invocation** (calling tools on "Hi there!"), **wrong tool**, **invalid or missing arguments**, and **ignoring tool responses**.
- **Natural Language Tools (NLT)** — Reid T. Johnson, Michelle D. Pain, Jordan D. West, Oct 2025, arXiv [2510.14453](https://arxiv.org/abs/2510.14453).
  - Method: JSON tool calling is replaced with a separate natural-language selection step, where the model lists each tool with YES/NO, decoupled from response generation.
  - Results: **+18.4 pp tool-calling accuracy and −70% output variance** across 10 models and 6,400 trials (customer-service and mental-health domains). **Open-weight models gain most** (one summary cites +10.6 pp for weaker models). The gains persist under prompt perturbations, and the method enables tool calling for models without native support.
  - A 2026 replication (arXiv 2607.03953; 14 models, 8,560 trials; snippet only) reports significant improvement. A secondary write-up says the effect varies by model, with gains for Claude Sonnet 4 but losses for Gemini 2.5 Pro (unverified).
  - — [arXiv](https://arxiv.org/abs/2510.14453); [replication listing](https://www.alphaxiv.org/abs/2607.03953); [StackOne write-up](https://www.stackone.com/blog/natural-language-tools-vs-function-calling/)

**Format restriction and constrained decoding**
- **Let Me Speak Freely? A Study on the Impact of Format Restrictions on LLM Performance** — Zhi Rui Tam et al., Aug 2024, arXiv [2408.02442](https://arxiv.org/abs/2408.02442) (EMNLP 2024 Industry; venue not re-verified). Stricter format constraints (JSON mode, format instructions) degraded reasoning, while classification sometimes benefited. The NLT paper cites a **27.3 pp GSM8K drop** when JSON output was required. — [arXiv](https://arxiv.org/abs/2408.02442); [NLT citing it](https://arxiv.org/html/2510.14453v1)
  - Contrary view: .txt (Outlines authors), "Say What You Mean" (Will Kurt, 2024; not re-fetched this session), argued the degradation came from prompt and parse mismatches, and that properly done structured generation matched or beat unstructured output. — [dottxt blog](https://blog.dottxt.co/say-what-you-mean.html)
- **The Constraint Tax: Measuring Validity-Correctness Tradeoffs in Structured Outputs for Small Language Models** — Jaideep Ray, May 2026, arXiv [2605.26128](https://arxiv.org/abs/2605.26128). 15,000 generations, 3 small models.
  - Hard answer-only schema decoding raised schema validity from **61.5% to 100.0%** but cut answer accuracy from **19.7% to 11.0%**.
  - The share of "wrong but schema-valid" outputs rose from 49.5% to 88.9%.
  - Qwen2.5-1.5B executable accuracy fell from **91.5% to 48.0%**.
  - — [arXiv](https://arxiv.org/abs/2605.26128)
- **The Hidden Cost of Structured Generation: Draft-Conditioned Constrained Decoding** — Avinash Reddy et al., Feb 2026, arXiv [2603.03305](https://arxiv.org/abs/2603.03305). Training-free: generate an unconstrained draft, then decode under constraints conditioned on the draft. **Up to +24 pp**; GSM8K for a 1B model went from **15.2% to 39.0%**. — [arXiv](https://arxiv.org/abs/2603.03305)
- **JSONSchemaBench** — Saibo Geng et al. (EPFL / Microsoft), Jan 2025, arXiv [2501.10868](https://arxiv.org/abs/2501.10868). 10K real-world JSON schemas, evaluating 6 constrained-decoding frameworks (including Guidance, Outlines, llama.cpp, XGrammar, OpenAI and Gemini) on coverage, efficiency and quality. A secondary summary reports XGrammar with **38 under-constrained failures** on the JSON Schema Test Suite vs 1 for Guidance, and modest quality *gains* from constrained decoding. The summary is unverified and conflicts in direction with the Constraint Tax paper. — [arXiv](https://arxiv.org/abs/2501.10868); [summary](https://beancount.io/bean-labs/research-logs/2026/07/08/jsonschemabench-structured-outputs-language-models)
- **XGrammar** — Yixin Dong et al., Nov 2024, arXiv [2411.15100](https://arxiv.org/abs/2411.15100) (MLSys 2025). CFG-constrained decoding with **up to 100x speedup** over prior solutions, giving near-zero overhead in serving. — [arXiv](https://arxiv.org/abs/2411.15100)
- **Guiding LLMs The Right Way (DOMINO)** — Luca Beurer-Kellner et al. (ETH Zurich), 2024, arXiv [2403.06988](https://arxiv.org/abs/2403.06988) (ICML 2024). Naive constraining misaligns grammar boundaries with subword tokens and distorts outputs. DOMINO is non-invasive, and in some cases runs **almost 2x faster** than unconstrained decoding. — [arXiv](https://arxiv.org/abs/2403.06988)
- **Outlines: Efficient Guided Generation for LLMs** — Brandon Willard, Rémi Louf, July 2023, arXiv [2307.09702](https://arxiv.org/abs/2307.09702). Indexes regex/CFG into finite-state machines over the vocabulary for O(1)-per-token guided generation. — [arXiv](https://arxiv.org/abs/2307.09702)
- **OpenAI Structured Outputs** (Aug 2024; not re-fetched this session): with `strict: true`, gpt-4o-2024-08-06 scored 100% on OpenAI's complex JSON-schema-following eval, vs <40% for gpt-4-0613. — [OpenAI](https://openai.com/index/introducing-structured-outputs-in-the-api/)

### Inferences
- **Xylo tool-calling "adapter" per model family.** Have one internal representation (name, args, call_id) and several wire formats:
  1. **Native** (Anthropic, OpenAI, Gemini, and Ollama `tools` for models whose template supports them).
  2. **Prompted text protocol** for models without reliable native support. Tools are described in the system prompt, and the model emits a delimited block such as `<tool_call>{"name":…,"arguments":{…}}</tool_call>` (Hermes/Qwen style). BFCL "Prompt" mode shows this is a legitimate, scored path.
  3. **NLT-style two-step** for weak models: (a) a selection pass where the model answers YES/NO per tool, or lists names, in plain text; (b) an argument pass for only the selected tool under a JSON-schema grammar.

  Choose the adapter per model from a small built-in eval (BFCL-style simple, multiple, parallel and irrelevance cases, plus Xylo's own tools). Do not hard-code the choice.
- **Constrain only the call, not the thinking.** With llama.cpp, use a grammar that activates only when the model starts a tool call. llama.cpp's function-calling support (run with `--jinja`) uses "lazy" grammars triggered on the tool-call opening token; this is from the llama.cpp docs, not re-fetched this session ([llama.cpp docs](https://github.com/ggml-org/llama.cpp/blob/master/docs/function-calling.md)). Alternatively, do draft-then-constrain (2603.03305): let the model write a free-form draft or plan, then run a constrained pass for the argument JSON. Avoid answer-only hard schemas on small models (Constraint Tax). If a schema includes a reasoning field, put it **before** answer fields, since decoding follows key order.
- **Validate, repair, feed back.** Parse in this order:
  1. Strict JSON.
  2. A lenient repair pass: strip code fences, trailing commas, single quotes, unescaped newlines; take the first balanced object.
  3. Validate against the tool's JSON Schema (e.g. the Rust `jsonschema` crate).
  4. Coerce safe types ("5" to 5, a single value to an array).

  On failure, do not execute and do not show an approval prompt. Return a structured error to the model containing the validation message, the expected schema snippet and a correct example (Anthropic's actionable-errors guidance), with a bounded retry budget of 2–3. Log repair rates per model as telemetry.
- **Guard against eager invocation.** Include an explicit "answer directly when no tool is needed" instruction and irrelevance tests in the per-model eval, since Docker saw local models call tools on greetings.
- Fewer tools per prompt matters most for local models (Less is More, TinyAgent, LongFuncEval, and Microsoft's 32K-context overflow data). Q1's deferred loading should be on by default for local models, with a lower threshold.
- Quantization (Q4_K_M) appears safe for tool calling per Docker's tests. Local latency, not accuracy, may be the binding constraint, which favors plan-then-execute (Q5) and fewer LLM round trips.

### Gaps
- There is no consensus on whether constrained decoding helps or hurts tool-call accuracy. JSONSchemaBench (secondhand) suggests modest gains, while Let Me Speak Freely and the Constraint Tax show losses, especially for small models and answer-only schemas. The effect is task-, model- and schema-dependent, and Xylo should measure it per model.
- I found no peer-reviewed study comparing Ollama's native `tools` API vs prompted Hermes-style text vs grammar-constrained output on the *same* local models.
- I did not extract specific BFCL v3/v4 leaderboard numbers for local models, or BFCL v4's format-sensitivity results. Check the live leaderboard.
- The NLT per-model breakdown (which models lose) is only secondhand.

---

## Q7. Error recovery: self-correction after tool errors, retries, tool hallucination, failure modes

### Takeaway
LLMs are poor at noticing and recovering from tool errors, especially silent or implicit ones. ToolMaze reports recovery rates dropping ~37% for implicit faults, and ToolBeHonest's best hallucination-diagnosis score was 45.3/100. MCP servers often fail to flag errors at all: GPT-4.1 judged 59% of unflagged results to contain errors. Harness-side guardrails help: argument validation, linting, explicit `isError`, actionable error text, keeping failures in context, and bounded retries. SWE-agent's linter guardrail alone was worth +3 points.

### Cited Findings
- **CRITICTOOL: Evaluating Self-Critique Capabilities of LLMs in Tool-Calling Error Scenarios** — arXiv [2506.13977](https://arxiv.org/abs/2506.13977) (June 2025). Classifies multi-step tool-call errors into five patterns, including tool hallucination, and finds most LLMs struggle to detect and recover from both internal (model) and external (environment) errors (snippet only). — [arXiv](https://arxiv.org/pdf/2506.13977)
- **ToolBeHonest: A Multi-level Hallucination Diagnostic Benchmark for Tool-Augmented LLMs** — Yuxiang Zhang et al., June 2024, arXiv [2406.20015](https://arxiv.org/abs/2406.20015). 700 samples across 7 tasks, covering missing, unavailable or insufficient tools. The top scores were only **45.3 (Gemini-1.5-Pro) and 37.0 (GPT-4o) out of 100**. — [arXiv](https://arxiv.org/abs/2406.20015)
- **Reducing Tool Hallucination via Reliability Alignment (Relign)** — Hongshen Xu et al., Dec 2024, arXiv [2412.04141](https://arxiv.org/abs/2412.04141). A taxonomy of tool-selection and tool-usage hallucinations. The alignment remedy is training-based, but the taxonomy is useful. — [arXiv](https://arxiv.org/abs/2412.04141)
- **Microsoft MCP survey**: of **5,983** tool results with no error flag, GPT-4.1 judged **3,536 (~59%)** to actually contain errors. Servers commonly return errors as normal content. — [Microsoft Research blog](https://www.microsoft.com/en-us/research/blog/tool-space-interference-in-the-mcp-era-designing-for-agent-compatibility-at-scale/)
- **Manus**: keep failed actions and errors in context so the model updates its beliefs and avoids repeating mistakes. Removing tools mid-task causes hallucinated actions and schema violations. — [Manus blog](https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus)
- **Anthropic**: error responses should be actionable and show correctly formatted inputs, not opaque codes or tracebacks. Frequent invalid-parameter errors in evals suggest descriptions need examples. — [Anthropic engineering](https://www.anthropic.com/engineering/writing-tools-for-agents)
- **SWE-agent guardrail**: an edit tool with a linter that rejects syntax errors scored 18.0 vs 15.0 without, on SWE-bench Lite. — [arXiv 2405.15793](https://arxiv.org/html/2405.15793)
- **Docker local evaluation**: failure modes in small local models were eager invocation, wrong tool, missing or malformed parameters, and ignoring tool output. — [Docker blog](https://www.docker.com/blog/local-llm-tool-calling-a-practical-evaluation/)
- **AnyTool**: self-reflection when a solution fails (re-entering hierarchical retrieval) is part of its +35.4% gain over ToolLLM. — [arXiv 2402.04253](https://arxiv.org/abs/2402.04253)
- 2026 error-recovery benchmarks (snippet only; methods not read in full):
  - **When Tools Fail / ToolMaze**, arXiv [2606.05806](https://arxiv.org/abs/2606.05806): models over-trust corrupted outputs, and recovery rates fall ~**37%** for implicit or deceptive faults vs explicit errors.
  - **Beyond Function Calling (ToolBench-X)**, arXiv [2606.25819](https://arxiv.org/abs/2606.25819): failures stem more from poor hazard diagnosis and ineffective recovery than from limited inference budget.
  - **ParaRecover**, arXiv [2609.12345](https://arxiv.org/abs/2609.12345) (EMNLP 2026 per listing): 10,626 instances and 14 error types in *parallel* tool-use. Error propagation and implicit failures remain hard.
  - **Closed-World Resolution Against Tool Hallucination**, arXiv [2609.19425](https://arxiv.org/abs/2609.19425): a five-class hallucination taxonomy, a Hallucinated-Tools Benchmark, and a live study across ten MCP-connected models.
  - **Outcome Monitors: Recovery Affordances for Silent Tool Failures**, arXiv [2608.19303](https://arxiv.org/abs/2608.19303) (title only seen).
  - — [ToolMaze](https://arxiv.org/abs/2606.05806); [ToolBench-X](https://arxiv.org/abs/2606.25819); [ParaRecover](https://arxiv.org/abs/2609.12345); [Closed-World](https://arxiv.org/pdf/2609.19425)

### Inferences
- **Unknown tool name (hallucinated tool).** Do not error opaquely. Return `isError: true` with "Tool `X` does not exist," the 3 closest names by edit distance or BM25 over the registry, and a pointer to `tool_search` if deferred tools exist. "Closed world" resolution means the harness is the authority on which tools exist.
- **Invalid arguments.** Validate before the approval prompt and before execution, and return a schema-guided error with an example. Never show the user an approval dialog for a call that will fail validation.
- **Silent failures.** Wrap MCP results in a lightweight error detector. Look for HTTP 4xx/5xx text, "error", "exception", "not found", "unauthorized" and empty results where data was expected. Prefix suspected errors with a harness note, such as "[Xylo: this result may indicate an error]", rather than silently passing them through. This is motivated by the 59% unflagged-error finding and ToolMaze's over-trust finding.
- **Retry policy.**
  - Transient errors (timeouts, rate limits) are retried automatically by the harness with backoff, without consulting the LLM. This applies only to tools that are idempotent or read-only.
  - Semantic errors go back to the model.
  - Cap consecutive failures of the same tool and signature, for example at 3. After that, inject a "stop and reconsider or ask the user" message to avoid loops.
- **Keep errors in context** (Manus), but compress repeated identical errors into one line with a count, to save context on small-window local models.
- **Guardrails in built-in tools** (SWE-agent): syntax-check on edit, existence check on paths, dry-run or diff preview for destructive file operations. These also improve the quality of approval prompts.
- **Track per-model error telemetry**: invalid JSON, schema failures, unknown tools, eager calls, and repeated identical calls. This feeds the model-adapter choice in Q6.

### Gaps
- Most 2026 recovery benchmarks were seen only as snippets. I could not extract which harness-level interventions (retry prompts, error formatting) they found most effective.
- I found no controlled study isolating the effect of *error-message wording* on recovery rate. The evidence is guidance from Anthropic and Manus plus the SWE-agent linter ablation.
- No source quantified how often small local models hallucinate non-existent tool names vs frontier models under the same harness.
