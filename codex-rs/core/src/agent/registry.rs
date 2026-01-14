//! Subagent registry responsible for loading and indexing manifests.
//!
//! This module implements the `codex-core::agent` responsibilities described in
//! `docs/subagents/architecture.md`: keep an immutable list of `AgentManifest`s
//! sourced via `codex-subagent`, preserve discovery ordering, and provide quick
//! lookups for downstream runtime/profile builders (Step 4 of the Claude
//! compatibility plan).

use std::collections::HashMap;
use std::collections::HashSet;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::ptr;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::Weak;
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError as MpscTryRecvError;
use std::task::Context;
use std::task::Poll;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use super::runtime::ActivationContext;
use super::runtime::ActivationError;
use super::runtime::AgentRuntimeProfile;
use async_channel::Receiver as AsyncReceiver;
use async_channel::Sender as AsyncSender;
use async_channel::TryRecvError;
use codex_otel::OtelManager;
use codex_subagent::AgentId;
use codex_subagent::AgentManifest;
use codex_subagent::DiscoveryScope;
use codex_subagent::DiscoveryTarget;
use codex_subagent::FsManifestLoader;
use codex_subagent::LoaderEvent;
use codex_subagent::LoaderIssue;
use codex_subagent::LoaderWatch;
use codex_subagent::LoaderWatchTryRecvError;
use codex_subagent::ManifestError;
use codex_subagent::ManifestLoader;
use codex_subagent::built_in_manifests;
use codex_subagent::default_discovery_targets;
use futures::stream::Stream;
use futures::stream::{self};

/// Registry that owns the currently discovered manifests plus the loader used
/// to refresh them.
#[allow(dead_code)]
pub struct AgentRegistry {
    loader: Arc<dyn ManifestLoader>,
    manifests: Vec<Arc<AgentManifest>>,
    index: HashMap<AgentId, Arc<AgentManifest>>,
    last_outcome: Option<RefreshOutcome>,
}

/// Summary returned after refreshing the registry.
#[derive(Debug, Clone, Copy)]
pub struct RefreshReport {
    /// Total manifests stored in the registry (including built-ins).
    pub total_manifests: usize,
    /// Number of built-in manifests added during the refresh.
    pub built_in_manifests: usize,
    /// Number of duplicate manifest identifiers that were skipped.
    pub skipped_duplicates: usize,
    /// Discovery-scope breakdown for the refresh.
    pub scope_breakdown: ScopeBreakdown,
}

impl RefreshReport {
    /// Number of non built-in manifests retained after deduplication.
    #[must_use]
    pub fn custom_manifests(&self) -> usize {
        self.total_manifests.saturating_sub(self.built_in_manifests)
    }
}

/// Counts of manifests discovered per scope.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScopeBreakdown {
    /// Project-scoped manifests.
    pub project: usize,
    /// CLI inline manifests.
    pub cli: usize,
    /// User directory manifests.
    pub user: usize,
    /// Plugin-provided manifests.
    pub plugin: usize,
    /// Built-in manifests bundled with Codex.
    pub built_in: usize,
    /// Manifests lacking scope metadata.
    pub unknown: usize,
}

impl ScopeBreakdown {
    /// Records a manifest scoped to the provided discovery scope.
    pub fn record(&mut self, scope: Option<&DiscoveryScope>) {
        match scope {
            Some(DiscoveryScope::Project { .. }) => self.project += 1,
            Some(DiscoveryScope::CliJson { .. }) => self.cli += 1,
            Some(DiscoveryScope::User { .. }) => self.user += 1,
            Some(DiscoveryScope::Plugin { .. }) => self.plugin += 1,
            Some(DiscoveryScope::BuiltIn { .. }) => self.built_in += 1,
            None => self.unknown += 1,
        }
    }
}

/// Returns the canonical telemetry label for a discovery scope.
pub fn telemetry_scope_label(scope: &DiscoveryScope) -> &'static str {
    match scope {
        DiscoveryScope::Project { .. } => "project",
        DiscoveryScope::CliJson { .. } => "cli",
        DiscoveryScope::User { .. } => "user",
        DiscoveryScope::Plugin { .. } => "plugin",
        DiscoveryScope::BuiltIn { .. } => "built_in",
    }
}

/// Outcome returned after refreshing the registry.
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    /// Counts describing the manifests stored in the registry.
    pub report: RefreshReport,
    /// Validation issues encountered while loading manifests.
    pub issues: Vec<RefreshIssue>,
}

impl RefreshOutcome {
    /// Returns the number of validation issues captured during refresh.
    #[must_use]
    pub fn issue_count(&self) -> usize {
        self.issues.len()
    }
}

/// Structured validation issue emitted during refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshIssue {
    /// Optional file path associated with the issue.
    pub path: Option<PathBuf>,
    /// Discovery scope linked to the invalid manifest.
    pub scope: Option<DiscoveryScope>,
    /// Human-readable error message.
    pub message: String,
}

impl RefreshIssue {
    /// Provides a best-effort scope label for diagnostics.
    #[must_use]
    pub fn scope_label(&self) -> Option<String> {
        self.scope.as_ref().map(|scope| match scope {
            DiscoveryScope::Project { path } => format!("project ({})", path.display()),
            DiscoveryScope::CliJson { label } => {
                format!("cli ({})", label.clone().unwrap_or_else(|| "inline".into()))
            }
            DiscoveryScope::User { path } => format!("user ({})", path.display()),
            DiscoveryScope::Plugin { plugin_id, .. } => format!("plugin: {}", plugin_id.as_str()),
            DiscoveryScope::BuiltIn { agent } => format!("built-in ({agent:?})"),
        })
    }

    /// Formats the path (if present) for logging or UI surfaces.
    #[must_use]
    pub fn path_label(&self) -> Option<String> {
        self.path.as_ref().map(|path| path.display().to_string())
    }
}

impl From<LoaderIssue> for RefreshIssue {
    fn from(issue: LoaderIssue) -> Self {
        Self {
            path: issue.path,
            scope: issue.scope,
            message: issue.message,
        }
    }
}

/// Summary of manifest categories used for diagnostics and logging.
#[derive(Debug, Clone, Copy, Default)]
pub struct ManifestCounts {
    /// Manifests authored by users, plugins, or CLI overrides.
    pub custom: usize,
    /// Built-in manifests bundled with Codex.
    pub built_in: usize,
}

impl ManifestCounts {
    /// Returns the total number of manifests represented by this summary.
    #[must_use]
    pub fn total(&self) -> usize {
        self.custom + self.built_in
    }
}

#[allow(dead_code)]
impl AgentRegistry {
    /// Loads manifests from the default discovery targets rooted at `cwd`.
    pub fn load_from_default_targets(cwd: &Path) -> Result<Self, ManifestError> {
        Self::load_from_default_targets_with_telemetry(cwd, None, RefreshInvocation::Unknown)
    }

    /// Loads manifests from the default discovery targets rooted at `cwd`, emitting telemetry.
    pub fn load_from_default_targets_with_telemetry(
        cwd: &Path,
        otel: Option<&OtelManager>,
        invocation: RefreshInvocation,
    ) -> Result<Self, ManifestError> {
        let loader: Arc<dyn ManifestLoader> = Arc::new(FsManifestLoader::new());
        let mut registry = AgentRegistry::new(loader);
        let targets = AgentRegistry::default_targets(cwd);
        registry.refresh_with_telemetry(&targets, otel, invocation)?;
        Ok(registry)
    }

    /// Loads manifests from the default discovery targets and enables watch mode.
    pub fn load_from_default_targets_with_watch(
        cwd: &Path,
        otel: Option<&OtelManager>,
        invocation: RefreshInvocation,
        config: AgentRegistryWatchConfig,
    ) -> Result<(Arc<RwLock<AgentRegistry>>, AgentRegistryWatch), ManifestError> {
        let loader: Arc<dyn ManifestLoader> = Arc::new(FsManifestLoader::new());
        let mut registry = AgentRegistry::new(loader);
        let targets = AgentRegistry::default_targets(cwd);
        registry.refresh_with_telemetry(&targets, otel, invocation)?;
        let shared = Arc::new(RwLock::new(registry));
        let watch = AgentRegistry::start_watch(Arc::clone(&shared), targets, otel, config)?;
        Ok((shared, watch))
    }

    /// Starts a background watch worker for an existing registry.
    pub fn start_watch(
        registry: Arc<RwLock<AgentRegistry>>,
        targets: Vec<DiscoveryTarget>,
        otel: Option<&OtelManager>,
        config: AgentRegistryWatchConfig,
    ) -> Result<AgentRegistryWatch, ManifestError> {
        let loader = {
            let guard = registry
                .read()
                .map_err(|_| ManifestError::Inline("agent registry lock poisoned".into()))?;
            Arc::clone(&guard.loader)
        };
        let watch = loader.watch(&targets)?;
        AgentRegistryWatch::spawn(
            Arc::downgrade(&registry),
            watch,
            targets,
            otel.cloned(),
            config,
        )
    }

    /// Creates a new registry backed by the provided loader.
    pub fn new(loader: Arc<dyn ManifestLoader>) -> Self {
        Self {
            loader,
            manifests: Vec::new(),
            index: HashMap::new(),
            last_outcome: None,
        }
    }

    /// Returns the number of manifests currently tracked.
    pub fn len(&self) -> usize {
        self.manifests.len()
    }

    /// Whether any manifests have been loaded.
    pub fn is_empty(&self) -> bool {
        self.manifests.is_empty()
    }

    /// Reloads manifests for the given discovery targets, replacing the previous
    /// registry contents while preserving the loader's priority ordering.
    pub fn refresh(
        &mut self,
        targets: &[DiscoveryTarget],
    ) -> Result<RefreshOutcome, ManifestError> {
        let load = self.loader.load(targets)?;
        let manifests = load.manifests;
        self.manifests.clear();
        self.index.clear();

        let mut deduped = Vec::new();
        let mut seen = HashSet::new();
        let mut skipped_duplicates = 0usize;

        for manifest in manifests {
            if seen.insert(manifest.id.clone()) {
                deduped.push(manifest);
            } else {
                skipped_duplicates += 1;
            }
        }

        let mut built_in_count = 0usize;
        for built_in in built_in_manifests() {
            if seen.insert(built_in.id.clone()) {
                built_in_count += 1;
                deduped.push(built_in);
            } else {
                skipped_duplicates += 1;
            }
        }

        for manifest in deduped {
            let id = manifest.id.clone();
            let arc = Arc::new(manifest);
            self.index.insert(id, Arc::clone(&arc));
            self.manifests.push(arc);
        }

        let mut scope_breakdown = ScopeBreakdown::default();
        for manifest in &self.manifests {
            scope_breakdown.record(manifest.source.as_ref());
        }

        let report = RefreshReport {
            total_manifests: self.manifests.len(),
            built_in_manifests: built_in_count,
            skipped_duplicates,
            scope_breakdown,
        };
        let outcome = RefreshOutcome {
            report,
            issues: load.issues.into_iter().map(RefreshIssue::from).collect(),
        };
        self.last_outcome = Some(outcome.clone());
        Ok(outcome)
    }

    /// Refreshes manifests and emits telemetry describing the outcome.
    pub fn refresh_with_telemetry(
        &mut self,
        targets: &[DiscoveryTarget],
        otel: Option<&OtelManager>,
        invocation: RefreshInvocation,
    ) -> Result<RefreshOutcome, ManifestError> {
        match self.refresh(targets) {
            Ok(outcome) => {
                emit_refresh_telemetry(
                    otel,
                    invocation,
                    RefreshStatus::Success,
                    Some(&outcome.report),
                    &outcome.issues,
                    None,
                );
                Ok(outcome)
            }
            Err(err) => {
                let message = err.to_string();
                emit_refresh_telemetry(
                    otel,
                    invocation,
                    RefreshStatus::Failure,
                    None,
                    &[],
                    Some(&message),
                );
                Err(err)
            }
        }
    }

    /// Iterates over the manifests in discovery order.
    pub fn manifests(&self) -> impl Iterator<Item = Arc<AgentManifest>> + '_ {
        self.manifests.iter().cloned()
    }

    /// Returns owned copies of the manifests for registration or inspection.
    pub fn manifests_snapshot(&self) -> Vec<AgentManifest> {
        self.manifests
            .iter()
            .map(|manifest| (**manifest).clone())
            .collect()
    }

    /// Computes how many manifests in the provided slice are custom vs built-in.
    pub fn manifest_counts(manifests: &[AgentManifest]) -> ManifestCounts {
        let mut counts = ManifestCounts::default();
        for manifest in manifests {
            if manifest.kind.is_built_in() {
                counts.built_in += 1;
            } else {
                counts.custom += 1;
            }
        }
        counts
    }

    /// Returns the last refresh report if available.
    pub fn last_report(&self) -> Option<RefreshReport> {
        self.last_outcome.as_ref().map(|outcome| outcome.report)
    }

    /// Returns the last refresh outcome if available.
    pub fn last_outcome(&self) -> Option<&RefreshOutcome> {
        self.last_outcome.as_ref()
    }

    /// Retrieves a manifest by identifier.
    pub fn get(&self, id: &AgentId) -> Option<Arc<AgentManifest>> {
        self.index.get(id).cloned()
    }

    /// Returns true when the registry contains a manifest identified by `agent_id`.
    pub fn has_agent(&self, agent_id: &str) -> bool {
        self.index.contains_key(&AgentId::from(agent_id))
    }

    /// Builds a runtime profile for the specified manifest identifier.
    pub fn activate(
        &self,
        agent_id: &str,
        ctx: &ActivationContext<'_>,
    ) -> Result<AgentRuntimeProfile, ActivationError> {
        let manifest =
            self.get(&AgentId::from(agent_id))
                .ok_or_else(|| ActivationError::UnknownAgent {
                    agent_id: agent_id.to_string(),
                })?;
        AgentRuntimeProfile::from_manifest(manifest, ctx)
    }

    /// Returns project/user discovery targets following the defaults outlined in
    /// docs/subagents/architecture.md (project `.claude/agents`, user
    /// `~/.claude/agents`), skipping any paths that do not exist.
    pub fn default_targets(cwd: &Path) -> Vec<DiscoveryTarget> {
        default_discovery_targets(cwd)
    }
}

/// Identifies the context that triggered an agent registry refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshInvocation {
    /// Session initialization (interactive CLI/TUI/exec).
    SessionStartup,
    /// `codex agents list --json`.
    CliListJson,
    /// `codex agents list` without `--json`.
    CliListHuman,
    /// Pre-flight validation (e.g., `--use-subagent` flag).
    EnsureAvailable,
    /// Watch-triggered refresh.
    Watch,
    /// Fallback for unclassified callers.
    Unknown,
}

impl RefreshInvocation {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStartup => "session_startup",
            Self::CliListJson => "cli_list_json",
            Self::CliListHuman => "cli_list_human",
            Self::EnsureAvailable => "ensure_available",
            Self::Watch => "watch",
            Self::Unknown => "unknown",
        }
    }
}

/// Status emitted alongside refresh telemetry.
#[derive(Debug, Clone, Copy)]
pub enum RefreshStatus {
    /// Refresh succeeded.
    Success,
    /// Refresh failed before producing a report.
    Failure,
}

impl RefreshStatus {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

/// Emits a structured telemetry event describing the outcome of a manifest refresh.
pub fn emit_refresh_telemetry(
    otel: Option<&OtelManager>,
    invocation: RefreshInvocation,
    status: RefreshStatus,
    report: Option<&RefreshReport>,
    issues: &[RefreshIssue],
    error: Option<&str>,
) {
    let (total, built_ins, duplicates, custom, scope_breakdown) = report
        .map(|report| {
            (
                report.total_manifests,
                report.built_in_manifests,
                report.skipped_duplicates,
                report.custom_manifests(),
                report.scope_breakdown,
            )
        })
        .unwrap_or_default();

    if let Some(otel) = otel {
        otel.subagent_registry_refresh(
            invocation.as_str(),
            status.as_str(),
            total,
            built_ins,
            custom,
            duplicates,
            scope_breakdown.project,
            scope_breakdown.cli,
            scope_breakdown.user,
            scope_breakdown.plugin,
            scope_breakdown.built_in,
            scope_breakdown.unknown,
            issues.len(),
            error,
        );
    } else {
        tracing::event!(
            tracing::Level::INFO,
            event.name = "codex.subagent_registry_refresh",
            invocation = invocation.as_str(),
            status = status.as_str(),
            total,
            built_in = built_ins,
            custom,
            duplicates,
            scope.project = scope_breakdown.project,
            scope.cli = scope_breakdown.cli,
            scope.user = scope_breakdown.user,
            scope.plugin = scope_breakdown.plugin,
            scope.built_in = scope_breakdown.built_in,
            scope.unknown = scope_breakdown.unknown,
            issues = issues.len(),
            error = error,
        );
    }

    for issue in issues {
        let scope = issue
            .scope
            .as_ref()
            .map(telemetry_scope_label)
            .map(String::from);
        let path = issue.path_label();
        if let Some(otel) = otel {
            otel.subagent_manifest_issue(
                invocation.as_str(),
                scope.as_deref(),
                path.as_deref(),
                &issue.message,
            );
        } else {
            tracing::event!(
                tracing::Level::WARN,
                event.name = "codex.subagent_manifest_issue",
                invocation = invocation.as_str(),
                scope = scope.as_deref(),
                path = path.as_deref(),
                message = issue.message.as_str(),
            );
        }
    }
}

const REGISTRY_WATCH_THREAD_NAME: &str = "agent-registry-watch";
const REGISTRY_WATCH_DEFAULT_DEBOUNCE: Duration = Duration::from_millis(300);
const REGISTRY_WATCH_IDLE_POLL: Duration = Duration::from_millis(50);

/// Configuration for the agent registry watch worker.
#[derive(Debug, Clone)]
pub struct AgentRegistryWatchConfig {
    /// Debounce window used to coalesce multiple loader events into a single refresh.
    pub debounce: Duration,
    /// Poll interval applied while waiting for new loader events.
    pub idle_poll_interval: Duration,
}

impl Default for AgentRegistryWatchConfig {
    fn default() -> Self {
        Self {
            debounce: REGISTRY_WATCH_DEFAULT_DEBOUNCE,
            idle_poll_interval: REGISTRY_WATCH_IDLE_POLL,
        }
    }
}

/// Watch handle that surfaces registry refresh outcomes.
pub struct AgentRegistryWatch {
    receiver: AsyncReceiver<RegistryEvent>,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl AgentRegistryWatch {
    fn spawn(
        registry: Weak<RwLock<AgentRegistry>>,
        loader_watch: LoaderWatch,
        targets: Vec<DiscoveryTarget>,
        otel: Option<OtelManager>,
        config: AgentRegistryWatchConfig,
    ) -> Result<Self, ManifestError> {
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let (sender, receiver) = async_channel::unbounded();
        let worker = WatchWorker {
            registry,
            loader_watch,
            targets,
            sender,
            otel,
            config,
        };
        let thread = thread::Builder::new()
            .name(REGISTRY_WATCH_THREAD_NAME.into())
            .spawn(move || worker.run(shutdown_rx))
            .map_err(|err| {
                ManifestError::Inline(format!("failed to start registry watch: {err}"))
            })?;
        Ok(Self {
            receiver,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        })
    }

    /// Blocks until the next registry event is available.
    pub fn recv_blocking(&self) -> Option<RegistryEvent> {
        self.receiver.recv_blocking().ok()
    }

    /// Attempts to receive a registry event without blocking.
    pub fn try_recv(&self) -> Result<RegistryEvent, AgentRegistryWatchTryRecvError> {
        match self.receiver.try_recv() {
            Ok(event) => Ok(event),
            Err(TryRecvError::Empty) => Err(AgentRegistryWatchTryRecvError::Empty),
            Err(TryRecvError::Closed) => Err(AgentRegistryWatchTryRecvError::Closed),
        }
    }

    /// Converts this watch into an async `Stream` of registry events.
    pub fn into_stream(self) -> AgentRegistryWatchEventStream {
        let this = ManuallyDrop::new(self);
        let receiver = unsafe { ptr::read(&this.receiver) };
        let shutdown = unsafe { ptr::read(&this.shutdown) };
        let thread = unsafe { ptr::read(&this.thread) };
        let inner_stream = stream::unfold(receiver, |receiver| async move {
            match receiver.recv().await {
                Ok(event) => Some((event, receiver)),
                Err(_) => None,
            }
        });
        AgentRegistryWatchEventStream {
            inner: Box::pin(inner_stream),
            shutdown,
            thread,
        }
    }

    /// Stops the watch worker and waits for it to exit.
    pub fn close(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AgentRegistryWatch {
    fn drop(&mut self) {
        self.close();
    }
}

/// Error returned when no registry watch events are immediately available.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentRegistryWatchTryRecvError {
    /// No event ready yet.
    Empty,
    /// Watch has been closed.
    Closed,
}

/// Async stream wrapper for registry watch events.
pub struct AgentRegistryWatchEventStream {
    inner: Pin<Box<dyn Stream<Item = RegistryEvent> + Send>>,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl AgentRegistryWatchEventStream {
    /// Stops the watch worker and waits for it to exit.
    pub fn close(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AgentRegistryWatchEventStream {
    fn drop(&mut self) {
        self.close();
    }
}

impl Stream for AgentRegistryWatchEventStream {
    type Item = RegistryEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.inner.as_mut().poll_next(cx)
    }
}

/// Structured event emitted by [`AgentRegistryWatch`].
#[derive(Debug, Clone)]
pub struct RegistryEvent {
    /// Invocation context for the refresh.
    pub invocation: RefreshInvocation,
    /// Discovery targets that triggered the refresh.
    pub scopes: Vec<DiscoveryTarget>,
    /// Outcome of the refresh attempt.
    pub kind: RegistryEventKind,
}

/// Result of a watch-triggered refresh.
#[derive(Debug, Clone)]
pub enum RegistryEventKind {
    /// Refresh succeeded and produced a [`RefreshOutcome`].
    RefreshSuccess { outcome: RefreshOutcome },
    /// Refresh failed with a manifest error.
    RefreshFailure { error: Arc<ManifestError> },
}

struct WatchWorker {
    registry: Weak<RwLock<AgentRegistry>>,
    loader_watch: LoaderWatch,
    targets: Vec<DiscoveryTarget>,
    sender: AsyncSender<RegistryEvent>,
    otel: Option<OtelManager>,
    config: AgentRegistryWatchConfig,
}

impl WatchWorker {
    fn run(mut self, shutdown: mpsc::Receiver<()>) {
        let invocation = RefreshInvocation::Watch;
        while let Some(scopes) = self.next_batch(&shutdown) {
            let Some(registry) = self.registry.upgrade() else {
                break;
            };
            let outcome = match registry.write() {
                Ok(mut guard) => {
                    guard.refresh_with_telemetry(&self.targets, self.otel.as_ref(), invocation)
                }
                Err(_) => {
                    tracing::error!(
                        event.name = "codex.agent_registry_watch_refresh",
                        scopes = %scope_labels(&scopes),
                        "agent registry watch lock poisoned"
                    );
                    break;
                }
            };
            match outcome {
                Ok(outcome) => {
                    self.log_success(&scopes, &outcome);
                    if self
                        .sender
                        .send_blocking(RegistryEvent {
                            invocation,
                            scopes: scopes.clone(),
                            kind: RegistryEventKind::RefreshSuccess {
                                outcome: outcome.clone(),
                            },
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    let error = Arc::new(err);
                    self.log_failure(&scopes, &error);
                    if self
                        .sender
                        .send_blocking(RegistryEvent {
                            invocation,
                            scopes: scopes.clone(),
                            kind: RegistryEventKind::RefreshFailure { error },
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    }

    fn next_batch(&mut self, shutdown: &mpsc::Receiver<()>) -> Option<Vec<DiscoveryTarget>> {
        let mut scopes = Vec::new();
        let first = self.wait_for_event(shutdown)?;
        Self::record_scope(&mut scopes, first.scope);
        let mut last_event_at = Instant::now();
        loop {
            if Self::shutdown_requested(shutdown) {
                return None;
            }
            match self.loader_watch.try_recv() {
                Ok(event) => {
                    Self::record_scope(&mut scopes, event.scope);
                    last_event_at = Instant::now();
                }
                Err(LoaderWatchTryRecvError::Closed) => break,
                Err(LoaderWatchTryRecvError::Empty) => {
                    if last_event_at.elapsed() >= self.config.debounce {
                        break;
                    }
                    thread::sleep(self.config.idle_poll_interval);
                }
            }
        }
        Some(scopes)
    }

    fn wait_for_event(&mut self, shutdown: &mpsc::Receiver<()>) -> Option<LoaderEvent> {
        loop {
            if Self::shutdown_requested(shutdown) {
                return None;
            }
            match self.loader_watch.try_recv() {
                Ok(event) => return Some(event),
                Err(LoaderWatchTryRecvError::Empty) => {
                    thread::sleep(self.config.idle_poll_interval);
                }
                Err(LoaderWatchTryRecvError::Closed) => return None,
            }
        }
    }

    fn shutdown_requested(shutdown: &mpsc::Receiver<()>) -> bool {
        match shutdown.try_recv() {
            Ok(_) | Err(MpscTryRecvError::Disconnected) => true,
            Err(MpscTryRecvError::Empty) => false,
        }
    }

    fn record_scope(scopes: &mut Vec<DiscoveryTarget>, scope: DiscoveryTarget) {
        if !scopes.iter().any(|existing| existing == &scope) {
            scopes.push(scope);
        }
    }

    fn log_success(&self, scopes: &[DiscoveryTarget], outcome: &RefreshOutcome) {
        let labels = scope_labels(scopes);
        let report = outcome.report;
        tracing::info!(
            event.name = "codex.agent_registry_watch_refresh",
            scopes = %labels,
            total = report.total_manifests,
            built_in = report.built_in_manifests,
            custom = report.custom_manifests(),
            duplicates = report.skipped_duplicates,
            issues = outcome.issues.len(),
            "registry refreshed after loader event"
        );
    }

    fn log_failure(&self, scopes: &[DiscoveryTarget], error: &ManifestError) {
        let labels = scope_labels(scopes);
        tracing::warn!(
            event.name = "codex.agent_registry_watch_refresh",
            scopes = %labels,
            error = %error,
            "registry refresh failed after loader event"
        );
    }
}

fn scope_labels(scopes: &[DiscoveryTarget]) -> String {
    scopes.iter().map(scope_label).collect::<Vec<_>>().join(",")
}

fn scope_label(scope: &DiscoveryTarget) -> String {
    match scope {
        DiscoveryTarget::ProjectDir(path) => format!("project:{}", path.display()),
        DiscoveryTarget::UserDir(path) => format!("user:{}", path.display()),
        DiscoveryTarget::PluginDir { plugin, .. } => format!("plugin:{}", plugin.as_str()),
        DiscoveryTarget::CliJson { label, .. } => {
            format!("cli:{}", label.as_deref().unwrap_or("inline"))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;
    use codex_subagent::AgentKind;
    use codex_subagent::DiscoveryScope;
    use codex_subagent::HookSet;
    use codex_subagent::LoadOutcome;
    use codex_subagent::LoaderIssue;
    use codex_subagent::ModelRef;
    use codex_subagent::PermissionMode;
    use codex_subagent::PluginId;
    use codex_subagent::ToolName;
    use codex_subagent::ToolScope;
    use codex_subagent::built_in_manifests;
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[test]
    fn refresh_populates_index_and_preserves_order() -> Result<(), ManifestError> {
        let loader = Arc::new(StubLoader::new(vec![manifest("alpha"), manifest("beta")]));
        let mut registry = AgentRegistry::new(loader.clone());
        let builtin_ids: Vec<_> = built_in_manifests()
            .into_iter()
            .map(|manifest| manifest.id)
            .collect();

        let _ = registry.refresh(&[])?;
        let ids: Vec<_> = registry.manifests().map(|m| m.id.clone()).collect();
        let mut expected = vec!["alpha".to_string(), "beta".to_string()];
        expected.extend(builtin_ids.iter().cloned());
        assert_eq!(ids, expected);
        assert_eq!(registry.len(), 2 + builtin_ids.len());

        // Update loader contents and ensure refresh replaces data.
        loader.set_manifests(vec![manifest("gamma")]);
        let _ = registry.refresh(&[])?;
        assert_eq!(registry.len(), 1 + builtin_ids.len());
        let ids: Vec<_> = registry.manifests().map(|m| m.id.clone()).collect();
        let mut expected_after = vec!["gamma".to_string()];
        expected_after.extend(builtin_ids);
        assert_eq!(ids, expected_after);

        Ok(())
    }

    #[test]
    fn get_returns_arc_for_id() -> Result<(), ManifestError> {
        let loader = Arc::new(StubLoader::new(vec![manifest("alpha")]));
        let mut registry = AgentRegistry::new(loader);
        let _ = registry.refresh(&[])?;

        let id: AgentId = "alpha".into();
        let loaded = registry.get(&id).expect("manifest present");
        assert_eq!(loaded.name, "alpha agent");
        Ok(())
    }

    #[test]
    fn refresh_appends_built_ins_and_counts_duplicates() -> Result<(), ManifestError> {
        let duplicate_id = "builtin-plan";
        let loader = Arc::new(StubLoader::new(vec![
            manifest("alpha"),
            manifest(duplicate_id),
        ]));
        let mut registry = AgentRegistry::new(loader);
        let outcome = registry.refresh(&[])?;
        let report = outcome.report;

        let expected_built_ins = built_in_manifests().len() - 1; // duplicate plan is skipped
        assert_eq!(report.built_in_manifests, expected_built_ins);
        assert_eq!(report.skipped_duplicates, 1);
        assert_eq!(report.custom_manifests(), 2);
        assert_eq!(
            report.total_manifests,
            report.custom_manifests() + expected_built_ins
        );
        assert!(registry.get(&AgentId::from("alpha")).is_some());
        assert!(
            registry.get(&AgentId::from(duplicate_id)).is_some(),
            "duplicate built-in should keep existing custom manifest"
        );
        Ok(())
    }

    #[test]
    fn manifest_counts_split_builtin_and_custom() {
        let builtin = built_in_manifests()
            .into_iter()
            .next()
            .expect("built-in manifest present");
        let manifests = vec![manifest("custom"), builtin];
        let counts = AgentRegistry::manifest_counts(&manifests);
        assert_eq!(counts.custom, 1);
        assert_eq!(counts.built_in, 1);
        assert_eq!(counts.total(), manifests.len());
    }

    #[test]
    fn refresh_surfaces_loader_issues() -> Result<(), ManifestError> {
        let scope = DiscoveryScope::Project {
            path: PathBuf::from("invalid.md"),
        };
        let loader = Arc::new(StubLoader::with_issues(
            vec![manifest("alpha")],
            vec![LoaderIssue {
                path: Some(PathBuf::from("invalid.md")),
                scope: Some(scope.clone()),
                message: "broken manifest".into(),
            }],
        ));
        let mut registry = AgentRegistry::new(loader);
        let outcome = registry.refresh(&[])?;
        assert_eq!(outcome.issues.len(), 1);
        let issue = &outcome.issues[0];
        assert_eq!(issue.path, Some(PathBuf::from("invalid.md")));
        assert_eq!(issue.scope, Some(scope));
        assert_eq!(issue.message, "broken manifest");
        assert_eq!(
            registry
                .last_outcome()
                .expect("outcome recorded")
                .issue_count(),
            1
        );
        Ok(())
    }

    #[test]
    fn refresh_scope_breakdown_counts_each_source() -> Result<(), ManifestError> {
        let built_in_expected = built_in_manifests().len();
        let manifests = vec![
            manifest_with_scope(
                "project",
                DiscoveryScope::Project {
                    path: PathBuf::from("proj.md"),
                },
            ),
            manifest_with_scope(
                "cli",
                DiscoveryScope::CliJson {
                    label: Some("inline".into()),
                },
            ),
            manifest_with_scope(
                "user",
                DiscoveryScope::User {
                    path: PathBuf::from("user.md"),
                },
            ),
            manifest_with_scope(
                "plugin",
                DiscoveryScope::Plugin {
                    path: PathBuf::from("plugin"),
                    plugin_id: PluginId::new("demo"),
                },
            ),
        ];
        let loader = Arc::new(StubLoader::new(manifests));
        let mut registry = AgentRegistry::new(loader);
        let outcome = registry.refresh(&[])?;
        let scope_breakdown = outcome.report.scope_breakdown;
        assert_eq!(scope_breakdown.project, 1);
        assert_eq!(scope_breakdown.cli, 1);
        assert_eq!(scope_breakdown.user, 1);
        assert_eq!(scope_breakdown.plugin, 1);
        assert_eq!(scope_breakdown.built_in, built_in_expected);
        assert_eq!(scope_breakdown.unknown, 0);
        Ok(())
    }

    #[test]
    fn activate_builds_runtime_profile_with_overrides() -> Result<(), ManifestError> {
        let mut custom = manifest("alpha");
        custom.model = Some(ModelRef("claude-3-haiku".into()));
        custom.permission_mode = PermissionMode::DontAsk;
        custom.tool_scope =
            ToolScope::restricted(vec![ToolName::from("read"), ToolName::from("search")]);
        let loader = Arc::new(StubLoader::new(vec![custom]));
        let mut registry = AgentRegistry::new(loader);
        registry.refresh(&[])?;

        let tools = vec!["read".to_string(), "apply_patch".to_string()];
        let ctx = ActivationContext::new("parent-model", AskForApproval::OnRequest, &tools);

        let profile = registry.activate("alpha", &ctx).expect("runtime profile");
        assert_eq!(profile.model(), "claude-3-haiku");
        assert_eq!(profile.approval_policy(), AskForApproval::Never);
        assert_eq!(
            profile.allowed_tools(),
            &["read".to_string(), "search".to_string()]
        );
        assert!(matches!(
            profile.session_source(),
            SessionSource::SubAgent(SubAgentSource::Other(label)) if label == "alpha"
        ));
        Ok(())
    }

    #[test]
    fn activate_maps_built_in_to_named_subagent_source() -> Result<(), ManifestError> {
        let loader = Arc::new(StubLoader::new(Vec::new()));
        let mut registry = AgentRegistry::new(loader);
        registry.refresh(&[])?;
        let tools = vec!["read".to_string()];
        let ctx = ActivationContext::new("parent-model", AskForApproval::OnRequest, &tools);
        let profile = registry
            .activate("builtin-plan", &ctx)
            .expect("built-in profile");
        assert!(matches!(
            profile.session_source(),
            SessionSource::SubAgent(SubAgentSource::Plan)
        ));
        assert_eq!(profile.model(), "claude-3.5-sonnet");
        Ok(())
    }

    fn manifest(id: &str) -> AgentManifest {
        AgentManifest {
            id: id.into(),
            kind: AgentKind::Custom,
            name: format!("{id} agent"),
            description: format!("{id} description"),
            model: None,
            tool_scope: ToolScope::default(),
            permission_mode: PermissionMode::Default,
            hooks: HookSet::default(),
            triggers: Vec::new(),
            skills: Vec::new(),
            body: "Prompt".into(),
            source: Some(DiscoveryScope::Project {
                path: PathBuf::from(format!("{id}.md")),
            }),
            digest: Some(format!("digest-{id}")),
        }
    }

    fn manifest_with_scope(id: &str, scope: DiscoveryScope) -> AgentManifest {
        let mut manifest = manifest(id);
        manifest.source = Some(scope);
        manifest
    }

    #[derive(Debug)]
    struct StubLoader {
        manifests: Mutex<Vec<AgentManifest>>,
        issues: Mutex<Vec<LoaderIssue>>,
    }

    impl StubLoader {
        fn new(manifests: Vec<AgentManifest>) -> Self {
            Self {
                manifests: Mutex::new(manifests),
                issues: Mutex::new(Vec::new()),
            }
        }

        fn with_issues(manifests: Vec<AgentManifest>, issues: Vec<LoaderIssue>) -> Self {
            Self {
                manifests: Mutex::new(manifests),
                issues: Mutex::new(issues),
            }
        }

        fn set_manifests(&self, manifests: Vec<AgentManifest>) {
            *self.manifests.lock().expect("poisoned mutex") = manifests;
        }
    }

    impl ManifestLoader for StubLoader {
        fn load(&self, _targets: &[DiscoveryTarget]) -> Result<LoadOutcome, ManifestError> {
            Ok(LoadOutcome {
                manifests: self.manifests.lock().expect("poisoned mutex").clone(),
                issues: self.issues.lock().expect("poisoned mutex").clone(),
            })
        }
    }
}
