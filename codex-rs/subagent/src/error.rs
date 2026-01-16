use std::path::PathBuf;

use thiserror::Error;

#[cfg(feature = "schema")]
use crate::manifest::AgentId;
#[cfg(feature = "schema")]
use crate::priority::DiscoveryScope;

/// Error returned while parsing, validating, or loading manifests.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// Failed to read the manifest file from disk.
    #[error("failed to read manifest at {path}: {source}")]
    Io {
        /// Location of the manifest on disk.
        path: PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
    /// Failed to parse the manifest contents.
    #[error("failed to parse manifest at {path}: {source}")]
    Parse {
        /// Location of the manifest on disk.
        path: PathBuf,
        /// Underlying parser error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The manifest did not pass schema validation.
    #[error("manifest validation failed at {path}: {issues}")]
    Validation {
        /// Manifest path.
        path: PathBuf,
        /// Structured validation issues.
        issues: ValidationIssues,
    },
    /// Two manifests resolved to the same agent identifier from different scopes.
    #[cfg(feature = "schema")]
    #[error("agent id {agent_id} defined more than once: {first:?} vs {second:?}")]
    DuplicateId {
        /// Conflicting identifier.
        agent_id: AgentId,
        /// First definition location.
        first: DiscoveryScope,
        /// Second definition location.
        second: DiscoveryScope,
    },
    /// The caller requested watch functionality but the loader does not support it yet.
    #[error("watch mode is not implemented for this loader")]
    WatchUnsupported,
    /// Inline errors that are already formatted for the caller.
    #[error("{0}")]
    Inline(String),
}

/// Individual validation issue captured while checking manifest invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationIssue {
    /// Required field was missing or empty.
    MissingField(&'static str),
    /// Field contained an invalid value that failed semantic checks.
    InvalidField {
        /// Field that failed validation.
        field: &'static str,
        /// Additional error context.
        message: String,
    },
    /// Hook definition violated exclusivity rules.
    ConflictingHook {
        /// Name of the offending hook.
        hook: String,
    },
    /// Trigger definition was invalid.
    InvalidTrigger {
        /// Identifier for the trigger.
        trigger: String,
        /// Additional error context.
        message: String,
    },
    /// Tool definition failed validation.
    InvalidTool {
        /// Tool name.
        name: String,
        /// Additional context.
        message: String,
    },
    /// Priority rules were violated.
    Priority(String),
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingField(field) => write!(f, "missing required field: {field}"),
            Self::InvalidField { field, message } => write!(f, "invalid {field}: {message}"),
            Self::ConflictingHook { hook } => {
                write!(f, "hook {hook} must specify exactly one action")
            }
            Self::InvalidTrigger { trigger, message } => {
                write!(f, "invalid trigger {trigger}: {message}")
            }
            Self::InvalidTool { name, message } => write!(f, "invalid tool {name}: {message}"),
            Self::Priority(message) => f.write_str(message),
        }
    }
}

/// Collection wrapper so we can attach multiple validation issues to an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationIssues {
    issues: Vec<ValidationIssue>,
}

impl ValidationIssues {
    /// Creates an empty set of issues.
    pub fn new() -> Self {
        Self { issues: Vec::new() }
    }

    /// Adds a new issue to the set.
    pub fn push(&mut self, issue: ValidationIssue) {
        self.issues.push(issue);
    }

    /// Returns true when no issues were recorded.
    pub fn is_empty(&self) -> bool {
        self.issues.is_empty()
    }

    /// Returns the recorded issues.
    pub fn into_vec(self) -> Vec<ValidationIssue> {
        self.issues
    }

    /// Provides shared access for callers that want to inspect issues individually.
    pub fn as_slice(&self) -> &[ValidationIssue] {
        &self.issues
    }
}

impl std::fmt::Display for ValidationIssues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.issues.is_empty() {
            return f.write_str("no validation issues recorded");
        }

        for (idx, issue) in self.issues.iter().enumerate() {
            if idx > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{issue}")?;
        }

        Ok(())
    }
}

impl From<ValidationIssue> for ValidationIssues {
    fn from(issue: ValidationIssue) -> Self {
        let mut issues = Self::new();
        issues.push(issue);
        issues
    }
}
