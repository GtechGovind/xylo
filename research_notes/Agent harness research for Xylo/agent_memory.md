# Long-Term Memory Management for LLM Agents (notes for the Xylo harness)

Scope: cross-session memory for agents: architectures, benchmarks, procedural/experience memory, write policies, retrieval, and security. In-context compaction and tool-calling design are out of scope; another researcher covers them. All arXiv IDs below were checked against the arXiv API on 2026-10-31: titles, authors and dates match. Vendor numbers (Mem0, Zep, Letta, MemoryLake, MemPalace) are self-reported unless noted otherwise.

---

## Key Question 1: Memory architectures. What do MemGPT/Letta, Generative Agents, A-MEM, Mem0, Zep/Graphiti, MemoryOS, MIRIX, MemOS, HippoRAG, CoALA and the 2025–2026 successors do, and what are their measured trade-offs?

### Takeaway
Nearly every system uses the same three-part shape:
- a small, always-in-context "core" block (MemGPT core memory, Anthropic's memory directory, MemPalace's ~170-token wake-up layer);
- a searchable episodic store holding raw conversation history;
- a distilled semantic store of facts or entities, sometimes a graph with temporal validity.

The systems differ in how much LLM work happens at write time. The measured trade-off is consistent: heavy LLM extraction and graph building cut read-time tokens by about 4–15×, but it adds write cost, creates extraction hallucinations, and on short histories it often loses to plain full-context or verbatim retrieval. The 2025–2026 trend runs toward:
- non-destructive writes (ADD-only, or temporal invalidation instead of DELETE);
- hybrid retrieval;
- explicit provenance;
- offline ("sleep-time") consolidation.

### Cited Findings

**Taxonomies and surveys**
- **CoALA: Cognitive Architectures for Language Agents.** Sumers, Yao, Narasimhan, Griffiths (Princeton), 2023; TMLR 2024; arXiv 2309.02427.
  - Memory taxonomy: working, episodic, semantic and procedural memory.
  - Decision cycle: "internal actions" (retrieval, reasoning, learning) are separate from external grounding actions. Learning means writing to episodic, semantic or procedural memory, or updating code/prompts.
  - [arXiv 2309.02427](https://arxiv.org/abs/2309.02427)
- **A Survey on the Memory Mechanism of LLM-based Agents.** Zhang et al. (Renmin Univ.), 2024. [arXiv 2404.13501](https://arxiv.org/abs/2404.13501)
- **Memory in the Age of AI Agents.** Hu et al., Dec 2025.
  - Proposes three lenses:
    - forms: token-level, parametric, latent;
    - functions: factual, experiential, working;
    - dynamics: formation, evolution, retrieval.
  - Says long/short-term taxonomies "have proven insufficient". Distinguishes agent memory from RAG and context engineering. Compiles benchmarks and open-source frameworks.
  - [arXiv 2512.13564](https://arxiv.org/abs/2512.13564)
- **Rethinking Memory in LLM based Agents: Representations, Operations, and Emerging Topics.** Du et al., 2025. Survey of memory operations: consolidation, indexing, updating, forgetting, retrieval, compression. [arXiv 2505.00675](https://arxiv.org/abs/2505.00675)

**MemGPT / Letta (virtual context, self-editing memory)**
- **MemGPT: Towards LLMs as Operating Systems.** Packer, Wooders, Lin, Fang et al. (UC Berkeley, later Letta), 2023.
  - Technique: "virtual context management", modeled on OS memory hierarchies. Data is paged between fast memory (the context window) and slow memory, and interrupts handle control flow.
  - Evaluated on document analysis and multi-session chat.
  - The paper defines in-context "core memory", which the agent edits via function calls, plus external "recall" (conversation history search) and "archival" (vector store) memory.
  - [arXiv 2310.08560](https://arxiv.org/abs/2310.08560)
- MemGPT's Deep Memory Retrieval (DMR) score is **93.4%**, as cited by Zep, which reports 94.8% on the same benchmark. [arXiv 2501.13956](https://arxiv.org/abs/2501.13956)

**Generative Agents (memory stream + reflection)**
- **Generative Agents: Interactive Simulacra of Human Behavior.** Park, O'Brien, Cai, Morris, Liang, Bernstein (Stanford/Google), UIST 2023.
  - Retrieval score = recency + importance + relevance, each min-max normalized to [0,1], all weights α=1:
    - recency: exponential decay, factor **0.995** per game-hour since last access;
    - importance: an LLM-rated 1–10 integer, assigned at write time;
    - relevance: embedding cosine similarity.
  - Reflection runs when the summed importance of recent events exceeds **150**, about 2–3 times per day. The model:
    - takes the 100 most recent records;
    - generates 3 salient questions;
    - retrieves against them;
    - writes 5 insights, each with pointers to its supporting memories.
  - Ablation (TrueSkill believability):

    | Configuration | Score |
    |---|---|
    | Full architecture | 29.89 |
    | No reflection | 26.88 |
    | No reflection and no planning | 25.64 |
    | Human crowdworkers | 22.95 |
    | No memory at all | 21.21 |

  - [arXiv 2304.03442](https://arxiv.org/abs/2304.03442)

**MemoryBank (forgetting curve)**
- **MemoryBank: Enhancing LLMs with Long-Term Memory.** Zhong et al., 2023; AAAI 2024. Memory strength is updated using an Ebbinghaus forgetting-curve model: memories decay unless recalled, and recall reinforces them. [arXiv 2305.10250](https://arxiv.org/abs/2305.10250)

**A-MEM (Zettelkasten)**
- **A-MEM: Agentic Memory for LLM Agents.** Xu, Liang, Mei, Gao et al. (Rutgers), 2025; NeurIPS 2025 according to the arXiv comment.
  - Each note holds: content, timestamp, LLM-generated keywords, tags, a contextual description, an embedding (all-MiniLM-L6-v2) and links.
  - On write:
    1. take the top-k nearest notes by cosine similarity;
    2. an LLM decides which links to create;
    3. an LLM may also *evolve* neighbors' descriptions and tags.
  - Retrieval returns the top-k notes by cosine. k=10 by default; 40–50 for GPT models, ~10 for small models.
  - [arXiv 2502.12110](https://arxiv.org/abs/2502.12110), with details from the [HTML v11](https://arxiv.org/html/2502.12110v11)
- A-MEM cost and small-model results:
  - About **1,200 tokens per memory operation**, versus ~16,900 for the LoCoMo/MemGPT baselines (85–93% less). Under $0.0003 per operation.
  - Latency: ~5.4 s per operation with GPT-4o-mini; **~1.1 s with local Llama 3.2 1B** on one GPU.
  - Small-model overall LoCoMo F1:

    | Model | A-MEM | MemoryBank (best baseline) |
    |---|---|---|
    | Qwen2.5-1.5B | 18.23 | 11.14 |
    | Llama 3.2-1B | 19.06 | 13.18 |

  - Caveat: Table 1's column labels appear misaligned, so per-category claims need checking against the PDF.
  - [arXiv HTML](https://arxiv.org/html/2502.12110v11)

**Mem0 / Mem0g (extract–update pipeline; 2025 paper vs 2026 algorithm)**
- **Mem0: Building Production-Ready AI Agents with Scalable Long-Term Memory.** Chhikara, Khant, Aryan, Singh et al. (Mem0), Apr 2025.
  - Two phases:
    - Extraction: an LLM reads a running conversation summary plus recent messages and extracts candidate facts.
    - Update: each fact is compared with the most similar existing memories, and an LLM tool call chooses **ADD / UPDATE / DELETE / NOOP**.
  - Mem0g adds an entity–relation graph and marks conflicting relations obsolete.
  - Headline claims: 26% relative improvement in LLM-as-Judge over OpenAI memory; **91% lower p95 latency** and **>90% token savings** versus full context.
  - [arXiv 2504.19413](https://arxiv.org/abs/2504.19413)
- Mem0 paper's own LoCoMo J scores (gpt-4o-mini):

  | System | J score |
  |---|---|
  | Mem0 | 66.88 |
  | Mem0g | 68.44 |
  | Zep (as Mem0 ran it) | 65.99 |
  | **Full-context baseline** | **72.90** |

  The full-context baseline *beat* both Mem0 variants on accuracy. [Zep's rebuttal](https://www.getzep.com/blog/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/); consistent with [arXiv 2504.19413](https://arxiv.org/abs/2504.19413)
- **Mem0's 2026 "token-efficient" algorithm** (blog dated Apr 16, 2026; updated Sep 2026). It reverses the 2025 design:
  - single LLM extraction call, **ADD-only**: a changed fact is stored next to the old one and both survive;
  - entity-linking layer;
  - parallel semantic + keyword + entity scoring, fused.
  - Mem0 says UPDATE/DELETE reconciliation was slow and was "where context got destroyed": overwrites erased key information, and deletes removed facts that were needed later.
  - Scores, single-pass retrieval (old → new, with average tokens per query):

    | Benchmark | Old | New | Avg. tokens/query |
    |---|---|---|---|
    | LoCoMo | 71.4 | 92.5 | ~6,956 |
    | LongMemEval | 67.8 | 94.4 | ~6,787 |
    | BEAM-1M | — | 64.1 | — |
    | BEAM-10M | — | 48.6 | — |

  - Weakest BEAM-1M category: contradiction resolution, 35.7. At 10M, temporal reasoning falls to 16.3.
  - The answer and judge models are not stated. Temporal reasoning and memory decay are platform-only features.
  - [Mem0 blog](https://mem0.ai/blog/mem0-the-token-efficient-memory-algorithm); [Mem0 search summary of research page](https://mem0.ai/research)

**Zep / Graphiti (temporal knowledge graph)**
- **Zep: A Temporal Knowledge Graph Architecture for Agent Memory.** Rasmussen, Paliychuk, Beauvais, Ryan et al. (Zep AI), Jan 2025.
  - Graphiti is a temporally aware knowledge graph with three subgraphs: episodes, entities/facts, and communities. It is bi-temporal: it tracks event time and ingestion time, and invalidates superseded edges rather than deleting them.
  - DMR: 94.8% vs MemGPT's 93.4%.
  - LongMemEval: up to **+18.5%** accuracy and **~90% lower latency** versus full context.
  - [arXiv 2501.13956](https://arxiv.org/abs/2501.13956)
- LongMemEval figures from Zep's blog:
  - accuracy: GPT-4o 71.2% (Zep) vs 60.2% (full context); GPT-4o-mini 63.8% vs 55.4%;
  - LongMemEval conversations average ~115k tokens, versus ~16–26k for LoCoMo;
  - Zep re-ran LoCoMo "correctly configured" and reported **75.14% ±0.17**, versus the 65.99% in Mem0's paper.
  - [Zep blog](https://www.getzep.com/blog/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/)
- Mem0 says Zep uses ~1.6K tokens per retrieval on LongMemEval versus Mem0's ~6.8K. (Vendor comparison.) [Mem0 Zep-vs-Mem0](https://mem0.ai/blog/zep-vs-mem0-which-ai-memory-layer-should-you-choose)

**OS-style hierarchies: MemoryOS, MemOS, MIRIX**
- **Memory OS of AI Agent (MemoryOS).** Kang, Ji, Zhao, Bai (BUPT), May 2025.
  - Three tiers: short-term, mid-term and long-term personal memory.
  - Short → mid promotion is dialogue-chain FIFO; mid → long uses segmented-page organization.
  - LoCoMo: +49.11% F1 and +46.18% BLEU-1 over baselines, on GPT-4o-mini.
  - [arXiv 2506.06326](https://arxiv.org/abs/2506.06326)
- **MemOS: A Memory OS for AI System.** Li et al. (MemTensor and collaborators), 2025.
  - The "MemCube" unit wraps memory content with metadata (provenance, versioning). It unifies plaintext, activation (KV-cache) and parameter memories.
  - [arXiv 2507.03724](https://arxiv.org/abs/2507.03724); shorter earlier version [arXiv 2505.22101](https://arxiv.org/abs/2505.22101)
- MemOS claims on LoCoMo versus OpenAI's memory: +159% on temporal reasoning, +38.97% overall accuracy, −60.95% token overhead. These numbers come from the HF abstract and a community README, not from the arXiv text I read; they depend on judge choice. [HF paper page](https://huggingface.co/papers/2507.03724); [GitHub README](https://github.com/codingbull87/MemOS)
- **MIRIX: Multi-Agent Memory System for LLM-Based Agents.** Yu Wang, Xi Chen, Jul 2025.
  - Six memory types: Core, Episodic, Semantic, Procedural, Resource, Knowledge Vault. A multi-agent controller handles updates and retrieval.
  - ScreenshotVQA (~20k screenshots per sequence): **+35% accuracy over RAG with 99.9% less storage**.
  - **LoCoMo 85.4%**.
  - Ships as a local app with "secure local storage".
  - [arXiv 2507.07957](https://arxiv.org/abs/2507.07957)

**Graph-based retrieval memory: HippoRAG**
- **HippoRAG.** Gutiérrez, Shu, Gu, Yasunaga, Su (OSU), NeurIPS 2024.
  - Builds an LLM-extracted knowledge graph and retrieves with Personalized PageRank, modeled on hippocampal indexing.
  - Multi-hop QA gains of up to 20%. Single-step retrieval is 10–30× cheaper and 6–13× faster than iterative IRCoT.
  - [arXiv 2405.14831](https://arxiv.org/abs/2405.14831)
- **HippoRAG 2 (From RAG to Memory).** ICML 2025.
  - Notes that knowledge-graph RAG variants often drop *below* standard RAG on basic factual memory.
  - HippoRAG 2 adds deeper passage integration and gets +7% on associative memory over a state-of-the-art embedding model without losing factual recall.
  - [arXiv 2502.14802](https://arxiv.org/abs/2502.14802)
- In MemoryAgentBench, HippoRAG-v2 is the strongest non-long-context memory system: 41.6 overall, 65.1 on accurate retrieval. [arXiv HTML 2507.05257](https://arxiv.org/html/2507.05257v4)

**2025–2026 successors**
- **Hindsight is 20/20.** Latimer, Boschi, Neeser et al. (Vectorize), Dec 2025.
  - Four logical networks: world facts, agent experiences, entity summaries, evolving beliefs. Three operations: retain, recall, reflect.
  - With an **open-source 20B model**, LongMemEval/LoCoMo accuracy goes from **39% (full context) to 83.6%**.
  - With larger backbones: 91.4% on LongMemEval and 89.61% on LoCoMo.
  - [arXiv 2512.12818](https://arxiv.org/abs/2512.12818)
- **LightMem.** Fang et al. (Zhejiang Univ. zjunlp), ICLR 2026.
  - Three stages modeled on Atkinson–Shiffrin: a sensory filter (compression), topic-grouped short-term memory, and long-term memory updated in a **sleep-time** offline pass.
  - With GPT and Qwen backbones on LongMemEval/LoCoMo:
    - up to +7.7% / +29.3% QA accuracy;
    - up to 38× / 20.9× fewer total tokens;
    - up to 30× / 55.5× fewer API calls;
    - online-only costs fall 106× / 117× in tokens.
  - [arXiv 2510.18866](https://arxiv.org/abs/2510.18866)
- **REALM (Retrieval-Driven Memory Reconsolidation).** Song et al., Sep 2026.
  - Retrieval feedback reorganizes memory into a heterogeneous graph.
  - LoCoMo 75.97%, LongMemEval 65.11% (+7.17 and +1.31 points over the strongest baselines). Ablations show reconsolidation helps.
  - [arXiv 2609.16053](https://arxiv.org/abs/2609.16053)
- **MemPalace critique (Spatial Metaphors for LLM Memory).** Dey & Viradecha, Apr 2026.
  - The open-source MemPalace (~47k GitHub stars in two weeks) claims **96.6% Recall@5 on LongMemEval with zero LLM calls at write time**.
  - Independent replication attributes this to **verbatim storage + ChromaDB's default all-MiniLM-L6-v2 embeddings**, not to the "palace" hierarchy, which works as ordinary metadata filtering.
  - Genuine contributions: ~170-token wake-up cost, and a deterministic, offline, zero-API-cost write path.
  - The paper also notes that Mem0's April 2026 algorithm raised its LongMemEval score from ~49% to 93.4%.
  - [arXiv 2604.21284](https://arxiv.org/abs/2604.21284)
- **Mi-Memory.** Liu, Teng, Li et al. (project page darwin-agent.github.io; affiliation not verified), Jul 2026.
  - Lifecycle framework built on typed evidence payloads with provenance, diagnostic traces, explicit strategy artifacts, and gate/rollback records for memory-policy evolution.
  - MemStack: 93.59% LoCoMo, 87.47% LongMemEval, 57.24% PersonaMem-V2, in "controlled-reference" evaluations.
  - [arXiv 2607.18975](https://arxiv.org/abs/2607.18975)
- **Ground Truth First / Veracium.** Spencer, Jul 2026. A layered architecture scores best among memory systems at both horizons (96.8% short-horizon). Details under Key Question 2. [arXiv 2607.21962](https://arxiv.org/abs/2607.21962)

**File-based memory**
- **Letta, "Benchmarking AI Agent Memory: Is a Filesystem All You Need?"** (Aug 12, 2025).
  - The LoCoMo history was put in a file attached to an agent with `grep`, `search_files`, `open`, `close`, using GPT-4o-mini and minimal prompt tuning.
  - Result: **74.0% on LoCoMo**, versus Mem0's best reported 68.5%.
  - Letta's argument: agents use filesystem tools well because post-training for agentic coding reinforces them, and iterative query rewriting beats single-hop retrieval.
  - [Letta blog](https://www.letta.com/blog/benchmarking-ai-agent-memory)
- **Anthropic memory tool + context editing** (Sep 29, 2025).
  - The memory tool is **file-based and client-side**: Claude creates, reads, updates and deletes files in a memory directory, and the developer controls the storage backend.
  - Internal agentic-search eval: memory tool + context editing **+39%** over baseline; context editing alone +29%.
  - 100-turn web-search eval: **84%** fewer tokens.
  - [Claude blog](https://claude.com/blog/context-management)

### Inferences

**Harness notes per architecture (how Xylo would implement each, with SQLite)**
- **CoALA.** Use it as the schema vocabulary, one table per memory kind:
  - `working` is the in-session context, not persisted;
  - `episodic` is session transcripts plus distilled episodes; Xylo's "search conversation history" already covers this;
  - `semantic` is "project facts";
  - `procedural` is "reusable skills".
  - Writes to each store should be explicit, logged internal actions.
- **MemGPT/Letta.** Implement a small pinned "core memory" block per project and per user (≤1–2k tokens), always injected and editable by the agent via `memory_replace` / `memory_append` tools. Recall memory is FTS search over past sessions. Archival memory is a fact table searched by tool. Sending conversation history to small models through tool calls is risky (see Q5), so give small models the pinned block plus automatic retrieval instead.
- **Generative Agents scoring.** Cheap and model-agnostic.
  - Store `importance` (1–10, rated once at write time; a small local model can do this), `last_accessed_at` and an embedding.
  - Rank by normalized recency (0.995^hours) + importance + relevance.
  - Reflection: trigger an offline job when accumulated importance passes a threshold, write insights with foreign keys to source memories, and show them to the user for review.
- **MemoryBank.** Add a `strength` column that increments on retrieval and decays over time. Use it to demote candidates in ranking, not to hard-delete.
- **A-MEM.** Store notes with keywords, tags, a one-line context and a `links` join table. On insert, take the top-k neighbors and let the LLM propose links and updates to neighbors' descriptions. This works with 1–3B local models at about 1 s per operation, but neighbor "evolution" mutates old memories, so keep version history (Mem0 dropped mutation; see Q4).
- **Mem0 (2026 design).**
  - Insert-only fact rows: one LLM extraction call per turn or per session end.
  - An entity table (proper nouns, file paths, symbols) for entity-boosted ranking.
  - Hybrid retrieval: FTS5 BM25 + embeddings + entity match, fused.
  - Handle conflicts at read time: sort by recency and show both versions with timestamps.
- **Zep/Graphiti.** For Xylo, the valuable part is the bi-temporal validity columns, not the graph database:
  - `valid_from`, `invalid_at`, `recorded_at`, `superseded_by`;
  - mark facts invalid instead of deleting them, so "what was true when" questions stay answerable.
  - A simple entity/edge table in SQLite with recursive CTEs can stand in for a graph store.
- **MemoryOS / MemOS / MIRIX.** Treat these as evidence that typed stores plus metadata (provenance, version, heat/access counts) matter more than OS metaphors. MIRIX's six types map onto Xylo as: Core = pinned block; Episodic = sessions; Semantic = project facts; Procedural = skills; Resource = attached files and docs; Knowledge Vault = secrets, which Xylo should *not* store in memory.
- **HippoRAG.** Only worth it if Xylo indexes large document corpora where multi-hop associations matter. Personalized PageRank over an entity–passage graph is cheap at query time but needs LLM triple extraction at write time.
- **Hindsight / LightMem.** Both show strong results with open-weight backbones. LightMem's design fits a desktop app well: cheap online filtering plus an offline consolidation pass.
- **MemPalace finding.** For Xylo's local and small-model mode, a zero-LLM write path (verbatim chunks + a small local embedding model + metadata filters by project/session) is a strong, cheap baseline, and LLM extraction can run later as an optional consolidation step.
- **Anthropic memory tool / Letta filesystem.** A `/memories` directory of Markdown files that the agent edits through file tools is the simplest design that has evidence behind it. Xylo can back this "directory" with SQLite rows, giving versioning and audit while presenting a file-like API to the model.

### Gaps
- I did not find independently replicated, apples-to-apples numbers (same answer model, same judge) that compare Mem0-2026, Zep, Letta, MIRIX and MemOS. Every headline LoCoMo/LongMemEval number above is self-reported under different judge and model setups.
- The MemOS numbers (+159% temporal, +38.97% overall, −60.95% tokens) were not verified in the arXiv full text.
- MemoryOS venue: not confirmed (possibly EMNLP 2025).
- MIRIX author affiliation: not confirmed.
- I did not fetch Letta's own docs on core/archival memory blocks or "sleep-time agents" (product); the descriptions here rely on the MemGPT and sleep-time papers.
- OpenAI ChatGPT memory ("saved memories" + "reference chat history", 2025) and LangChain LangMem: no primary-source benchmark numbers found or fetched in this pass.

---

## Key Question 2: Benchmarks and evaluations. What do LoCoMo, LongMemEval, MemoryAgentBench and newer benchmarks reveal about which designs work (full context vs RAG vs structured memory) and about failure modes (stale or contradicting memories, temporal reasoning, knowledge updates)?

### Takeaway
On short-to-medium histories that fit in context (LoCoMo, ~16–26k tokens), full context or simple BM25/verbatim retrieval matches or beats elaborate memory systems. Structured memory pays off mainly on cost and latency, and on very long horizons (LongMemEval ~115k tokens; BEAM up to 10M; nine-week histories).

Three things stay unsolved across every benchmark:
- conflict resolution and knowledge updates (multi-hop conflict accuracy ≤28%);
- temporal reasoning at scale;
- applying memory to *act*, as opposed to answering recall questions.

Newer agentic benchmarks (MemoryArena, Mem2ActBench, StreamMemBench, DolphinBench, LongMemEval-V2) show that systems near ceiling on LoCoMo do poorly when memory must drive multi-session tasks.

### Cited Findings

**Recall benchmarks**
- **LoCoMo (Evaluating Very Long-Term Conversational Memory of LLM Agents).** Maharana, Lee, Tulyakov, Bansal et al. (UNC / Snap), ACL 2024.
  - Conversations of ~300 turns and ~9K tokens on average, over up to 35 sessions.
  - Tasks: QA (single-hop, multi-hop, temporal, open-domain, adversarial), event summarization, multimodal dialogue.
  - Long-context LLMs and RAG "substantially lag behind human performance".
  - [arXiv 2402.17753](https://arxiv.org/abs/2402.17753)
- LoCoMo's known problems, per Zep:
  - conversations are only ~16–26k tokens, so they fit in modern context windows;
  - no knowledge-update questions;
  - category 5 has no ground truth and is excluded by everyone;
  - BLIP image captions lack the needed information;
  - speakers are misattributed in some questions;
  - some questions are ambiguous.
  - [Zep blog](https://www.getzep.com/blog/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/)
- Letta also argues LoCoMo measures retrieval rather than memory management, and says Mem0's MemGPT numbers were "controversial" (Letta could not reproduce Mem0's backfill). [Letta blog](https://www.letta.com/blog/benchmarking-ai-agent-memory)
- Judge choice changes LoCoMo scores noticeably; Mem0-style evaluations historically used gpt-4o-mini as judge. [MemMachine PR diff](https://github.com/MemMachine/memmachine.github.io/pull/41.diff)
- **LongMemEval.** Di Wu et al. (UCLA & Tencent AI Lab), ICLR 2025.
  - 500 questions testing five abilities: information extraction, multi-session reasoning, **temporal reasoning, knowledge updates, abstention**.
  - Commercial assistants and long-context LLMs drop ~30% in accuracy.
  - The paper splits memory design into **indexing → retrieval → reading**.
  - [arXiv 2410.10813](https://arxiv.org/abs/2410.10813)
- LongMemEval detailed numbers:
  - Commercial assistants on a 97-question subset, where offline reading with GPT-4o scores 0.918:
    - ChatGPT memory: 0.577 with GPT-4o (−37%);
    - Coze: 0.330 (−64%).
  - Long-context, oracle → LongMemEval_S:

    | Model | Oracle | LongMemEval_S | Drop |
    |---|---|---|---|
    | GPT-4o | 0.870 | 0.606 | −30% |
    | Llama 3.1 70B | 0.744 | 0.334 | −55% |
    | Llama 3.1 8B | 0.710 | 0.454 | −36% |
    | Phi-3.5-mini | 0.660 | 0.342 | −48% |

  - [arXiv HTML](https://arxiv.org/html/2410.10813v2)
- **BEAM (Beyond a Million Tokens).** Tavakoli et al., Oct 2025.
  - 100 coherent conversations of up to **10M tokens**, with 2,000 questions.
  - Even 1M-context LLMs, with or without RAG, "struggle as dialogues lengthen".
  - The LIGHT framework (episodic memory + working memory + scratchpad of salient facts) adds +3.5% to +12.69% over the strongest baselines.
  - [arXiv 2510.27246](https://arxiv.org/abs/2510.27246)

**MemoryAgentBench: four competencies**
- **Evaluating Memory in LLM Agents via Incremental Multi-Turn Interactions (MemoryAgentBench).** Hu, Wang, McAuley (UCSD), Jul 2025. Competencies: accurate retrieval (AR), test-time learning (TTL), long-range understanding (LRU), and selective forgetting / conflict resolution (SF). [arXiv 2507.05257](https://arxiv.org/abs/2507.05257)
- Key results ([HTML v4](https://arxiv.org/html/2507.05257v4)):
  - **AR:** long-context models lead (GPT-5-mini 74.4). Best memory agent: HippoRAG-v2 65.1. **BM25 (60.5) beats embedding RAG (~54–55)** and beats structure/agentic memory by a wide margin:

    | System | AR score |
    |---|---|
    | Zep | 37.5 |
    | GraphRAG | 40.9 |
    | MemGPT | 34.3 |
    | Mem0 | 32.6 |
    | Cognee | 28.3 |

  - **TTL and LRU:** long-context wins (Claude-3.7-Sonnet TTL 53.9; GPT-5-mini LRU 66.2). Retrieval "captures only partial context".
  - **Selective forgetting (FactConsolidation):**
    - multi-hop: most systems score **2–5%**; the best is GPT-5-mini at **28%**; Mem0 2%;
    - o4-mini reaches 80% on the 6K split but **14% at 32K**.
  - Overall: GPT-5-mini 60.6; HippoRAG-v2 41.6; **BM25 41.5**; Mem0 21.1; Cognee 20.6; Self-RAG 18.7.
  - **Backbone matters more for agentic memory than for RAG:** MIRIX goes from 26.2 to 37.7 overall when its backbone moves from GPT-4o-mini to GPT-4.1-mini, while RAG agents gain only marginally.
  - Smaller chunks help AR but hurt LRU.

**Hallucination and governance**
- **HaluMem: Evaluating Hallucinations in Memory Systems of Agents.** Chen et al., Nov 2025.
  - Operation-level benchmark (extraction, updating, QA) with ~15k memory points and 3.5k questions, over contexts >1M tokens.
  - Finding: existing memory systems "generate and accumulate hallucinations during the extraction and updating stages", which then propagate to QA.
  - [arXiv 2511.03506](https://arxiv.org/abs/2511.03506)

**Agentic and longitudinal benchmarks (2026)**
- **EverMemBench (Evaluating Long-Horizon Memory for Multi-Party Collaborative Dialogues).** Hu et al., Feb 2026.
  - Multi-party, multi-group conversations over 1M+ tokens.
  - Multi-hop reasoning collapses under multi-party attribution *even with oracle evidence* (26%).
  - "Temporal reasoning fails without explicit version semantics beyond timestamps."
  - Similarity-based retrieval misses implicitly relevant information.
  - [arXiv 2602.01313](https://arxiv.org/abs/2602.01313)
- **MemoryArena.** He, Wang, Zhi et al., Feb 2026; ICML 2026.
  - Multi-session memory–agent–environment loops: web navigation, preference-constrained planning, progressive search, sequential formal reasoning.
  - Agents "with near-saturated performance on existing long-context memory benchmarks like LoCoMo perform poorly in our agentic setting".
  - [arXiv 2602.16313](https://arxiv.org/abs/2602.16313)
- **MemoryLake on MemoryArena** (vendor-authored matched study, Aug 2026, gpt-5-mini).
  - Equal-weight average success rate: MemoryLake 20.5% vs 13.6% for the best comparator (Mem0, text-embedding-3-small RAG, long context).
  - **Every system scored 0 on travel planning.**
  - Authors say confidence intervals overlap and the results support "a workload-dependent view".
  - [arXiv 2608.13883](https://arxiv.org/abs/2608.13883)
- **Mem2ActBench.** Shen et al., Jan 2026.
  - 400 tool-use tasks where remembered preferences and state must ground tool parameters.
  - Seven memory frameworks "remain inadequate at actively utilizing memory for parameter grounding".
  - [arXiv 2601.19935](https://arxiv.org/abs/2601.19935)
- **StreamMemBench.** Liu et al.; Findings of EMNLP 2026. Eight memory systems "often fail to use observed evidence or turn feedback into reliable follow-up behavior, even when evidence is stored". [arXiv 2606.14571](https://arxiv.org/abs/2606.14571)
- **DolphinBench.** Rathi, Yadav, Singh (Mem0 authors), Sep 2026.
  - Three knowledge-work personas with ~500k tokens of history each, 200 verified tasks per persona.
  - Requires reporting **cost and latency alongside accuracy**.
  - [arXiv 2609.24971](https://arxiv.org/abs/2609.24971)
- **LongMemEval-V2.** Di Wu et al., May 2026.
  - 451 questions on web-agent experience: static state recall, dynamic state tracking, **workflow knowledge, environment gotchas**, premise awareness. Histories of up to 500 trajectories / 115M tokens.
  - **AgentRunbook-C (trajectories stored as files + a coding agent that gathers evidence): 72.5%**, versus the best RAG baseline at 48.5% and an off-the-shelf coding agent at 69.3%. It has "high latency costs".
  - [arXiv 2605.12493](https://arxiv.org/abs/2605.12493)
- **Ground Truth First (Veracium).** Spencer, Jul 2026.
  - Synthetic corpus with per-fact validity intervals, sent/received trust distinctions, injection probes and as-of-date questions.
  - **Rankings invert with history length:** a *budgeted curated-map memory* leads at 3 weeks, but its recall of evicted content falls **96% → 72% by 9 weeks**, while a provenance-typed graph rises to 90%.
  - Full rendered history ties or beats the best memory at the short horizon at ~2× read cost, with no judge-independent advantage at 9 weeks.
  - **Write quality predicts downstream quality:** weakly written facts fail 24% of the time versus 2%.
  - [arXiv 2607.21962](https://arxiv.org/abs/2607.21962)
- **Evo-Memory.** Wei, Sachdeva, Coleman, He et al., Nov 2025.
  - Streaming benchmark of **test-time learning** across 10 datasets, with >10 memory modules re-implemented.
  - Baselines: ExpRAG (experience retrieval) and ReMem (an action–think–memory-refine loop).
  - [arXiv 2511.20857](https://arxiv.org/abs/2511.20857)

### Inferences
- For Xylo's typical project histories (tens of sessions), full context of the *relevant* sessions plus BM25 search will be hard to beat on accuracy. Structured memory is mainly worth adding for cost and latency on local models and for long-lived projects (months). Ground Truth First's crossover suggests a capped "curated file" memory (CLAUDE.md-style) degrades as content gets evicted, so Xylo needs a searchable archive behind any capped file.
- Knowledge updates and contradictions are the weakest point everywhere, so Xylo should:
  - store explicit versions and validity intervals ("version semantics beyond timestamps");
  - surface conflicts to the user, rather than trusting an LLM to UPDATE/DELETE correctly.
- Evaluate on Xylo's own tasks, not LoCoMo. A small internal suite modeled on Mem2ActBench, MemoryArena or DolphinBench would test whether remembered facts (build commands, conventions, preferences) change actions in later sessions, and would report tokens and latency for each model tier.
- LongMemEval-V2's result (files + coding-agent search > RAG) matches Xylo's coding-agent setting. For capable cloud models, "past sessions as searchable files + grep/FTS tools" is the best-evidenced design for environment experience.

### Gaps
- No independent third-party leaderboard compares commercial memory layers under a fixed answer model and judge. DolphinBench is from Mem0 authors; MemoryLake's study is vendor-authored.
- Per-category LongMemEval results for knowledge updates and abstention in the original paper were not extracted; that part of the HTML was truncated.
- I found no benchmark specifically on *coding-agent project memory* (repo conventions, build commands) apart from LongMemEval-V2's web-environment framing and SWE-bench numbers inside procedural-memory papers.

---

## Key Question 3: Procedural memory and experience learning. Reflexion, ExpeL, Voyager, AWM, Memp, ReasoningBank, Dynamic Cheatsheet, Agent KB, ACE, and Claude-style skills: how can an agent learn reusable workflows or lessons from past sessions?

### Takeaway
Distilling past trajectories into short, retrievable strategy notes, workflows or code is the best-evidenced way to make agents improve across sessions without fine-tuning:
- typical gains of +4 to +9 points on web and SWE agents;
- large gains on repetitive reasoning tasks;
- curated skills: +16.6pp on SkillsBench.

Three conditions matter:
1. Lessons from *failures* help as well as successes.
2. Incremental, itemized updates beat whole-document rewrites, which cause "context collapse".
3. Libraries degrade as they grow: retrieval precision collapses and self-distilled skills are often worse than raw trajectories.

So small, curated, user-reviewable skill sets with progressive disclosure are the safe default.

### Cited Findings

**Reflection and insight extraction**
- **Reflexion: Language Agents with Verbal Reinforcement Learning.** Shinn, Cassano, Berman, Gopinath, Narasimhan, Yao, NeurIPS 2023.
  - After a failure, the agent writes a verbal self-reflection into an episodic buffer used on later trials.
  - **91% pass@1 on HumanEval vs 80% for GPT-4**.
  - [arXiv 2303.11366](https://arxiv.org/abs/2303.11366)
- **ExpeL: LLM Agents Are Experiential Learners.** Zhao et al. (Tsinghua), AAAI 2024.
  - Gathers experiences on training tasks and extracts natural-language **insights** by comparing successful and failed trajectories. The paper's insight list is maintained with ADD / EDIT / UPVOTE / DOWNVOTE operations.
  - At inference it recalls insights plus similar successful trajectories; performance improves as experience accumulates.
  - [arXiv 2308.10144](https://arxiv.org/abs/2308.10144)

**Skill libraries and workflows**
- **Voyager.** Wang et al. (NVIDIA / Caltech et al.), 2023.
  - Ever-growing **skill library of executable code**, retrieved by description embedding, with self-verification before a skill is added.
  - Results: **3.3× more unique items, 2.3× longer distances, tech-tree milestones up to 15.3× faster**. Skills transfer to new worlds.
  - [arXiv 2305.16291](https://arxiv.org/abs/2305.16291)
- **Agent Workflow Memory (AWM).** Z. Wang, Mao, Fried, Neubig (CMU / MIT), 2024.
  - Induces reusable **workflows** from past trajectories, offline or online from test queries, and adds them to agent memory.
  - **+24.6% relative success on Mind2Web and +51.1% on WebArena**, with fewer steps. Online AWM beats baselines by 8.9–14.0 absolute points as train/test distribution gaps widen.
  - [arXiv 2409.07429](https://arxiv.org/abs/2409.07429)
- **Memp: Exploring Agent Procedural Memory.** Fang et al. (Zhejiang Univ. zjunlp group, per code repo); ACL 2026 Findings.
  - Distills trajectories into fine-grained step-by-step instructions *and* script-like abstractions. Compares Build, Retrieval and Update strategies, with continuous update, correction and **deprecation**.
  - TravelPlanner and ALFWorld: success and efficiency rise as memory is refined.
  - **Procedural memory built by a stronger model transfers to a weaker model with substantial gains.**
  - [arXiv 2508.06433](https://arxiv.org/abs/2508.06433)

**Reasoning memory and evolving playbooks**
- **ReasoningBank.** Ouyang et al. (Google Cloud AI Research et al.); ICLR 2026.
  - Memory items: `{title, one-sentence description, content}`. Up to 3 items are extracted per trajectory from **both self-judged successes and failures**, using an LLM judge with no ground truth.
  - Retrieval: gemini-embedding cosine, **top-k=1** by default. Consolidation is a simple append.
  - [arXiv 2509.25140](https://arxiv.org/abs/2509.25140)
- ReasoningBank results (success rate SR, average steps AS) ([HTML](https://arxiv.org/html/2509.25140v2)):

  | Setting | No memory | ReasoningBank |
  |---|---|---|
  | WebArena, Gemini-2.5-flash | 40.5 SR, 9.7 steps | **48.8** SR, 8.3 steps (Synapse 42.1, AWM 44.1) |
  | WebArena, Gemini-2.5-pro | 46.7 | 53.9 |
  | WebArena, Claude-3.7-Sonnet | 41.7 | 46.3 |
  | SWE-Bench-Verified, Gemini-2.5-flash | 34.2 | 38.8 (fewer steps) |
  | SWE-Bench-Verified, Gemini-2.5-pro | 54.0 | 57.4 |

  - The LLM judge agreed with ground truth only 72.7% of the time, yet judges simulated at 70–90% accuracy performed about as well as ground truth, so the method tolerates noisy self-judgment.
  - MaTTS (memory-aware test-time scaling) adds about +2.5–3 points.
- **Dynamic Cheatsheet.** Suzgun, Yuksekgonul, Bianchi, Jurafsky, Zou (Stanford), 2025.
  - A self-curated, persistent "cheatsheet" of strategies and code snippets for black-box LMs.
  - Results:
    - **Claude 3.5 Sonnet's AIME accuracy more than doubled**;
    - **GPT-4o on Game of 24: 10% → 99%**;
    - equation balancing: near-perfect, versus ~50% baselines;
    - GPQA-Diamond +9% and MMLU-Pro +8% for Claude.
  - [arXiv 2504.07952](https://arxiv.org/abs/2504.07952)
- **Agentic Context Engineering (ACE).** Zhang et al. (Stanford / SambaNova / UC Berkeley); ICLR 2026.
  - Treats context as an evolving **playbook**, maintained by generation → reflection → curation.
  - Identifies **"brevity bias"** (summaries drop domain insights) and **"context collapse"** (iterative rewriting erodes detail), and prevents them with **structured, incremental updates**.
  - **+10.6% on agents** (AppWorld), **+8.6% on finance**. Matches the top AppWorld production agent using a smaller open-source model. Works from execution feedback without labels.
  - [arXiv 2510.04618](https://arxiv.org/abs/2510.04618)

**Experience sharing and case-based memory**
- **Agent KB.** Tang et al., 2025.
  - Cross-framework experience knowledge base with hybrid retrieval: planning seeds plus feedback-stage fixes, and a "disagreement gate" against knowledge interference.
  - smolagents GAIA: 55.2% → 73.9% at pass@3. OpenHands SWE-bench: 24.3% → 28.3% pass@1.
  - Automatically generated experiences matched manual curation.
  - [arXiv 2507.06229](https://arxiv.org/abs/2507.06229)
- **Memento: Fine-tuning LLM Agents without Fine-tuning LLMs.** Zhou et al., 2025.
  - Case-based episodic memory with a learned case-selection policy (memory-based online RL).
  - GAIA validation 87.88% pass@3; test 79.40%. Case memory adds +4.7 to +9.6 points on out-of-distribution tasks.
  - [arXiv 2508.16153](https://arxiv.org/abs/2508.16153)

**Agent Skills (SKILL.md) and their evaluation**
- **Anthropic Agent Skills** (Oct 16, 2025). Progressive disclosure in three levels:
  1. only `name` + `description` of every skill is preloaded in the system prompt;
  2. SKILL.md is read when the skill is relevant;
  3. bundled files are opened only when needed.
  - Anthropic states the goal of agents that "create, edit, and evaluate Skills on their own" as future work; today a human asks Claude to capture approaches into a skill.
  - Security: install only trusted skills, and audit scripts and network instructions.
  - [Anthropic engineering](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills)
- **SkillsBench.** Li et al., Feb 2026.
  - 87 tasks, 8 domains, 18 model–harness configurations.
  - **Curated Skills raise the average pass rate from 33.9% to 50.5% (+16.6pp)**; configuration-level gains range from +4.1 to +25.7pp.
  - **Focused skills with ≤3 modules beat larger bundles**, and **smaller models with Skills can match larger models without them**.
  - [arXiv 2602.12670](https://arxiv.org/abs/2602.12670)
- **Demystifying Agent Skills: Why They Work—Until They Don't.** Jiang et al., Aug 2026.
  - Based on 8,135 trials.
  - Skills beat Workflow Memory by 6.06 points.
  - In 65.7% of cases skills work as "**procedural anchors**", versus 4.5% for knowledge injection.
  - **Retrieval precision falls from 29.6% to 3.3% as the pool grows from 5 to 100 skills.**
  - Skills fail under brittle assumptions or incompatible contexts.
  - [arXiv 2608.14036](https://arxiv.org/abs/2608.14036)
- **SkillEvolBench.** Lei et al., May 2026.
  - 180 tasks, 10 model configurations, 3 harnesses.
  - Agents "often adapt locally but rarely form robust reusable skills".
  - **Raw-trajectory reuse frequently outperforms distilled skills.**
  - Writing more skills adds "episode-specific drift and procedural clutter".
  - [arXiv 2605.24117](https://arxiv.org/abs/2605.24117)
- **AFTER (Managing Procedural Memory in LLM Agents).** Belikova et al., Jun 2026.
  - 382 enterprise tasks and 22 skills.
  - One refinement round adds +3.7–6.7 points.
  - **Skills evolved from diverse multi-model traces reach 73.1% cross-model accuracy**, beating single-model sources. Some skills over-specialize to particular roles.
  - [arXiv 2606.23127](https://arxiv.org/abs/2606.23127)
- **Authoring Agent Skills: A Software-Engineering Approach.** Destefanis, Jul 2026.
  - Treats skills as software artifacts: single responsibility, low coupling, economy in a shared token budget, behavioral evaluation.
  - Compares skills with project memory files, slash commands, subagents and hooks, using the rule "who decides that a mechanism runs and what guarantee it provides".
  - [arXiv 2607.25032](https://arxiv.org/abs/2607.25032)

### Inferences

**Harness notes**
- **Reflexion → "lessons" table.** At session end, or after a failed tool sequence or test run, have the model write a 1–3 sentence lesson tied to a trigger context (repo, tool, error signature). Retrieve by error signature or BM25 when similar errors reappear. A small local model can write these lessons.
- **ExpeL / ACE → itemized playbook with counters.** Keep each lesson or strategy as its own row with `helpful_count`, `harmful_count` and `last_used`. Apply deltas (add, edit, upvote, downvote) rather than regenerating the whole playbook, to avoid ACE's "context collapse". Prune rows whose net votes go negative.
- **ReasoningBank → schema and retrieval.** Use `{title, description, content}` for both success strategies and failure "pitfalls". Inject only the top 1–3. Self-judging success is acceptable even when the judge is noisy (~70%+ agreement), which matters for Xylo, where ground truth is usually missing. Better success signals: tests passing, user acceptance, no user correction.
- **AWM / Memp → workflow induction.** Periodically cluster successful sessions by task type and induce a parameterized workflow ("to add a new IPC command in this repo: …"). Store it as a *candidate skill* in Xylo's existing "review reusable skills" queue, with deprecation when the workflow later fails.
- **Voyager / Dynamic Cheatsheet → executable snippets.** For coding work, verified scripts or commands (build, test, lint invocations) are higher value than prose. Only store a snippet after it has executed successfully, as Voyager's self-verification does.
- **Skills.** Keep Xylo's skill index small and well-described. Retrieval precision collapses past ~dozens of skills, so scope skills per project/workspace and pre-filter by context (language, repo) before the model sees descriptions. Prefer focused skills with at most three modules.
- **Small local models.** SkillsBench, Memp and AFTER all show procedural memory written by strong models helps weaker ones. Xylo could let a cloud model author or consolidate skills that local models then *consume*, but should not rely on small models to author skills: SkillEvolBench shows even frontier agents rarely distill robust skills.
- **Keep raw trajectories retrievable** alongside distilled skills, since raw-trajectory reuse often beats distilled skills (SkillEvolBench), and support "show me how I did X last time".

### Gaps
- SkillsBench's earlier versions reportedly compared *self-generated* skills too; the v4 abstract reports only curated versus none, so I could not verify the self-generated result.
- I found no controlled study of agent-*authored* SKILL.md files in coding harnesses (Claude Code / Codex-style) with user review in the loop.
- ExpeL's and ACE's exact numeric results were not extracted in this pass (only the abstracts and known mechanisms).

---

## Key Question 4: Memory write policies. What to store, when (online vs end-of-session, sleep-time compute), deduplication, conflicting or outdated facts, forgetting/decay, and user control and privacy.

### Takeaway
Evidence favors four things:
1. a cheap, lossless online capture (verbatim turns and episodes) plus asynchronous or offline consolidation;
2. non-destructive updates (append + supersede/invalidate with validity intervals) over LLM-driven UPDATE/DELETE, which destroyed information in practice (Mem0's 2026 reversal; HaluMem);
3. write quality checks, since badly written facts fail about 12× more often;
4. provenance-tagged, user-visible, editable memory stored locally.

Decay works best as a ranking signal, not as deletion.

### Cited Findings

**Online vs offline consolidation**
- **Sleep-time Compute.** Lin, Snell, Wang, Packer, Wooders, Stoica, Gonzalez (Letta / UC Berkeley), Apr 2025.
  - The model "thinks offline" about a context before queries arrive.
  - Effects:
    - **~5× less test-time compute** for the same accuracy on Stateful GSM-Symbolic and Stateful AIME;
    - up to **+13% / +18%** accuracy when sleep-time compute is scaled;
    - **2.5× lower cost per query** when amortized across related queries.
  - Efficacy correlates with how predictable the user's query is. Includes an agentic SWE case study.
  - [arXiv 2504.13171](https://arxiv.org/abs/2504.13171)
- **LightMem** decouples long-term consolidation into an offline "sleep-time update". Online token cost falls up to 106–117× and API calls 159–310×, while accuracy rises. [arXiv 2510.18866](https://arxiv.org/abs/2510.18866)
- **Generative Agents** consolidate through periodic reflection triggered by accumulated importance (>150), producing cited higher-level insights. [arXiv HTML 2304.03442](https://arxiv.org/html/2304.03442v2)

**Conflicts, updates and deletion**
- **Mem0 2025 vs 2026.**
  - 2025: an online LLM extract → compare → ADD/UPDATE/DELETE/NOOP per fact. [arXiv 2504.19413](https://arxiv.org/abs/2504.19413)
  - 2026: switched to **ADD-only single-pass extraction**, because UPDATE/DELETE "was where context got destroyed". Old and new versions coexist, and history of state changes helps temporal questions. Mem0 also began storing **agent-generated facts** with equal weight, which gave its largest gain (single-session-assistant +51.8 on LongMemEval).
  - [Mem0 blog](https://mem0.ai/blog/mem0-the-token-efficient-memory-algorithm)
- **Zep/Graphiti** keeps bi-temporal edges and *invalidates* contradicted facts instead of deleting them. [arXiv 2501.13956](https://arxiv.org/abs/2501.13956)
- **HaluMem:** memory systems generate and accumulate hallucinations in **extraction and updating**, and errors propagate to answers. The authors call for "interpretable and constrained memory operation mechanisms". [arXiv 2511.03506](https://arxiv.org/abs/2511.03506)
- **Ground Truth First:** write-stage quality strongly predicts downstream quality (weakly written facts fail 24% vs 2%). A budgeted (capped) curated memory loses evicted facts over time (96% → 72% recall at 9 weeks). [arXiv 2607.21962](https://arxiv.org/abs/2607.21962)
- **EverMemBench:** temporal reasoning needs "explicit version semantics beyond timestamps". [arXiv 2602.01313](https://arxiv.org/abs/2602.01313)
- **MemoryAgentBench:** selective forgetting / multi-hop conflict resolution is the hardest competency (≤28% for all methods; Mem0 2%). [arXiv HTML 2507.05257](https://arxiv.org/html/2507.05257v4)

**Deprecation, decay and forgetting**
- **Memp** keeps procedural memory with continuous update, correction and **deprecation** of outdated procedures. [arXiv 2508.06433](https://arxiv.org/abs/2508.06433)
- **MemoryBank** uses an Ebbinghaus forgetting curve with reinforcement on recall. [arXiv 2305.10250](https://arxiv.org/abs/2305.10250)
- **Generative Agents** use exponential recency decay (0.995 per hour) in ranking. [arXiv HTML 2304.03442](https://arxiv.org/html/2304.03442v2)
- Mem0's 2026 platform adds "Memory Decay" as a platform-only feature. No public algorithm details were found. [Mem0 blog](https://mem0.ai/blog/mem0-the-token-efficient-memory-algorithm)

**Deduplication and evolution**
- **A-MEM** links each new note to its neighbors and can rewrite neighbors' descriptions ("memory evolution"). [arXiv 2502.12110](https://arxiv.org/abs/2502.12110)
- **ACE** prevents redundancy and collapse with structured, incremental updates instead of monolithic rewrites. [arXiv 2510.04618](https://arxiv.org/abs/2510.04618)
- **REALM** reconsolidates memories based on *retrieval feedback*: memories that are retrieved together get reorganized into coherent local structures. [arXiv 2609.16053](https://arxiv.org/abs/2609.16053)

**Governance, user control and privacy**
- **SSGM (Stability and Safety Governed Memory).** Lam et al., Mar 2026. A conceptual framework that decouples memory evolution from execution, enforcing **consistency verification, temporal decay modeling, and dynamic access control before any consolidation**. Targets semantic drift from iterative summarization and "topology-induced knowledge leakage". [arXiv 2603.11768](https://arxiv.org/abs/2603.11768)
- **Mi-Memory** has explicit support for correction and forgetting, typed evidence with provenance, and gate/rollback records for accepted memory-policy changes. [arXiv 2607.18975](https://arxiv.org/abs/2607.18975)
- **Anthropic's memory tool** runs client-side; developers control where and how memory data persists. [Claude blog](https://claude.com/blog/context-management)
- **MIRIX** ships with local storage "to ensure privacy". [arXiv 2507.07957](https://arxiv.org/abs/2507.07957)
- **MEXTRA (Unveiling Privacy Risks in LLM Agent Memory).** ACL 2025. Black-box prompts can extract private user–agent interactions stored as memory demonstrations. [arXiv 2502.13172](https://arxiv.org/abs/2502.13172)

### Inferences

**Write pipeline for Xylo**
- **Online capture (always, zero LLM):** persist full transcripts, tool calls and outcomes to SQLite with FTS5. This is MemPalace's verbatim-first lesson and is safe for small local models.
- **End-of-session or idle consolidation ("sleep-time"):** when the app is idle, or using a cheaper or local model, run extraction:
  - candidate project facts (with source message IDs);
  - lessons (success or failure);
  - candidate workflows;
  - updates to the pinned core block.
  - Desktop apps have natural idle time, which suits Letta and LightMem's offline consolidation.
- **Non-destructive conflict handling:** facts get `valid_from` / `invalid_at` / `superseded_by`. On conflict, insert the new fact and mark the old one superseded, with an inline "previously: X (until date)" shown at retrieval. Never hard-delete automatically; user deletions should be real deletions for privacy.
- **Write quality gate:**
  - require each extracted fact to be self-contained (no pronouns), dated, attributed (user vs agent vs tool output) and linked to its source;
  - reject or flag low-confidence extractions;
  - optional: a second-pass verifier checks the fact against the source turn (targets HaluMem's failure stage).
- **Dedup:** normalize, check FTS and embedding similarity above a threshold, and increment `seen_count` instead of inserting duplicates (ExpeL-style upvote).
- **Decay:** use `last_accessed_at`, `access_count` and an importance score in ranking only. Surface "stale" facts (unaccessed for N weeks, or contradicted) in a review UI instead of silently forgetting them.
- **User control:** every memory is visible, editable and deletable, with its provenance and the session it came from. Add an "incognito/no-memory" session toggle and per-project scoping. Never store secrets; detect and redact keys and tokens before writing.

### Gaps
- No public, measured comparison of online-per-turn versus end-of-session-batch extraction on the same benchmark (accuracy and cost) was found. LightMem and sleep-time compute are the closest.
- The details of Mem0's "Memory Decay" and of how ADD-only conflicts are resolved at retrieval time are not public.
- ChatGPT's memory controls and the privacy design of OpenAI/Google memory products were not verified from primary sources in this pass.

---

## Key Question 5: Memory retrieval. Embedding vs BM25/FTS vs hybrid vs graph; agent-driven retrieval (memory as tools) vs automatic injection; how many memories to inject; cost for small local models. Is embedding search worth it compared with simple file-based memory (CLAUDE.md-style, Anthropic's memory tool)?

### Takeaway
Lexical search is a surprisingly strong baseline: BM25 beats embedding RAG and most structured memory layers on MemoryAgentBench. With capable models, an agent that iteratively searches plain files (grep) beats single-shot retrieval (Letta 74% on LoCoMo; LongMemEval-V2 72.5% vs 48.5% for RAG), at a latency cost.

Hybrid ranking (BM25 + embeddings + entity match), as in Mem0 2026 and Zep, is the production consensus. Small local models have three specific weaknesses:
- they degrade with too much injected memory (Llama 3.1 8B beyond ~3k retrieved tokens);
- they are poor at query rewriting;
- agentic memory loses more from a weak backbone.

So for local models, automatic injection of a few high-precision items beats tool-driven retrieval. Embeddings are worth adding as one signal in a hybrid, not as the only retriever.

### Cited Findings

**Lexical vs embedding vs structured retrieval**
- **MemoryAgentBench accurate-retrieval scores:**

  | Retriever / system | AR score |
  |---|---|
  | **BM25** | **60.5** |
  | Qwen3-Embedding-4B | 54.7 |
  | text-embedding-3-large | 54.6 |
  | text-embedding-3-small | 53.8 |
  | Contriever | 33.9 |
  | Zep | 37.5 |
  | MemGPT | 34.3 |
  | Mem0 | 32.6 |

  Overall: BM25 41.5, nearly tied with HippoRAG-v2 at 41.6, the best non-long-context system. [arXiv HTML 2507.05257](https://arxiv.org/html/2507.05257v4)
- **Letta filesystem agent:** 74.0% on LoCoMo with grep, search_files, open and close (GPT-4o-mini), versus Mem0's 68.5%. Letta argues "whether an agent remembers" depends mainly on whether it knows *when and how* to use a retrieval tool, and that simpler tools are used better. [Letta blog](https://www.letta.com/blog/benchmarking-ai-agent-memory)
- **LongMemEval-V2:** trajectories stored as files plus a coding agent (AgentRunbook-C) scored 72.5%, versus 48.5% for the best RAG and 69.3% for an off-the-shelf coding agent, but with "high latency costs". [arXiv 2605.12493](https://arxiv.org/abs/2605.12493)
- **MemPalace:** 96.6% Recall@5 on LongMemEval comes from verbatim storage plus a small default embedding model (all-MiniLM-L6-v2), with zero LLM calls at write time and ~170-token wake-up context. [arXiv 2604.21284](https://arxiv.org/abs/2604.21284)

**Index design (LongMemEval)**
- Round-level (per-turn) values help GPT-4o reading.
- **Fact-augmented key expansion:** indexing each value under its extracted facts as well as its raw text gives **+9.4% recall@k and +5.4% accuracy** on average.
- **Time-aware query expansion:** +11.3% (rounds) / +6.8% (sessions) recall on temporal questions.
- **Chain-of-Note + JSON-formatted retrieved items** is the best reading strategy. A poor reading strategy costs up to 10 points even with oracle retrieval.
- [arXiv HTML 2410.10813](https://arxiv.org/html/2410.10813v2)

**Small models and injection budget**
- **LongMemEval, small-model specifics:**
  - Llama 3.1 8B "struggles to produce accurate time ranges", so time-aware query expansion did not help it (recall 0.481 vs 0.489 baseline).
  - Its accuracy **drops sharply beyond ~3k retrieved tokens**, while GPT-4o keeps improving past 20k.
  - With good round-level retrieval, Llama 8B reached 0.600 Top-5 QA versus 0.615 for GPT-4o.
  - Chain-of-Note *lowered* Llama 8B's score.
  - [arXiv HTML 2410.10813](https://arxiv.org/html/2410.10813v2)
- **MemoryAgentBench:** the backbone matters more for agentic memory than for RAG (MIRIX 26.2 → 37.7 when upgraded from GPT-4o-mini to GPT-4.1-mini; RAG gains only marginally). [arXiv HTML 2507.05257](https://arxiv.org/html/2507.05257v4)
- **How many items to inject:**
  - ReasoningBank defaults to **top-1** memory item [arXiv HTML 2509.25140](https://arxiv.org/html/2509.25140v2);
  - A-MEM uses k=10 for small models and 40–50 for GPT models [arXiv HTML 2502.12110](https://arxiv.org/html/2502.12110v11);
  - Mem0 2026 uses ~6.7–7k tokens per query in a single retrieval pass, versus 25k+ for full context [Mem0 blog](https://mem0.ai/blog/mem0-the-token-efficient-memory-algorithm);
  - Zep uses ~1.6k tokens per retrieval on LongMemEval, per Mem0 [Mem0 comparison](https://mem0.ai/blog/zep-vs-mem0-which-ai-memory-layer-should-you-choose).

**Hybrid and graph retrieval**
- **Mem0 2026:** semantic, keyword and entity scoring run in parallel and are fused. Entities (proper nouns, quoted text, compound noun phrases) boost matching memories. [Mem0 blog](https://mem0.ai/blog/mem0-the-token-efficient-memory-algorithm)
- **Graph retrieval:** HippoRAG's Personalized PageRank is 10–30× cheaper than iterative retrieval for multi-hop QA [arXiv 2405.14831](https://arxiv.org/abs/2405.14831). But KG-augmented RAG often underperforms standard RAG on basic factual recall [arXiv 2502.14802](https://arxiv.org/abs/2502.14802). In MemoryAgentBench, graph systems (Zep 37.5, GraphRAG 40.9, Cognee 28.3) trail BM25 on AR [arXiv HTML 2507.05257](https://arxiv.org/html/2507.05257v4).

**Skills and pinned context**
- **Skill and procedure retrieval:** actual-use precision falls from 29.6% to 3.3% as the pool grows from 5 to 100 skills [arXiv 2608.14036](https://arxiv.org/abs/2608.14036). Anthropic's progressive disclosure keeps only name and description resident and loads bodies on demand [Anthropic engineering](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills).
- **Pinned file memory:** Anthropic's file-based memory tool plus context editing gave +39% on an internal agentic-search eval [Claude blog](https://claude.com/blog/context-management). In Ground Truth First, a capped curated-map memory leads early but loses evicted content (72% recall at 9 weeks), while a provenance-typed graph reached 90% [arXiv 2607.21962](https://arxiv.org/abs/2607.21962).

### Inferences

**Recommended retrieval stack for Xylo (SQLite-native)**
1. **Tier 0, always injected:** a pinned per-project "core memory" file of ≤~1–2k tokens (CLAUDE.md / AGENTS.md style, which Xylo can import from repo files), plus a user-profile block. Both are editable by user and agent.
2. **Tier 1, automatic injection:** for each user turn, run FTS5 BM25 + entity match (file paths, symbols, package names) + an optional embedding search. Fuse with reciprocal-rank fusion, weight by recency and importance, and inject the **top 3–5 items, capped at ~1–2k tokens for local models**. Format as dated, attributed JSON-like bullets (LongMemEval's best reading format).
3. **Tier 2, memory as tools (cloud or strong models only):** `search_memory`, `search_sessions` and `read_session` tools (grep/FTS over past transcripts, like Letta and LongMemEval-V2) for iterative search when the automatic injection isn't enough.
- **Is embedding search worth it?** For Xylo, yes as an *optional* hybrid signal: it helps paraphrase matches, and a small local embedding model (MiniLM-class) is cheap. But BM25/FTS5 alone is a credible baseline that needs no model, and should be the default fallback. A graph store is not justified for typical project memory. Entity tables plus temporal validity give most of the benefit.
- **Small local models:**
  - keep injected memory small and high-precision;
  - avoid relying on the model for query rewriting or time-range extraction; do these deterministically in the harness (date parsing, entity extraction via regex or tree-sitter for code symbols);
  - prefer automatic injection over tool-driven retrieval;
  - offload consolidation to a stronger model if the user allows cloud, or to idle-time batch runs.
- **Index facts by their extracted keys plus raw text** (fact-augmented key expansion), storing extracted facts as extra FTS rows that point back to the source turn.

### Gaps
- I found no published head-to-head of SQLite FTS5 versus embedding hybrids specifically for coding-agent memory.
- I found no controlled study of automatic injection versus tool-driven retrieval *with small local models* (≤8B). The recommendation above is inferred from the LongMemEval and MemoryAgentBench backbone findings.
- Letta's filesystem result has not been re-run against Mem0's 2026 numbers. (A Towards AI commentary notes this; it is opinion and paywalled, so not used.)

---

## Key Question 6: Memory poisoning and security risks of persistent memory (MINJA, AgentPoison and successors) and mitigations.

### Takeaway
Persistent memory turns one-time prompt injection into a durable, cross-session compromise. Several attacks work through normal query-only interaction:
- MINJA: >95% injection success;
- GhostWriter: ~98% injection, ~60% activation;
- MemPoison bypasses selective extraction pipelines (ASR up to 0.95);
- FARMA forges remembered reasoning (up to 100%).

Content-based filtering and consensus defenses can be bypassed, including by "laundering" through the agent's own summarization. The strongest 2026 results come from **write-time provenance / origin binding**, meaning memories derived from untrusted content can never gain instruction-level authority:
- HMAC-signed provenance cuts unsigned injection from 93–100% to 0%;
- origin-bound information-flow control gives 0% on direct and laundering attacks.

Privacy extraction (MEXTRA) is a separate risk.

### Cited Findings

**Attacks**
- **MINJA (Memory Injection Attacks on LLM Agents via Query-Only Interaction).** Dong et al., Mar 2025.
  - The attacker only queries the agent. Bridging steps plus an "indication prompt" that is progressively shortened make the agent itself write malicious records, which are later retrieved for the victim's queries.
  - [arXiv 2503.03704](https://arxiv.org/abs/2503.03704)
- MINJA's reported **>95% injection success and 70% attack success** hold under idealized conditions. A 2026 re-evaluation on EHR agents (GPT-4o-mini, Gemini-2.0-Flash, **Llama-3.1-8B**) found that **pre-existing legitimate memories dramatically reduce attack effectiveness**. Its defenses (composite trust scoring for input/output moderation; memory sanitization with trust-aware retrieval using temporal decay and pattern filtering) need careful threshold calibration. [arXiv 2601.05504](https://arxiv.org/abs/2601.05504)
- **AgentPoison.** Chen, Xiang, Xiao, Song, Li, NeurIPS 2024.
  - Optimized backdoor triggers map triggered queries to poisoned memory and RAG entries.
  - **>80% average attack success with <0.1% poison rate and <1% benign degradation**, on driving, QA and EHR agents.
  - [arXiv 2407.12784](https://arxiv.org/abs/2407.12784)
- **GhostWriter / AM-Sentry (When Agents Remember Too Much).** Jul 2026.
  - Targets tool-using personal agents (email, calendar, code push).
  - **~98% injection rate, ~60% activation rate**.
  - Defense AM-Sentry combines a **memory-saving policy** and a **memory-retrieval screen**, and "dramatically reduces" success while preserving utility.
  - [arXiv 2607.06595](https://arxiv.org/abs/2607.06595)
- **MemPoison.** ACM CCS 2026.
  - Bypasses *selective* extraction and rewriting pipelines (Mem0-style) through:
    - a semantic bridge binding trigger and payload;
    - entity masquerading;
    - embedding-cluster optimization.
  - ASR up to 0.95. Tested defenses show "fundamental limitations".
  - [arXiv 2605.29960](https://arxiv.org/abs/2605.29960)
- **FARMA / SENTINEL (Forged Reasoning Attacks on LLM Agent Memory).** Jul 2026.
  - Poisons *remembered reasoning traces*. Up to 100% ASR; defeats keyword filters and A-MemGuard.
  - SENTINEL's "Reasoning Guard" (five weighted structural signals) cuts ASR to as low as 0% with no false positives on 326 benign traces.
  - [arXiv 2607.05029](https://arxiv.org/abs/2607.05029)
- **MEXTRA (Unveiling Privacy Risks in LLM Agent Memory).** ACL 2025. Black-box memory extraction attacks recover private past interactions stored in memory. [arXiv 2502.13172](https://arxiv.org/abs/2502.13172)

**Defenses**
- **A-MemGuard.** Sep 2025. Consensus validation across reasoning paths from multiple related memories, plus a separate "lessons" memory of detected failures. Cuts attack success by **>95%** with minimal utility cost, but FARMA later bypassed it. [arXiv 2510.02373](https://arxiv.org/abs/2510.02373); [arXiv 2607.05029](https://arxiv.org/abs/2607.05029)
- **SMSR (Signed Memory with Smoothed Retrieval).** Jun 2026.
  - Component 1: **HMAC-SHA256 provenance at write time**. Cuts unsigned injection success from **93–100% to 0%**.
  - Component 2: randomized memory ablation with verdict-based majority voting. Holds an authenticated single-injection adversary to 8.0%.
  - End-to-end query-only attack (agent writes the poison itself): **65.3% → 5.3%**.
  - Clean utility: 90% for Component 1, 85% combined.
  - Proves that no provenance-free retrieval-time filter can certify against adaptive injection.
  - [arXiv 2606.12703](https://arxiv.org/abs/2606.12703)
- **TMA-NM (Non-Malleable, Origin-Bound Authority).** Jun 2026.
  - Shows that content-based and lineage-based defenses can be **laundered** through three channels: the agent's own summarization, a trusted-tool echo, and manufactured corroboration.
  - Existing defenses reach up to 68% laundering ASR.
  - Proves write-time origin binding is necessary. Its construction reaches **0% ASR across eight frontier models at full utility**.
  - Threat model: untrusted content stored in one session later steers a payment, a settings change or exfiltration.
  - [arXiv 2606.24322](https://arxiv.org/abs/2606.24322)
- **Ground Truth First:** "injection resistance tracked whether provenance boundaries survive representation". Its benchmark includes sent/received trust distinctions and injection probes. [arXiv 2607.21962](https://arxiv.org/abs/2607.21962)
- **SSGM:** consistency verification, temporal decay and dynamic access control *before* consolidation. Warns that sensitive contexts get "solidified into long-term storage". [arXiv 2603.11768](https://arxiv.org/abs/2603.11768)
- **Anthropic Skills guidance:** skills are an attack surface. Install only from trusted sources; audit scripts, dependencies and instructions that reach out to external networks. [Anthropic engineering](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills)

### Inferences

**Security design for Xylo memory**
- **Origin binding at write time** (the most important measure): every memory row stores an immutable `origin` set by the harness, never by the model:
  - `user_typed`
  - `user_approved`
  - `agent_inferred_from_user`
  - `tool_output`
  - `web/file_content`
  - `imported_skill`

  The origin of derived memories is the *least trusted* of their sources, so summarization cannot launder it (TMA-NM). Optionally sign rows with a local HMAC key held in the OS keychain to detect tampering by other processes or synced files (SMSR Component 1).
- **Authority separation:** only `user_typed` / `user_approved` memories and reviewed skills may be injected as *instructions*. Everything else is injected as quoted, dated *data* ("Previously observed: …"), never placed in the system-instruction region.
- **Skills from consolidation stay quarantined** until the user approves them. Xylo's existing "review reusable skills" UI is the natural gate. Show diffs for edits to existing skills.
- **Memory-saving policy (AM-Sentry style):** don't auto-save facts extracted from tool outputs, web pages or fetched files that contain imperative language aimed at the agent. Flag them for review. Never store credentials, tokens or keys.
- **Retrieval screen:** for consequential actions (shell commands, network calls, file deletion), log which memories influenced the decision and require user confirmation when a memory of untrusted origin is among them.
- **Privacy (MEXTRA):**
  - scope memory per project/workspace;
  - don't retrieve cross-project memories by default;
  - support deletion that truly removes rows and their FTS/embedding index entries;
  - keep everything local by default;
  - make it clear when memory content is sent to cloud models.
- **Small-model note:** small local models are likely *more* susceptible to following injected memory content and weaker as LLM-based verifiers. This is inferred; the 2026 EHR study included Llama-3.1-8B but I did not extract per-model results. So harness-level, deterministic provenance controls matter more than model-based screening.

### Gaps
- Per-model attack-success breakdowns (especially for small local models) from the MINJA re-evaluation and the GhostWriter paper were not extracted.
- I found no published attacks or defenses specifically targeting *file-based* memories (CLAUDE.md / AGENTS.md / SKILL.md) in coding harnesses, apart from Anthropic's trust guidance and the skill-authoring paper's "trust question".
- Most 2026 attack and defense papers are preprints (MemPoison is CCS 2026), evaluated on limited agent stacks; certified guarantees (SMSR, TMA-NM) come from single-author preprints without independent replication.
