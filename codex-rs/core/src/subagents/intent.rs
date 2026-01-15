use std::collections::HashSet;

use codex_subagent::AgentManifest;
use codex_subagent::TriggerDefinition;
use globset::Glob;
use tracing::debug;

const MAX_ANALYZED_CHARS: usize = 8_192;
const MIN_INTENT_SCORE: u32 = 10;
const MIN_SCORE_LEAD: u32 = 3;
const SKILL_MATCH_POINTS: u32 = 4;
const NAME_TOKEN_POINTS: u32 = 6;
const ID_TOKEN_POINTS: u32 = 5;

/// Result of matching a manifest against the latest user text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IntentMatch {
    pub agent_id: String,
    pub agent_name: String,
    pub score: u32,
    pub reason: Option<String>,
}

/// Attempts to infer a manifest identifier from the free-form user text.
pub(crate) fn infer_subagent_from_text(
    manifests: &[AgentManifest],
    raw_text: &str,
) -> Option<IntentMatch> {
    let trimmed = raw_text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut normalized = trimmed.to_lowercase();
    if normalized.len() > MAX_ANALYZED_CHARS {
        normalized.truncate(MAX_ANALYZED_CHARS);
    }
    let tokens = tokenize(&normalized);
    if tokens.is_empty() {
        return None;
    }
    let token_set: HashSet<String> = tokens.iter().cloned().collect();

    struct Candidate<'a> {
        manifest: &'a AgentManifest,
        score: u32,
        reason: Option<String>,
    }

    let mut best: Option<Candidate<'_>> = None;
    let mut runner_up_score = 0u32;

    for manifest in manifests {
        if manifest.id == "builtin-general-purpose" {
            continue;
        }

        let (score, reason) = score_manifest(manifest, &normalized, &token_set, &tokens);
        if score == 0 {
            continue;
        }

        match &mut best {
            Some(current) if score > current.score => {
                runner_up_score = current.score;
                *current = Candidate {
                    manifest,
                    score,
                    reason,
                };
            }
            Some(_) => {
                runner_up_score = runner_up_score.max(score);
            }
            None => {
                best = Some(Candidate {
                    manifest,
                    score,
                    reason,
                });
            }
        }
    }

    let best = best?;
    if best.score < MIN_INTENT_SCORE {
        return None;
    }
    if best.score < runner_up_score + MIN_SCORE_LEAD {
        return None;
    }

    Some(IntentMatch {
        agent_id: best.manifest.id.clone(),
        agent_name: best.manifest.name.clone(),
        score: best.score,
        reason: best.reason,
    })
}

fn score_manifest(
    manifest: &AgentManifest,
    text: &str,
    token_set: &HashSet<String>,
    tokens: &[String],
) -> (u32, Option<String>) {
    let mut score = 0u32;
    let mut reasons: Vec<String> = Vec::new();

    for trigger in &manifest.triggers {
        match trigger {
            TriggerDefinition::Keyword { phrase, weight } => {
                if phrase.is_empty() {
                    continue;
                }
                let phrase_lower = phrase.to_lowercase();
                if text.contains(&phrase_lower) {
                    score += u32::from(*weight);
                    reasons.push(format!("keyword \"{phrase}\""));
                }
            }
            TriggerDefinition::Glob { pattern, weight } => {
                if glob_trigger_matches(pattern, tokens) {
                    score += u32::from(*weight);
                    reasons.push(format!("glob \"{pattern}\""));
                }
            }
        }
    }

    for skill in &manifest.skills {
        if skill.trim().is_empty() {
            continue;
        }
        let skill_lower = skill.to_lowercase();
        let matched = if skill_lower.contains(' ') {
            text.contains(&skill_lower)
        } else {
            token_set.contains(&skill_lower)
        };
        if matched {
            score += SKILL_MATCH_POINTS;
            reasons.push(format!("skill \"{skill}\""));
        }
    }

    score += score_label_tokens(
        &manifest.name,
        token_set,
        &mut reasons,
        NAME_TOKEN_POINTS,
        "name",
    );
    score += score_label_tokens(&manifest.id, token_set, &mut reasons, ID_TOKEN_POINTS, "id");

    (score, reasons.into_iter().next())
}

fn glob_trigger_matches(pattern: &str, tokens: &[String]) -> bool {
    if tokens.is_empty() || pattern.is_empty() {
        return false;
    }
    let normalized_pattern = pattern.to_lowercase();
    match Glob::new(&normalized_pattern) {
        Ok(glob) => {
            let matcher = glob.compile_matcher();
            tokens.iter().any(|token| matcher.is_match(token))
        }
        Err(err) => {
            debug!(
                pattern = pattern,
                error = %err,
                "skipping invalid glob trigger pattern"
            );
            false
        }
    }
}

fn score_label_tokens(
    label: &str,
    token_set: &HashSet<String>,
    reasons: &mut Vec<String>,
    points: u32,
    group: &str,
) -> u32 {
    let mut subtotal = 0u32;
    for token in tokenize_label(label) {
        if token_set.contains(&token) {
            subtotal += points;
            reasons.push(format!("{group} token \"{token}\""));
        }
    }
    subtotal
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '\\') {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn tokenize_label(label: &str) -> Vec<String> {
    label
        .to_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter_map(|segment| {
            let token = segment.trim();
            if token.len() >= 3 && !is_stopword(token) {
                Some(token.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token,
        "agent" | "agents" | "builtin" | "claude" | "codex" | "default"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_subagent::AgentKind;
    use codex_subagent::HookSet;
    use codex_subagent::PermissionMode;
    use codex_subagent::ToolScope;

    fn manifest(id: &str, name: &str) -> AgentManifest {
        AgentManifest {
            id: id.into(),
            kind: AgentKind::Custom,
            name: name.into(),
            description: "demo".into(),
            model: None,
            tool_scope: ToolScope::inherit(),
            permission_mode: PermissionMode::Default,
            hooks: HookSet::default(),
            triggers: Vec::new(),
            skills: Vec::new(),
            body: String::new(),
            source: None,
            digest: None,
        }
    }

    #[test]
    fn keyword_trigger_wins() {
        let mut alpha = manifest("alpha", "Alpha Builder");
        alpha.triggers = vec![TriggerDefinition::Keyword {
            phrase: "build".into(),
            weight: 10,
        }];
        let bravo = manifest("bravo", "Bravo QA");
        let matched = infer_subagent_from_text(
            &[alpha, bravo],
            "Please use the build agent for this task.",
        )
        .expect("expected a match");
        assert_eq!(matched.agent_id, "alpha");
        assert!(matched.score >= 10);
        assert!(
            matched
                .reason
                .expect("missing reason")
                .contains("keyword \"build\"")
        );
    }

    #[test]
    fn name_tokens_cover_plan_agent() {
        let plan = manifest("builtin-plan", "Claude Plan");
        let result =
            infer_subagent_from_text(&[plan], "Can you use the plan agent to outline steps?");
        assert_eq!(
            result.expect("expected plan match").agent_id,
            "builtin-plan"
        );
    }

    #[test]
    fn ties_are_considered_ambiguous() {
        let mut alpha = manifest("alpha", "Alpha Builder");
        alpha.triggers = vec![TriggerDefinition::Keyword {
            phrase: "deploy".into(),
            weight: 10,
        }];
        let mut beta = manifest("beta", "Beta Builder");
        beta.triggers = alpha.triggers.clone();
        assert!(infer_subagent_from_text(&[alpha, beta], "deploy the service").is_none());
    }

    #[test]
    fn glob_triggers_match_tokens() {
        let mut alpha = manifest("alpha", "Alpha Builder");
        alpha.triggers = vec![TriggerDefinition::Glob {
            pattern: "deploy-*".into(),
            weight: 12,
        }];
        assert_eq!(
            infer_subagent_from_text(&[alpha], "run deploy-api please")
                .expect("expected match")
                .agent_id,
            "alpha"
        );
    }
}
