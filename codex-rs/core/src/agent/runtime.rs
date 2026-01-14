use std::collections::HashSet;
use std::sync::Arc;

use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_subagent::AgentKind;
use codex_subagent::AgentManifest;
use codex_subagent::BuiltInAgent;
use codex_subagent::HookSet;
use codex_subagent::PermissionMode;
use codex_subagent::ToolScope;
use thiserror::Error;

/// Runtime configuration produced for an activated subagent manifest.
#[derive(Clone, Debug)]
pub struct AgentRuntimeProfile {
    manifest: Arc<AgentManifest>,
    claude_agent_id: String,
    session_source: SessionSource,
    model: String,
    approval_policy: AskForApproval,
    allowed_tools: Vec<String>,
    hooks: HookSet,
}

impl AgentRuntimeProfile {
    pub(crate) fn from_manifest(
        manifest: Arc<AgentManifest>,
        ctx: &ActivationContext<'_>,
    ) -> Result<Self, ActivationError> {
        let permission_resolver = PermissionResolver;
        let approval_policy =
            permission_resolver.resolve(&manifest.permission_mode, ctx.parent_approval_policy);

        let tool_filter = ToolScopeFilter;
        let allowed_tools = tool_filter.filter(&manifest.tool_scope, ctx.available_tools);

        let model = manifest
            .model
            .as_ref()
            .map(|m| m.as_str().to_owned())
            .unwrap_or_else(|| ctx.parent_model.to_owned());

        let session_source = session_source_for_manifest(&manifest);
        let hooks = manifest.hooks.clone();

        Ok(Self {
            claude_agent_id: manifest.id.clone(),
            manifest,
            session_source,
            model,
            approval_policy,
            allowed_tools,
            hooks,
        })
    }

    /// Returns the manifest backing this runtime profile.
    #[must_use]
    pub fn manifest(&self) -> &AgentManifest {
        &self.manifest
    }

    /// Identifier passed to Claude's `register_subagents` API.
    #[must_use]
    pub fn claude_agent_id(&self) -> &str {
        &self.claude_agent_id
    }

    /// Effective session source used for telemetry and headers.
    #[must_use]
    pub fn session_source(&self) -> &SessionSource {
        &self.session_source
    }

    /// Model that should be used for the runtime session.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Approval policy enforced for the subagent.
    #[must_use]
    pub fn approval_policy(&self) -> AskForApproval {
        self.approval_policy
    }

    /// Tools allowed for the subagent.
    #[must_use]
    pub fn allowed_tools(&self) -> &[String] {
        &self.allowed_tools
    }

    /// Hook definitions supplied by the manifest.
    #[must_use]
    pub fn hooks(&self) -> &HookSet {
        &self.hooks
    }
}

/// Context describing the parent session used to activate a subagent.
#[derive(Debug, Clone)]
pub struct ActivationContext<'a> {
    parent_model: &'a str,
    parent_approval_policy: AskForApproval,
    available_tools: &'a [String],
}

impl<'a> ActivationContext<'a> {
    #[must_use]
    pub fn new(
        parent_model: &'a str,
        parent_approval_policy: AskForApproval,
        available_tools: &'a [String],
    ) -> Self {
        Self {
            parent_model,
            parent_approval_policy,
            available_tools,
        }
    }

    #[must_use]
    pub fn parent_model(&self) -> &str {
        self.parent_model
    }

    #[must_use]
    pub fn parent_approval_policy(&self) -> AskForApproval {
        self.parent_approval_policy
    }

    #[must_use]
    pub fn available_tools(&self) -> &[String] {
        self.available_tools
    }
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ActivationError {
    #[error("unknown subagent `{agent_id}`")]
    UnknownAgent { agent_id: String },
}

#[derive(Default, Debug, Clone, Copy)]
pub struct PermissionResolver;

impl PermissionResolver {
    #[must_use]
    pub fn resolve(&self, mode: &PermissionMode, parent: AskForApproval) -> AskForApproval {
        match mode {
            PermissionMode::Default => parent,
            PermissionMode::AcceptEdits => AskForApproval::UnlessTrusted,
            PermissionMode::DontAsk => AskForApproval::Never,
            PermissionMode::BypassPermissions => AskForApproval::Never,
            PermissionMode::Plan => AskForApproval::OnRequest,
            PermissionMode::Ignore => AskForApproval::Never,
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct ToolScopeFilter;

impl ToolScopeFilter {
    #[must_use]
    pub fn filter(&self, scope: &ToolScope, available_tools: &[String]) -> Vec<String> {
        match scope.as_slice() {
            None => available_tools.to_vec(),
            Some(restricted) => {
                let available: HashSet<&str> = available_tools
                    .iter()
                    .map(std::string::String::as_str)
                    .collect();
                let mut allowed = Vec::new();
                let mut seen = HashSet::new();
                for tool in restricted {
                    let name = tool.as_str();
                    let aliases = tool_alias_candidates(name);
                    let has_match = aliases.iter().any(|alias| available.contains(*alias));
                    if !has_match {
                        continue;
                    }
                    insert_unique(&mut allowed, &mut seen, name);
                    for alias in aliases {
                        if available.contains(alias) {
                            insert_unique(&mut allowed, &mut seen, alias);
                        }
                    }
                }
                allowed
            }
        }
    }
}

fn insert_unique(allowed: &mut Vec<String>, seen: &mut HashSet<String>, name: &str) {
    if seen.insert(name.to_owned()) {
        allowed.push(name.to_owned());
    }
}

fn tool_alias_candidates(name: &str) -> Vec<&str> {
    const SHELL_ALIASES: &[&str] = &["shell", "local_shell", "shell_command", "container.exec"];
    match name {
        "shell" | "local_shell" | "shell_command" | "container.exec" => SHELL_ALIASES.to_vec(),
        _ => vec![name],
    }
}

fn session_source_for_manifest(manifest: &AgentManifest) -> SessionSource {
    let sub_source = match &manifest.kind {
        AgentKind::BuiltIn { agent } => match agent {
            BuiltInAgent::GeneralPurpose => SubAgentSource::GeneralPurpose,
            BuiltInAgent::Plan => SubAgentSource::Plan,
            BuiltInAgent::Explore => SubAgentSource::Explore,
            BuiltInAgent::Review => SubAgentSource::Review,
        },
        _ => SubAgentSource::Other(manifest.id.clone()),
    };
    SessionSource::SubAgent(sub_source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_subagent::ToolName;
    use pretty_assertions::assert_eq;

    fn manifest_with_scope(scope: ToolScope) -> Arc<AgentManifest> {
        Arc::new(AgentManifest {
            id: "agent".into(),
            kind: AgentKind::Custom,
            name: "Test Agent".into(),
            description: "subagent used for tests".into(),
            model: None,
            tool_scope: scope,
            permission_mode: PermissionMode::Default,
            hooks: HookSet::default(),
            triggers: vec![],
            skills: vec![],
            body: String::new(),
            source: None,
            digest: None,
        })
    }

    #[test]
    fn restricted_scope_clamps_allowed_tools() {
        let manifest = manifest_with_scope(ToolScope::restricted(vec![
            ToolName::from("read_file"),
            ToolName::from("list_dir"),
        ]));
        let available = vec![
            "read_file".to_string(),
            "list_dir".to_string(),
            "shell".to_string(),
        ];
        let ctx =
            ActivationContext::new("claude-3.5-sonnet", AskForApproval::OnRequest, &available);
        let profile =
            AgentRuntimeProfile::from_manifest(manifest, &ctx).expect("profile should build");

        let expected = vec!["read_file".to_string(), "list_dir".to_string()];
        assert_eq!(profile.allowed_tools(), expected.as_slice());
    }

    #[test]
    fn inherit_scope_includes_all_parent_tools() {
        let manifest = manifest_with_scope(ToolScope::inherit());
        let available = vec!["shell".to_string(), "apply_patch".to_string()];
        let ctx = ActivationContext::new("claude-3-opus", AskForApproval::Never, &available);
        let profile =
            AgentRuntimeProfile::from_manifest(manifest, &ctx).expect("profile should build");

        assert_eq!(profile.allowed_tools(), available.as_slice());
    }

    #[test]
    fn shell_aliases_expand_when_tool_is_available() {
        let manifest = manifest_with_scope(ToolScope::restricted(vec![ToolName::from("shell")]));
        let available = vec!["shell_command".to_string()];
        let ctx = ActivationContext::new("claude-3.5-sonnet", AskForApproval::Never, &available);
        let profile =
            AgentRuntimeProfile::from_manifest(manifest, &ctx).expect("profile should build");

        assert_eq!(
            profile.allowed_tools(),
            &["shell".to_string(), "shell_command".to_string()]
        );
    }

    #[test]
    fn permission_and_scope_overrides_survive_activation() {
        let manifest = Arc::new(AgentManifest {
            id: "scoped".into(),
            kind: AgentKind::Custom,
            name: "Scoped".into(),
            description: "tests permission+scope overrides".into(),
            model: None,
            tool_scope: ToolScope::restricted(vec![
                ToolName::from("read_file"),
                ToolName::from("list_dir"),
                ToolName::from("nonexistent"),
            ]),
            permission_mode: PermissionMode::Plan,
            hooks: HookSet::default(),
            triggers: vec![],
            skills: vec![],
            body: String::new(),
            source: None,
            digest: None,
        });
        let available = vec!["read_file".to_string()];
        let ctx = ActivationContext::new("claude-3.5-sonnet", AskForApproval::Never, &available);
        let profile =
            AgentRuntimeProfile::from_manifest(manifest, &ctx).expect("profile should build");

        assert_eq!(profile.approval_policy(), AskForApproval::OnRequest);
        assert_eq!(profile.allowed_tools(), &["read_file".to_string()]);
    }
}
