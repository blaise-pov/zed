use anyhow::Result;
use collections::{HashMap, HashSet};
use gpui::SharedString;
use handlebars::{Handlebars, Template as HandlebarsTemplate};
use parking_lot::Mutex;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// Dev builds read the checkout's templates at runtime instead of embedding
// them; see the `assets` crate for the rationale.
util::fs_embed! {
    struct Assets,
    crate_relative = "src/templates",
    root_relative = "crates/agent/src/templates",
    include = ["*.hbs"],
}

pub struct Templates(Mutex<Handlebars<'static>>);

struct PartialsGuard<'a> {
    handlebars: &'a mut Handlebars<'static>,
    registered_names: Vec<String>,
}

impl<'a> Drop for PartialsGuard<'a> {
    fn drop(&mut self) {
        for name in &self.registered_names {
            self.handlebars.unregister_template(name);
        }
    }
}

impl Templates {
    pub fn new() -> Arc<Self> {
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(true);
        handlebars.register_helper("contains", Box::new(contains));
        handlebars.register_embed_templates::<Assets>().unwrap();
        Arc::new(Self(Mutex::new(handlebars)))
    }

    pub fn render_custom_template<T: serde::Serialize>(
        &self,
        template_str: &str,
        data: &T,
        worktree_root: Option<&Path>,
    ) -> anyhow::Result<String> {
        let directories = agent_settings::prompt_partials_dirs(worktree_root);
        self.render_custom_template_with_dirs(template_str, data, &directories)
    }

    pub fn render_custom_template_with_dirs<T: serde::Serialize>(
        &self,
        template_str: &str,
        data: &T,
        directories: &[PathBuf],
    ) -> anyhow::Result<String> {
        let root_template = HandlebarsTemplate::compile(template_str)?;
        let partials = load_prompt_partials(directories);
        let mut handlebars = self.0.lock();

        let mut visited = HashSet::default();
        let mut stack = Vec::new();
        collect_partial_references(&root_template, &mut stack);

        while let Some(name) = stack.pop() {
            if !visited.insert(name.clone()) {
                continue;
            }
            if let Some(partial_template) = partials.get(&name) {
                collect_partial_references(partial_template, &mut stack);
            } else if handlebars.get_template(&name).is_some() {
                // Built-in template (e.g. system_prompt.hbs)
            } else {
                anyhow::bail!("prompt partial not found: {name}");
            }
        }

        let mut guard = PartialsGuard {
            handlebars: &mut handlebars,
            registered_names: Vec::new(),
        };
        for (name, template) in partials {
            guard.handlebars.register_template(&name, template);
            guard.registered_names.push(name);
        }
        let rendered = guard.handlebars.render_template(template_str, data)?;
        Ok(rendered)
    }
}

fn extract_partial_name(param: &handlebars::template::Parameter) -> Option<String> {
    match param {
        handlebars::template::Parameter::Name(name) => {
            let trimmed = name.trim();
            let unquoted = if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
                || (trimmed.starts_with('"') && trimmed.ends_with('"'))
            {
                if trimmed.len() >= 2 {
                    &trimmed[1..trimmed.len() - 1]
                } else {
                    trimmed
                }
            } else {
                trimmed
            };
            Some(unquoted.to_string())
        }
        _ => None,
    }
}

fn collect_partial_references(template: &HandlebarsTemplate, references: &mut Vec<String>) {
    for element in &template.elements {
        match element {
            handlebars::template::TemplateElement::PartialExpression(partial) => {
                if let Some(name) = extract_partial_name(&partial.name) {
                    references.push(name);
                }
            }
            handlebars::template::TemplateElement::PartialBlock(partial) => {
                if let Some(name) = extract_partial_name(&partial.name) {
                    references.push(name);
                }
                if let Some(inner) = &partial.template {
                    collect_partial_references(inner, references);
                }
            }
            handlebars::template::TemplateElement::HelperBlock(helper) => {
                if let Some(inner) = &helper.template {
                    collect_partial_references(inner, references);
                }
                if let Some(inner) = &helper.inverse {
                    collect_partial_references(inner, references);
                }
            }
            handlebars::template::TemplateElement::DecoratorBlock(decorator) => {
                if let Some(inner) = &decorator.template {
                    collect_partial_references(inner, references);
                }
            }
            _ => {}
        }
    }
}

fn is_in_cycle(start: &str, edges: &HashMap<String, Vec<String>>) -> bool {
    let mut visited = HashSet::default();
    let mut stack = Vec::new();
    if let Some(neighbors) = edges.get(start) {
        for next in neighbors {
            if next == start {
                return true;
            }
            stack.push(next.as_str());
        }
    }

    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue;
        }
        if let Some(neighbors) = edges.get(current) {
            for next in neighbors {
                if next == start {
                    return true;
                }
                if !visited.contains(next.as_str()) {
                    stack.push(next.as_str());
                }
            }
        }
    }

    false
}

fn load_prompt_partials(directories: &[PathBuf]) -> HashMap<String, HandlebarsTemplate> {
    let mut candidate_partials: HashMap<String, (PathBuf, String)> = HashMap::default();

    for directory in directories {
        let Ok(read_dir) = std::fs::read_dir(directory) else {
            continue;
        };

        let mut entries: Vec<_> = read_dir.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            if extension != "md" && extension != "hbs" {
                continue;
            }

            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if stem.is_empty() {
                continue;
            }

            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    candidate_partials.insert(stem.to_string(), (path, content));
                }
                Err(err) => {
                    log::warn!(
                        "failed to read prompt partial file from {}: {err}",
                        path.display()
                    );
                }
            }
        }
    }

    let mut parsed_partials: HashMap<String, (PathBuf, HandlebarsTemplate)> = HashMap::default();
    for (name, (path, content)) in candidate_partials {
        match HandlebarsTemplate::compile(&content) {
            Ok(template) => {
                parsed_partials.insert(name, (path, template));
            }
            Err(err) => {
                log::warn!(
                    "failed to parse prompt partial template from {}: {err}",
                    path.display()
                );
            }
        }
    }

    let mut edges: HashMap<String, Vec<String>> = HashMap::default();
    for (name, (_, template)) in &parsed_partials {
        let mut references = Vec::new();
        collect_partial_references(template, &mut references);
        let valid_edges = references
            .into_iter()
            .filter(|referenced_name| parsed_partials.contains_key(referenced_name))
            .collect();
        edges.insert(name.clone(), valid_edges);
    }

    let mut valid_partials = HashMap::default();
    for (name, (path, template)) in parsed_partials {
        if is_in_cycle(&name, &edges) {
            log::warn!(
                "prompt partial '{name}' from {} participates in a recursive include cycle and was skipped",
                path.display()
            );
        } else {
            valid_partials.insert(name, template);
        }
    }

    valid_partials
}

pub trait Template: Sized {
    const TEMPLATE_NAME: &'static str;

    fn render(&self, templates: &Templates) -> Result<String>
    where
        Self: Serialize + Sized,
    {
        Ok(templates.0.lock().render(Self::TEMPLATE_NAME, self)?)
    }
}

#[derive(Serialize)]
pub struct SystemPromptTemplate<'a> {
    #[serde(flatten)]
    pub project: &'a prompt_store::ProjectContext,
    pub available_tools: Vec<SharedString>,
    pub model_name: Option<String>,
    pub date: String,
    /// Contents of the user-global `~/.config/zed/AGENTS.md` file (or the
    /// platform equivalent), if present and non-empty.
    pub user_agents_md: Option<SharedString>,
    /// Whether agent-run terminal commands are wrapped in an OS-level
    /// sandbox for this thread. When `true` — and the `terminal` tool is
    /// in `available_tools` — the rendered prompt describes the sandbox's
    /// read/write/network rules and the per-command flags the model can
    /// request to relax them. Otherwise the prompt omits the sandbox
    /// section entirely.
    pub sandboxing: bool,
    /// Whether the host is Linux. The writable-temp story differs by
    /// platform (Linux exposes an ephemeral `tmpfs` over `/tmp`; other
    /// platforms provide a persistent per-thread `$TMPDIR`), so the sandbox
    /// section describes the right one rather than advertising a `$TMPDIR`
    /// that doesn't behave as stated.
    pub is_linux: bool,
    /// Whether sandboxed terminal commands run through WSL on Windows.
    pub is_windows: bool,
    /// Custom instructions from the active agent profile.
    pub custom_instructions: Option<String>,
    /// A note for sub-agents that just hit the nesting depth limit, telling
    /// them to complete the task themselves. Only set when the `spawn_agent`
    /// tool was withheld because of the depth limit (see
    /// `Thread::subagent_delegation_note`).
    pub subagent_delegation_note: Option<&'static str>,
    /// Catalog of agents this profile may delegate to, one line per agent
    /// (`id — name: description`). Only set when the active profile has a
    /// `delegation` block; rendered inside the delegation section of the
    /// system prompt.
    pub available_agents: Option<String>,
}

impl Template for SystemPromptTemplate<'_> {
    const TEMPLATE_NAME: &'static str = "system_prompt.hbs";
}

/// Handlebars helper for checking if an item is in a list
fn contains(
    h: &handlebars::Helper,
    _: &handlebars::Handlebars,
    _: &handlebars::Context,
    _: &mut handlebars::RenderContext,
    out: &mut dyn handlebars::Output,
) -> handlebars::HelperResult {
    let list = h
        .param(0)
        .and_then(|v| v.value().as_array())
        .ok_or_else(|| {
            handlebars::RenderError::new("contains: missing or invalid list parameter")
        })?;
    let query = h.param(1).map(|v| v.value()).ok_or_else(|| {
        handlebars::RenderError::new("contains: missing or invalid query parameter")
    })?;

    if list.contains(query) {
        out.write("true")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_custom_template() {
        #[derive(serde::Serialize)]
        struct Data {
            name: String,
        }

        let templates = Templates::new();
        let rendered = templates
            .render_custom_template(
                "Hello, {{name}}!",
                &Data {
                    name: "Zed".to_string(),
                },
                None,
            )
            .unwrap();
        assert_eq!(rendered, "Hello, Zed!");

        // Strict mode rejects templates referencing fields the data doesn't have.
        let err = templates.render_custom_template(
            "{{missing_field}}",
            &Data {
                name: "Zed".to_string(),
            },
            None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_system_prompt_template() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(rendered.contains("You are the Zed coding agent"));
        assert!(rendered.contains("Today's Date: 2026-01-01"));
        assert!(rendered.contains("## Fixing Diagnostics"));
        assert!(rendered.contains("test-model"));
    }

    #[test]
    fn test_system_prompt_renders_subagent_delegation_note() {
        let project = prompt_store::ProjectContext::default();
        let base = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["read_file".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let without_note = base.render(&templates).unwrap();
        assert!(!without_note.contains("Delegation is unavailable"));

        let with_note = SystemPromptTemplate {
            subagent_delegation_note: Some(
                "Delegation is unavailable: the maximum sub-agent depth has been reached.",
            ),
            ..base
        };
        let rendered = with_note.render(&templates).unwrap();
        assert!(
            rendered.contains(
                "Delegation is unavailable: the maximum sub-agent depth has been reached."
            )
        );
        // The delegation section itself is absent without the tool.
        assert!(!rendered.contains("## Multi-agent delegation"));
    }

    #[test]
    fn test_system_prompt_renders_user_agents_md_before_project_rules() {
        use prompt_store::{ProjectContext, RulesFileContext, WorktreeContext};
        use util::rel_path::RelPath;

        let worktrees = vec![WorktreeContext {
            root_name: "my-project".to_string(),
            abs_path: std::path::Path::new("/tmp/my-project").into(),
            rules_file: Some(RulesFileContext {
                path_in_worktree: RelPath::from_unix_str("AGENTS.md").unwrap().into(),
                text: "project-specific guidance".to_string(),
                project_entry_id: 1,
            }),
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: Some("always be concise".into()),
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("### Personal `AGENTS.md`"));
        assert!(rendered.contains("always be concise"));
        assert!(rendered.contains("### Project Rules"));
        assert!(rendered.contains("project-specific guidance"));

        let personal_idx = rendered.find("### Personal `AGENTS.md`").unwrap();
        let project_idx = rendered.find("### Project Rules").unwrap();
        assert!(
            personal_idx < project_idx,
            "personal AGENTS.md should render before project rules so project rules can override it"
        );
    }

    #[test]
    fn test_system_prompt_omits_sandbox_section_when_sandboxing_disabled() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(!rendered.contains("## Terminal sandbox"));
        assert!(!rendered.contains("allow_hosts"));
    }

    #[test]
    fn test_system_prompt_renders_sandbox_section_with_worktrees_when_enabled() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![
            WorktreeContext {
                root_name: "alpha".to_string(),
                abs_path: std::path::Path::new("/tmp/alpha").into(),
                rules_file: None,
            },
            WorktreeContext {
                root_name: "beta".to_string(),
                abs_path: std::path::Path::new("/tmp/beta").into(),
                rules_file: None,
            },
        ];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        assert!(rendered.contains("`/tmp/alpha`"));
        assert!(rendered.contains("`/tmp/beta`"));
        assert!(rendered.contains("allow_hosts"));
        assert!(rendered.contains("allow_all_hosts: true"));
        assert!(rendered.contains("fs_write_paths"));
        assert!(rendered.contains("allow_fs_write_all: true"));
        assert!(rendered.contains("unsandboxed: true"));
        assert!(rendered.contains("`.git` directories remain protected"));
        assert!(rendered.contains("Git metadata writes are never grantable inside the sandbox"));
        assert!(rendered.contains("request `unsandboxed: true` with a reason"));
        assert!(rendered.contains("git --no-optional-locks status"));
        assert!(rendered.contains("for the rest of the thread"));
        // macOS tolerates granting a not-yet-existing path, so the
        // existing-directory requirement must not be stated there; the
        // `create_directory` flow is the preferred guidance instead.
        assert!(!rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_linux_sandbox_section_omits_tmpdir() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![WorktreeContext {
            root_name: "alpha".to_string(),
            abs_path: std::path::Path::new("/tmp/alpha").into(),
            rules_file: None,
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: true,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        // On Linux we must not advertise the special persistent `$TMPDIR`.
        assert!(!rendered.contains("$TMPDIR"));
        assert!(rendered.contains("`/tmp` is writable"));
        assert!(rendered.contains("`/tmp/alpha`"));
        // Linux write grants must already exist (bwrap binds existing paths).
        assert!(rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_windows_sandbox_section_rejects_host_specific_network() {
        use prompt_store::{ProjectContext, WorktreeContext};

        let worktrees = vec![WorktreeContext {
            root_name: "alpha".to_string(),
            abs_path: std::path::Path::new("C:/Users/me/project").into(),
            rules_file: None,
        }];
        let project = ProjectContext::new(worktrees);
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: true,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("commands run inside WSL under Bubblewrap"));
        assert!(rendered.contains("Protected Git metadata remains read-only"));
        assert!(rendered.contains("do not use this on Windows"));
        assert!(rendered.contains("such requests are rejected"));
        assert!(rendered.contains("allow_all_hosts: true"));
        assert!(rendered.contains("git --no-optional-locks status"));
        // Out-of-project `create_directory` grants aren't supported on Windows,
        // so the prompt must not recommend that flow; it suggests granting the
        // nearest existing parent instead.
        assert!(rendered.contains("Each path must be an existing directory"));
        assert!(rendered.contains("nearest existing parent directory"));
        assert!(!rendered.contains("first create it with the `create_directory` tool"));
    }

    #[test]
    fn test_system_prompt_sandbox_section_handles_zero_worktrees() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "terminal".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("## Terminal sandbox"));
        assert!(rendered.contains("No project directories are currently writable"));
    }

    #[test]
    fn test_system_prompt_omits_sandbox_section_when_terminal_tool_unavailable() {
        // A profile can disable the terminal tool entirely; the prompt must not
        // describe a sandboxed `terminal` tool the model doesn't have.
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: true,
            is_linux: false,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(!rendered.contains("## Terminal sandbox"));
        assert!(!rendered.contains("allow_hosts"));
    }

    #[test]
    fn test_system_prompt_omits_user_agents_md_section_when_absent() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(!rendered.contains("### Personal `AGENTS.md`"));
    }

    #[test]
    fn test_system_prompt_does_not_render_legacy_zed_rules_section() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(!rendered.contains("The user has specified the following rules"));
        assert!(!rendered.contains("Rules title:"));
    }

    #[test]
    fn test_render_custom_template_basic_include() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("greeting.md"), "Hello from partial!").unwrap();

        let templates = Templates::new();
        let rendered = templates
            .render_custom_template_with_dirs(
                "Start {{> greeting}} End",
                &(),
                &[temp_dir.path().to_path_buf()],
            )
            .unwrap();
        assert_eq!(rendered, "Start Hello from partial! End");
    }

    #[test]
    fn test_render_custom_template_precedence() {
        let config_dir = tempfile::tempdir().unwrap();
        let worktree_dir = tempfile::tempdir().unwrap();
        std::fs::write(config_dir.path().join("greeting.md"), "From config").unwrap();
        std::fs::write(worktree_dir.path().join("greeting.hbs"), "From worktree").unwrap();

        let templates = Templates::new();
        let dirs = vec![
            config_dir.path().to_path_buf(),
            worktree_dir.path().to_path_buf(),
        ];
        let rendered = templates
            .render_custom_template_with_dirs("{{> greeting}}", &(), &dirs)
            .unwrap();
        assert_eq!(rendered, "From worktree");
    }

    #[test]
    fn test_render_custom_template_nesting_and_variables() {
        #[derive(serde::Serialize)]
        struct Context {
            variable: String,
        }

        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("inner.md"), "inner: {{variable}}").unwrap();
        std::fs::write(temp_dir.path().join("outer.hbs"), "outer: [{{> inner}}]").unwrap();

        let templates = Templates::new();
        let rendered = templates
            .render_custom_template_with_dirs(
                "root: {{> outer}}",
                &Context {
                    variable: "interpolated_value".to_string(),
                },
                &[temp_dir.path().to_path_buf()],
            )
            .unwrap();
        assert_eq!(rendered, "root: outer: [inner: interpolated_value]");
    }

    #[test]
    fn test_render_custom_template_self_recursive() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("recurse.md"), "hello {{> recurse}}").unwrap();

        let templates = Templates::new();
        let result = templates.render_custom_template_with_dirs(
            "start {{> recurse}}",
            &(),
            &[temp_dir.path().to_path_buf()],
        );
        assert!(result.is_err(), "expected error, got: {:?}", result);
    }

    #[test]
    fn test_render_custom_template_mutual_recursive() {
        let temp_dir = tempfile::tempdir().unwrap();
        std::fs::write(temp_dir.path().join("cycle_a.md"), "a: {{> cycle_b}}").unwrap();
        std::fs::write(temp_dir.path().join("cycle_b.md"), "b: {{> cycle_a}}").unwrap();

        let templates = Templates::new();
        let result = templates.render_custom_template_with_dirs(
            "start {{> cycle_a}}",
            &(),
            &[temp_dir.path().to_path_buf()],
        );
        assert!(result.is_err(), "expected error, got: {:?}", result);
    }

    #[test]
    fn test_render_custom_template_missing_partial() {
        let temp_dir = tempfile::tempdir().unwrap();
        let templates = Templates::new();
        let result = templates.render_custom_template_with_dirs(
            "{{> missing_partial}}",
            &(),
            &[temp_dir.path().to_path_buf()],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_render_custom_template_includes_builtin_system_prompt() {
        let project = prompt_store::ProjectContext::default();
        let template_data = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            sandboxing: false,
            is_linux: false,
            is_windows: false,
            custom_instructions: None,
            subagent_delegation_note: None,
            available_agents: None,
        };

        let templates = Templates::new();
        let rendered = templates
            .render_custom_template(
                "Prefix\n{{> system_prompt.hbs}}\nSuffix",
                &template_data,
                None,
            )
            .unwrap();

        assert!(rendered.starts_with("Prefix\n"));
        assert!(rendered.ends_with("\nSuffix"));
        assert!(rendered.contains("You are the Zed coding agent"));
        assert!(rendered.contains("test-model"));
    }
}
