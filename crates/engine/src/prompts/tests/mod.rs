use std::path::{Path, PathBuf};

use xylo_core::PermissionMode;

use super::instructions::INSTRUCTION_BYTES;
use super::render::{RenderError, placeholders, render};
use super::skills::{RAW_SKILLS, RawSkill, SKILLS_INDEX_BYTES, parse};
use super::*;

fn sample(workspace: &Path) -> PromptContext<'_> {
    PromptContext {
        workspace,
        os: "macos",
        arch: "aarch64",
        shell: "bash (falls back to sh)",
        git: Some(GitInfo {
            branch: Some("main".to_owned()),
        }),
        date: "2026-10-31".to_owned(),
        permission_mode: PermissionMode::AskForApproval,
        project_instructions: String::new(),
        skills_index: String::new(),
        facts_index: String::new(),
    }
}

#[test]
fn renderer_substitutes_values_once() {
    let rendered = render("a {{x}} b {{y}} {{x}}", &[("x", "{{y}}"), ("y", "2")]).unwrap();
    assert_eq!(rendered, "a {{y}} b 2 {{y}}");
}

#[test]
fn renderer_rejects_placeholder_without_value() {
    assert_eq!(
        render("hello {{name}}", &[]),
        Err(RenderError::MissingValue("name".to_owned()))
    );
}

#[test]
fn renderer_rejects_value_the_template_does_not_use() {
    assert_eq!(
        render("hello", &[("name", "x")]),
        Err(RenderError::UnusedValue("name".to_owned()))
    );
}

#[test]
fn renderer_rejects_malformed_placeholders() {
    for template in ["a {{ name }}", "a {{Name}}", "a {{name", "a {{}}"] {
        assert!(
            matches!(
                render(template, &[("name", "x")]),
                Err(RenderError::Malformed(2))
            ),
            "{template}"
        );
    }
}

/// Every bundled template declares exactly the placeholders its caller fills.
#[test]
fn bundled_templates_use_exactly_their_placeholders() {
    let expected: &[(&str, &[&str])] = &[
        (SYSTEM, &["environment", "permissions"]),
        (
            ENVIRONMENT,
            &["workspace", "os", "arch", "shell", "git", "date"],
        ),
        (PERMISSIONS, &["permission_mode", "permission_policy"]),
        (PERMISSION_CHANGE, &["permission_mode", "permission_policy"]),
        (SYSTEM_PROJECT_INSTRUCTIONS, &["instructions"]),
        (SYSTEM_SKILLS, &["index"]),
        (SYSTEM_FACTS, &["index"]),
        (PERMISSION_ASK_FOR_APPROVAL, &[]),
        (PERMISSION_APPROVE_FOR_ME, &[]),
        (PERMISSION_FULL_ACCESS, &[]),
        (PERMISSION_READ_ONLY, &[]),
        (COMPACTION, &[]),
        (COMPACTION_REQUEST, &["transcript"]),
        (COMPACTION_UPDATE, &["previous", "transcript"]),
        (COMPACTION_SUMMARY, &["summary"]),
        (TITLE, &[]),
        (SUBAGENT, &["environment", "permissions"]),
        (SUBAGENT_TASK, &["task"]),
        (SKILL_PERMISSIONS_NOTE, &[]),
        (FORKED_SKILL_CONTEXT, &["skill", "package_root"]),
        (APPLICATION_NOTE, &["text"]),
        (STEP_LIMIT, &[]),
        (COMPACTION_EXCERPTS, &[]),
        (COMPACTION_TOOL_NOTES, &[]),
    ];
    for (template, names) in expected {
        let mut found = placeholders(template);
        found.sort_unstable();
        let mut names = names.to_vec();
        names.sort_unstable();
        assert_eq!(found, names, "{template}");
        let values: Vec<(&str, &str)> = names.iter().map(|name| (*name, "value")).collect();
        render(template, &values).unwrap();
    }
}

#[test]
fn system_prompt_is_deterministic_and_complete() {
    let workspace = PathBuf::from("/work/project");
    let mut ctx = sample(&workspace);
    ctx.project_instructions = "## AGENTS.md\nUse tabs.".to_owned();
    ctx.skills_index = skills_index([("code-review", "Review a diff.")]);
    ctx.facts_index = facts_index(["Tests run with cargo nextest"]);
    let first = system(&ctx);
    assert_eq!(first, system(&ctx.clone()));
    for expected in [
        "Workspace: /work/project",
        "Git repository, branch main",
        "Today (UTC): 2026-10-31",
        "# Permissions: Ask for approval",
        "# Project instructions",
        "Use tabs.",
        "- code-review: Review a diff.",
        "- Tests run with cargo nextest",
    ] {
        assert!(first.contains(expected), "missing {expected:?}");
    }
    assert!(!first.contains("{{"));
}

#[test]
fn system_prompt_omits_empty_sections() {
    let workspace = PathBuf::from("/work/project");
    let prompt = system(&sample(&workspace));
    assert!(!prompt.contains("# Project instructions"));
    assert!(!prompt.contains("# Skills"));
    assert!(!prompt.contains("# Retained project facts"));
}

#[test]
fn system_prompt_states_each_permission_mode() {
    let workspace = PathBuf::from("/work/project");
    let mut prompts = Vec::new();
    for mode in PermissionMode::ALL {
        let mut ctx = sample(&workspace);
        ctx.permission_mode = mode;
        let prompt = system(&ctx);
        assert!(prompt.contains(&format!("# Permissions: {}", mode.label())));
        prompts.push(prompt);
    }
    prompts.dedup();
    assert_eq!(prompts.len(), PermissionMode::ALL.len());
}

/// The fixed prompt (everything except project instructions) is paid on
/// every model call; keep it small. See prompts/README.md for the budget.
#[test]
fn fixed_system_prompt_stays_within_budget() {
    const FIXED_BUDGET: usize = 12 * 1024;
    let workspace = PathBuf::from("/Users/someone/projects/a-fairly-long-project-name");
    let mut ctx = sample(&workspace);
    ctx.skills_index = skills_index(
        bundled_skills()
            .iter()
            .map(|skill| (skill.name, skill.description)),
    );
    ctx.facts_index = facts_index(std::iter::repeat_n(
        "A retained fact title of typical length",
        24,
    ));
    for mode in PermissionMode::ALL {
        ctx.permission_mode = mode;
        let size = system(&ctx).len();
        assert!(size <= FIXED_BUDGET, "{mode:?}: {size} bytes");
    }
}

#[test]
fn git_line_distinguishes_detached_and_missing_repositories() {
    let workspace = PathBuf::from("/work/project");
    let mut ctx = sample(&workspace);
    ctx.git = Some(GitInfo { branch: None });
    assert!(system(&ctx).contains("Git repository (detached HEAD or unknown branch)"));
    ctx.git = None;
    assert!(system(&ctx).contains("Version control: not a Git repository"));
}

#[test]
fn dates_are_civil_utc() {
    assert_eq!(utc_date(0), "1970-01-01");
    assert_eq!(utc_date(1_761_436_800_000), "2025-10-26");
    assert_eq!(utc_date(951_782_400_000), "2000-02-29");
    assert_eq!(utc_date(-1), "1969-12-31");
}

#[test]
fn context_detects_repository_and_branch() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".git")).unwrap();
    std::fs::write(root.path().join(".git/HEAD"), "ref: refs/heads/feature\n").unwrap();
    let nested = root.path().join("crates/app");
    std::fs::create_dir_all(&nested).unwrap();
    let ctx = PromptContext::detect(&nested, PermissionMode::ReadOnly, 0);
    assert_eq!(
        ctx.git,
        Some(GitInfo {
            branch: Some("feature".to_owned())
        })
    );
    assert_eq!(ctx.date, "1970-01-01");
}

#[test]
fn worktree_pointer_resolves_branch() {
    let root = tempfile::tempdir().unwrap();
    let git_dir = root.path().join("real-git");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
    let checkout = root.path().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::write(checkout.join(".git"), "gitdir: ../real-git\n").unwrap();
    assert_eq!(
        GitInfo::detect(&checkout),
        Some(GitInfo {
            branch: Some("topic".to_owned())
        })
    );
}

fn repository() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".git")).unwrap();
    let nested = root.path().join("crates/app");
    std::fs::create_dir_all(&nested).unwrap();
    (root, nested)
}

#[test]
fn instructions_run_from_repository_root_to_workspace() {
    let (root, nested) = repository();
    std::fs::write(root.path().join("AGENTS.md"), "Use tabs.").unwrap();
    std::fs::write(root.path().join("CLAUDE.md"), "Root Claude notes.").unwrap();
    std::fs::write(root.path().join("crates/AGENTS.md"), "Crate rules.").unwrap();
    std::fs::write(nested.join("CLAUDE.md"), "Run cargo test.").unwrap();
    let text = project_instructions(&nested);
    let order: Vec<usize> = [
        "## AGENTS.md\nUse tabs.",
        "## CLAUDE.md\nRoot Claude notes.",
        "## crates/AGENTS.md\nCrate rules.",
        "## crates/app/CLAUDE.md\nRun cargo test.",
    ]
    .iter()
    .map(|needle| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle} in {text}"))
    })
    .collect();
    assert!(order.is_sorted(), "{text}");
}

#[test]
fn instructions_ignore_directories_above_the_repository() {
    let outer = tempfile::tempdir().unwrap();
    std::fs::write(outer.path().join("AGENTS.md"), "Outside rules.").unwrap();
    let root = outer.path().join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("AGENTS.md"), "Inside rules.").unwrap();
    let text = project_instructions(&root);
    assert!(text.contains("Inside rules."));
    assert!(!text.contains("Outside rules."));
}

#[test]
fn instructions_outside_a_repository_use_only_the_workspace() {
    let outer = tempfile::tempdir().unwrap();
    std::fs::write(outer.path().join("AGENTS.md"), "Parent rules.").unwrap();
    let workspace = outer.path().join("scratch");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("AGENTS.md"), "Scratch rules.").unwrap();
    assert_eq!(
        project_instructions(&workspace),
        "## AGENTS.md\nScratch rules."
    );
}

#[test]
fn instructions_skip_empty_and_duplicate_files() {
    let (root, nested) = repository();
    std::fs::write(root.path().join("AGENTS.md"), "Shared rules.\n").unwrap();
    std::fs::write(root.path().join("CLAUDE.md"), "Shared rules.").unwrap();
    std::fs::write(nested.join("AGENTS.md"), "  \n").unwrap();
    assert_eq!(project_instructions(&nested), "## AGENTS.md\nShared rules.");
}

#[test]
fn instruction_budget_keeps_the_nearest_files() {
    let (root, nested) = repository();
    std::fs::write(root.path().join("AGENTS.md"), "outer ".repeat(3000)).unwrap();
    std::fs::write(root.path().join("crates/AGENTS.md"), "middle ".repeat(3000)).unwrap();
    std::fs::write(
        nested.join("AGENTS.md"),
        format!("{}NEAREST-END", "near ".repeat(1000)),
    )
    .unwrap();
    let text = project_instructions(&nested);
    assert!(
        text.len() <= INSTRUCTION_BYTES + 128,
        "{} bytes",
        text.len()
    );
    assert!(
        text.contains("NEAREST-END"),
        "nearest file must be complete"
    );
    assert!(text.contains("[… truncated]") || text.contains("omitted"));
    assert!(text.find("middle").unwrap() < text.find("NEAREST-END").unwrap());
}

#[test]
fn oversized_instruction_file_is_truncated_on_a_char_boundary() {
    let (_root, nested) = repository();
    std::fs::write(nested.join("AGENTS.md"), "é".repeat(INSTRUCTION_BYTES)).unwrap();
    let text = project_instructions(&nested);
    assert!(text.len() <= INSTRUCTION_BYTES);
    assert!(text.ends_with("[… truncated]"));
}

#[test]
fn skills_index_is_one_bounded_line_per_skill() {
    let long = "word ".repeat(200);
    let index = skills_index([("a", "Does a.\nUse when a."), ("b", long.as_str())]);
    let lines: Vec<&str> = index.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "- a: Does a. Use when a.");
    assert!(lines[1].ends_with('…'));
    assert!(lines[1].chars().count() < 420);
    assert_eq!(skills_index(std::iter::empty()), "");
}

#[test]
fn skills_index_shortens_then_counts_when_over_budget() {
    let description = "Detailed description of when to use this skill. ".repeat(6);
    let names: Vec<String> = (0..40).map(|n| format!("skill-{n:02}")).collect();
    let shortened = skills_index(
        names
            .iter()
            .map(|name| (name.as_str(), description.as_str())),
    );
    assert!(shortened.len() <= SKILLS_INDEX_BYTES);
    assert_eq!(
        shortened.lines().count(),
        40,
        "shortening alone fits 40 skills"
    );
    assert!(shortened.lines().all(|line| line.chars().count() < 130));
    let names: Vec<String> = (0..200).map(|n| format!("skill-{n:03}")).collect();
    let counted = skills_index(
        names
            .iter()
            .map(|name| (name.as_str(), description.as_str())),
    );
    assert!(counted.len() <= SKILLS_INDEX_BYTES);
    let last = counted.lines().last().unwrap();
    assert!(
        last.starts_with("- (") && last.ends_with("more skills not listed)"),
        "{last}"
    );
}

#[test]
fn bundled_skill_descriptions_fit_the_index_untruncated() {
    for skill in bundled_skills() {
        assert!(
            !skills_index([(skill.name, skill.description)]).ends_with('…'),
            "{}",
            skill.name
        );
    }
}

#[test]
fn facts_index_lists_bounded_titles() {
    let titles: Vec<String> = (0..40).map(|n| format!("Fact {n}")).collect();
    let index = facts_index(titles.iter().map(String::as_str));
    assert_eq!(index.lines().count(), 24);
    assert!(index.starts_with("- Fact 0\n- Fact 1"));
    assert_eq!(facts_index(["", "  "]), "");
}

#[test]
fn every_bundled_skill_parses() {
    assert_eq!(bundled_skills().len(), RAW_SKILLS.len());
    for raw in RAW_SKILLS {
        parse(raw).unwrap_or_else(|error| panic!("{}: {error}", raw.folder));
    }
    let names: Vec<&str> = bundled_skills().iter().map(|skill| skill.name).collect();
    assert!(names.is_sorted(), "keep RAW_SKILLS in name order");
}

#[test]
fn bundled_skills_follow_the_agent_skills_format() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills");
    let mut folders: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    folders.sort();
    let names: Vec<&str> = bundled_skills().iter().map(|skill| skill.name).collect();
    assert_eq!(folders, names, "every skill folder must be registered");
    for skill in bundled_skills() {
        assert!(!skill.description.is_empty());
        assert!(
            skill.body.lines().count() <= 400,
            "{} body too long",
            skill.name
        );
        // Every file in the folder is bundled, and every bundled file is
        // referenced from the body so the model can find it on demand.
        let mut files = Vec::new();
        collect_files(&root.join(skill.name), &root.join(skill.name), &mut files);
        files.retain(|file| file != "SKILL.md");
        files.sort();
        let mut bundled: Vec<String> = skill.resources.iter().map(|r| r.path.to_owned()).collect();
        bundled.sort();
        assert_eq!(files, bundled, "{} resources", skill.name);
        for resource in skill.resources {
            assert!(
                skill.body.contains(resource.path),
                "{} must reference {}",
                skill.name,
                resource.path
            );
            assert_eq!(skill.resource(resource.path), Some(resource));
        }
    }
}

#[test]
fn bundled_skills_name_only_real_tools() {
    // Tool names the skills may mention; keep in step with the tool broker.
    const TOOLS: &[&str] = &[
        "agent_delegate",
        "file_read",
        "file_edit",
        "file_write",
        "files_list",
        "find_files",
        "search_project",
        "git_status",
        "git_diff",
        "run_command",
        "shell_command",
        "todo_write",
        "memory_search",
        "memory_remember",
        "skill_list",
        "skill_read",
        "extension_skill",
        "extension_resource",
    ];
    for skill in bundled_skills() {
        for tool in &skill.allowed_tools {
            assert!(
                TOOLS.contains(tool),
                "{}: unknown allowed tool {tool}",
                skill.name
            );
        }
        let texts = std::iter::once(skill.body).chain(skill.resources.iter().map(|r| r.content));
        for text in texts {
            for word in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
                let looks_like_tool = word.contains('_')
                    && word.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                    && TOOL_PREFIXES.iter().any(|prefix| word.starts_with(prefix));
                assert!(
                    !looks_like_tool || TOOLS.contains(&word),
                    "{}: unknown tool {word}",
                    skill.name
                );
            }
        }
    }
}

const TOOL_PREFIXES: &[&str] = &[
    "agent_",
    "file_",
    "files_",
    "find_",
    "search_",
    "git_",
    "run_",
    "shell_",
    "todo_",
    "memory_",
    "skill_",
    "extension_",
];

fn collect_files(base: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(base, &path, out);
        } else {
            out.push(
                path.strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}

#[test]
fn bundled_skill_lookup_by_name() {
    assert_eq!(
        bundled_skill("code-review").map(|skill| skill.name),
        Some("code-review")
    );
    assert!(bundled_skill("missing").is_none());
}

#[test]
fn skill_parser_rejects_bad_frontmatter() {
    let cases = [
        ("x", "no frontmatter"),
        ("x", "---\ndescription: d\n---\nbody"),
        ("x", "---\nname: x\n---\nbody"),
        ("x", "---\nname: y\ndescription: d\n---\nbody"),
        ("X", "---\nname: X\ndescription: d\n---\nbody"),
        ("x", "---\nname: x\ndescription: d\nversion: 2\n---\nbody"),
        ("x", "---\nname: x\ndescription: d\n---\n  \n"),
    ];
    for (folder, source) in cases {
        let raw = RawSkill {
            folder,
            source,
            resources: &[],
        };
        assert!(parse(&raw).is_err(), "{source:?}");
    }
}

#[test]
fn delegated_and_auxiliary_prompts_render() {
    assert!(skill_permissions_note().contains("never grants tool access"));
    let forked = forked_skill_context("pkg:lint", "/skills/lint");
    assert!(forked.starts_with("Skill pkg:lint.") && forked.contains("/skills/lint"));
    assert!(compaction().contains("Next steps"));
    assert!(!title().is_empty());
    assert!(step_limit().contains("Do not call any more tools") && !step_limit().contains("{{"));
    let first = compaction_request(None, "T1");
    assert!(first.ends_with("T1") && !first.contains("Update that summary"));
    let update = compaction_request(Some("S0"), "T2");
    assert!(update.contains("S0") && update.ends_with("T2"));
    for header in [compaction_excerpts_header(), compaction_tool_notes_header()] {
        assert!(header.starts_with('[') && header.ends_with(']') && !header.contains('\n'));
    }
    let summary = compaction_summary("S1");
    assert!(summary.starts_with("# Summary of earlier work") && summary.ends_with("\nS1"));
}

#[test]
fn subagent_shares_environment_permissions_and_instructions_only() {
    let workspace = PathBuf::from("/work/project");
    let mut ctx = sample(&workspace);
    ctx.permission_mode = PermissionMode::ReadOnly;
    ctx.project_instructions = "## AGENTS.md\nUse tabs.".to_owned();
    ctx.skills_index = skills_index([("code-review", "Review a diff.")]);
    ctx.facts_index = facts_index(["Tests run with cargo nextest"]);
    let child = subagent(&ctx, "  Find the config loader.  ");
    for expected in [
        "Workspace: /work/project",
        "Git repository, branch main",
        "# Permissions: Read only",
        "# Project instructions",
        "Use tabs.",
    ] {
        assert!(child.contains(expected), "missing {expected:?}");
    }
    assert!(child.ends_with("# Task\nFind the config loader."));
    assert!(!child.contains("code-review") && !child.contains("cargo nextest"));
    assert!(!child.contains("{{"));
    assert_eq!(child, subagent(&ctx, "Find the config loader."));
}

#[test]
fn permission_change_note_states_the_new_mode_and_policy() {
    for mode in PermissionMode::ALL {
        let note = permission_change(mode);
        assert!(note.contains(mode.label()), "{note}");
        assert!(note.ends_with(permission_policy(mode)));
        assert!(!note.contains("{{"));
    }
}
