//! File-system backed manifest loader.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::mem::ManuallyDrop;
use std::mem::{self};
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::ptr;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use async_channel::Receiver as AsyncReceiver;
use async_channel::Sender as AsyncSender;
use async_channel::TryRecvError;
use async_stream::stream;
use dunce::canonicalize as normalize_watch_path;
use futures::stream::Stream;
use notify::Config as NotifyConfig;
use notify::Event as NotifyEvent;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::warn;
use walkdir::WalkDir;

use crate::error::ManifestError;
use crate::manifest::compute_digest;
use crate::manifest::AgentManifest;
use crate::priority::DiscoveryPriority;
use crate::priority::DiscoveryScope;
use crate::priority::PluginId;
use crate::validation::validate_manifest;

/// Event sent by loaders when they detect a file system change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderEvent {
    /// Scope that triggered the change.
    pub scope: DiscoveryTarget,
}

/// Handle returned by [`ManifestLoader::watch`] that exposes blocking and async-friendly APIs.
pub struct LoaderWatch {
    receiver: AsyncReceiver<LoaderEvent>,
    thread: Option<WatchThreadHandle>,
}

impl LoaderWatch {
    /// Blocks the current thread until the next loader event is available.
    ///
    /// Returns `None` when the watch has been closed.
    pub fn recv_blocking(&self) -> Option<LoaderEvent> {
        self.receiver.recv_blocking().ok()
    }

    /// Attempts to receive the next loader event without blocking.
    pub fn try_recv(&self) -> Result<LoaderEvent, LoaderWatchTryRecvError> {
        match self.receiver.try_recv() {
            Ok(event) => Ok(event),
            Err(TryRecvError::Empty) => Err(LoaderWatchTryRecvError::Empty),
            Err(TryRecvError::Closed) => Err(LoaderWatchTryRecvError::Closed),
        }
    }

    /// Converts this watch into a [`Stream`] so async callers can await change notifications.
    pub fn into_stream(self) -> LoaderWatchEventStream {
        let this = ManuallyDrop::new(self);
        let receiver = unsafe { ptr::read(&this.receiver) };
        let thread = unsafe { ptr::read(&this.thread) };
        let inner_stream = stream! {
            let rx = receiver;
            while let Ok(event) = rx.recv().await {
                yield event;
            }
        };
        LoaderWatchEventStream {
            inner: Box::pin(inner_stream),
            thread,
        }
    }

    /// Closes the watch and waits for the underlying watcher thread to exit.
    pub fn close(&mut self) {
        if let Some(handle) = self.thread.take() {
            handle.close();
        }
    }

    /// Creates a [`LoaderWatch`] backed by the provided receiver.
    ///
    /// This is intended for tests so higher layers can inject synthetic watch events without
    /// spawning an OS file watcher.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn from_receiver_for_tests(receiver: AsyncReceiver<LoaderEvent>) -> Self {
        Self {
            receiver,
            thread: None,
        }
    }
}

impl Drop for LoaderWatch {
    fn drop(&mut self) {
        self.close();
    }
}

/// Error returned by [`LoaderWatch::try_recv`] when no event is immediately available.
#[derive(Debug, PartialEq, Eq)]
pub enum LoaderWatchTryRecvError {
    /// No event was ready but the watch is still active.
    Empty,
    /// The watch has been closed and no more events will arrive.
    Closed,
}

impl std::fmt::Display for LoaderWatchTryRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("no loader events are ready"),
            Self::Closed => f.write_str("loader watch closed"),
        }
    }
}

impl std::error::Error for LoaderWatchTryRecvError {}

/// Stream wrapper returned by [`LoaderWatch::into_stream`].
pub struct LoaderWatchEventStream {
    inner: Pin<Box<dyn Stream<Item = LoaderEvent> + Send>>,
    thread: Option<WatchThreadHandle>,
}

impl Unpin for LoaderWatchEventStream {}

impl LoaderWatchEventStream {
    /// Closes the watch and waits for the underlying watcher thread to exit.
    pub fn close(&mut self) {
        if let Some(handle) = self.thread.take() {
            handle.close();
        }
    }
}

impl Drop for LoaderWatchEventStream {
    fn drop(&mut self) {
        self.close();
    }
}

impl Stream for LoaderWatchEventStream {
    type Item = LoaderEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.inner.as_mut().poll_next(cx)
    }
}

/// Validation issue captured while loading manifests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoaderIssue {
    /// Path associated with the validation error.
    pub path: Option<PathBuf>,
    /// Discovery scope for the manifest.
    pub scope: Option<DiscoveryScope>,
    /// Human-readable message.
    pub message: String,
}

/// Outcome returned by manifest loaders.
#[derive(Clone, Debug, Default)]
pub struct LoadOutcome {
    /// Successfully parsed manifests.
    pub manifests: Vec<AgentManifest>,
    /// Validation issues encountered during loading.
    pub issues: Vec<LoaderIssue>,
}

/// Abstraction over the manifest discovery stack described in docs/subagents/architecture.md.
pub trait ManifestLoader: Send + Sync {
    /// Loads manifests from the provided discovery targets.
    fn load(&self, targets: &[DiscoveryTarget]) -> Result<LoadOutcome, ManifestError>;

    /// Starts watching the given targets for changes.
    ///
    /// Inline CLI overrides emit a single synthetic event because their payloads are static.
    fn watch(&self, targets: &[DiscoveryTarget]) -> Result<LoaderWatch, ManifestError> {
        let _ = targets;
        Err(ManifestError::WatchUnsupported)
    }
}

/// File-system backed implementation that understands Markdown + front matter, YAML, and JSON
/// manifests as described in `docs/subagents/architecture.md`.
#[derive(Debug, Default)]
pub struct FsManifestLoader;

impl FsManifestLoader {
    /// Creates a new loader.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    fn parse_json_payload(
        &self,
        payload: &Value,
        scope: DiscoveryScope,
    ) -> Result<AgentManifest, ManifestError> {
        let cli_path = PathBuf::from("<cli>");
        let mut manifest: AgentManifest =
            serde_json::from_value(payload.clone()).map_err(|err| ManifestError::Parse {
                path: cli_path.clone(),
                source: Box::new(err),
            })?;
        manifest.source = Some(scope);
        let serialized = serde_json::to_vec(payload).map_err(|err| ManifestError::Parse {
            path: cli_path.clone(),
            source: Box::new(err),
        })?;
        manifest.digest = Some(compute_digest(&serialized));
        self.validate(cli_path, manifest)
    }

    fn validate(
        &self,
        path: PathBuf,
        manifest: AgentManifest,
    ) -> Result<AgentManifest, ManifestError> {
        validate_manifest(&manifest).map_err(|issues| ManifestError::Validation {
            path: path.clone(),
            issues,
        })?;
        Ok(manifest)
    }

    fn parse_file(
        &self,
        path: &Path,
        scope: DiscoveryScope,
    ) -> Result<AgentManifest, ManifestError> {
        let bytes = fs::read(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let digest = compute_digest(&bytes);
        let format = manifest_format(path).ok_or_else(|| {
            ManifestError::Inline(format!(
                "unsupported manifest extension: {}",
                path.display()
            ))
        })?;
        let mut manifest = match format {
            ManifestFormat::Markdown => {
                parse_markdown(&bytes).map_err(|err| ManifestError::Parse {
                    path: path.to_path_buf(),
                    source: err,
                })?
            }
            ManifestFormat::Yaml => parse_yaml::<AgentManifest>(&bytes, path)?,
            ManifestFormat::Json => parse_json::<AgentManifest>(&bytes, path)?,
        };
        manifest.source = Some(scope);
        manifest.digest = Some(digest);
        self.validate(path.to_path_buf(), manifest)
    }

    fn parse_cli_manifest_file(
        &self,
        path: &Path,
        scope: DiscoveryScope,
    ) -> Result<AgentManifest, ManifestError> {
        let bytes = fs::read(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut manifest: AgentManifest =
            serde_json::from_slice(&bytes).map_err(|err| ManifestError::Parse {
                path: path.to_path_buf(),
                source: Box::new(err),
            })?;
        manifest.source = Some(scope);
        manifest.digest = Some(compute_digest(&bytes));
        self.validate(path.to_path_buf(), manifest)
    }

    fn read_directory<F>(
        &self,
        path: &Path,
        scope_builder: F,
        issues: &mut Vec<LoaderIssue>,
    ) -> Result<Vec<AgentManifest>, ManifestError>
    where
        F: Fn(&Path) -> DiscoveryScope,
    {
        let mut collected = Vec::new();
        let metadata = fs::metadata(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        if metadata.is_file() {
            let scope = scope_builder(path);
            match self.parse_file(path, scope.clone()) {
                Ok(manifest) => collected.push(manifest),
                Err(err) => {
                    record_validation_issue(err, Some(scope), issues)?;
                }
            }
            return Ok(collected);
        }

        for entry in WalkDir::new(path).max_depth(1) {
            let entry = entry.map_err(|err| {
                let description = err.to_string();
                let io_err = match err.into_io_error() {
                    Some(inner) => inner,
                    None => io::Error::other(description),
                };
                ManifestError::Io {
                    path: path.to_path_buf(),
                    source: io_err,
                }
            })?;
            if entry.path() == path {
                continue;
            }
            if !entry.file_type().is_file() || !is_supported(entry.path()) {
                continue;
            }
            let scope = scope_builder(entry.path());
            match self.parse_file(entry.path(), scope.clone()) {
                Ok(manifest) => collected.push(manifest),
                Err(err) => {
                    record_validation_issue(err, Some(scope), issues)?;
                }
            }
        }

        Ok(collected)
    }
}

impl ManifestLoader for FsManifestLoader {
    fn load(&self, targets: &[DiscoveryTarget]) -> Result<LoadOutcome, ManifestError> {
        let mut manifests = Vec::new();
        let mut ids: HashMap<String, DiscoveryScope> = HashMap::new();
        let mut issues = Vec::new();

        for target in targets {
            match target {
                DiscoveryTarget::CliJson { manifest, label } => {
                    let scope = DiscoveryScope::CliJson {
                        label: label.clone(),
                    };
                    match self.parse_json_payload(manifest, scope.clone()) {
                        Ok(manifest) => {
                            push_manifest(manifest, &mut manifests, &mut ids, &mut issues)?;
                        }
                        Err(err) => {
                            record_validation_issue(err, Some(scope), &mut issues)?;
                        }
                    }
                }
                DiscoveryTarget::CliManifestFile { path, label } => {
                    let scope = DiscoveryScope::CliJson {
                        label: label.clone(),
                    };
                    match self.parse_cli_manifest_file(path, scope.clone()) {
                        Ok(manifest) => {
                            push_manifest(manifest, &mut manifests, &mut ids, &mut issues)?;
                        }
                        Err(err) => {
                            record_validation_issue(err, Some(scope), &mut issues)?;
                        }
                    }
                }
                DiscoveryTarget::ProjectDir(path) => {
                    let scoped = self.read_directory(
                        path,
                        |p| DiscoveryScope::Project {
                            path: p.to_path_buf(),
                        },
                        &mut issues,
                    )?;
                    for manifest in scoped {
                        push_manifest(manifest, &mut manifests, &mut ids, &mut issues)?;
                    }
                }
                DiscoveryTarget::UserDir(path) => {
                    let scoped = self.read_directory(
                        path,
                        |p| DiscoveryScope::User {
                            path: p.to_path_buf(),
                        },
                        &mut issues,
                    )?;
                    for manifest in scoped {
                        push_manifest(manifest, &mut manifests, &mut ids, &mut issues)?;
                    }
                }
                DiscoveryTarget::PluginDir { path, plugin } => {
                    let plugin_id = plugin.clone();
                    let scoped = self.read_directory(
                        path,
                        |p| DiscoveryScope::Plugin {
                            path: p.to_path_buf(),
                            plugin_id: plugin_id.clone(),
                        },
                        &mut issues,
                    )?;
                    for manifest in scoped {
                        push_manifest(manifest, &mut manifests, &mut ids, &mut issues)?;
                    }
                }
            }
        }

        manifests.sort_by(|a, b| {
            b.priority()
                .cmp(&a.priority())
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(LoadOutcome { manifests, issues })
    }

    fn watch(&self, targets: &[DiscoveryTarget]) -> Result<LoaderWatch, ManifestError> {
        start_watch(targets)
    }
}

fn guard_duplicate(
    ids: &mut HashMap<String, DiscoveryScope>,
    manifest: &AgentManifest,
) -> Result<(), ManifestError> {
    let scope = manifest
        .source
        .clone()
        .ok_or_else(|| ManifestError::Inline(format!("manifest {} missing source", manifest.id)))?;
    match ids.entry(manifest.id.clone()) {
        Entry::Vacant(entry) => {
            entry.insert(scope);
            Ok(())
        }
        Entry::Occupied(existing) => Err(ManifestError::DuplicateId {
            agent_id: manifest.id.clone(),
            first: existing.get().clone(),
            second: scope,
        }),
    }
}

fn push_manifest(
    manifest: AgentManifest,
    manifests: &mut Vec<AgentManifest>,
    ids: &mut HashMap<String, DiscoveryScope>,
    issues: &mut Vec<LoaderIssue>,
) -> Result<(), ManifestError> {
    match guard_duplicate(ids, &manifest) {
        Ok(()) => {
            manifests.push(manifest);
            Ok(())
        }
        Err(err) => {
            record_duplicate_issue(err, issues)?;
            Ok(())
        }
    }
}

fn is_supported(path: &Path) -> bool {
    manifest_format(path).is_some()
}

pub(crate) fn parse_markdown(
    bytes: &[u8],
) -> Result<AgentManifest, Box<dyn std::error::Error + Send + Sync>> {
    let contents = String::from_utf8(bytes.to_vec())?;
    let (front_matter, body) = split_front_matter(&contents)?;
    let mut manifest: AgentManifest = serde_yaml::from_str(&front_matter)?;
    manifest.body = body.trim_start().to_owned();
    Ok(manifest)
}

fn parse_yaml<T>(bytes: &[u8], path: &Path) -> Result<T, ManifestError>
where
    T: DeserializeOwned,
{
    serde_yaml::from_slice(bytes).map_err(|err| ManifestError::Parse {
        path: path.to_path_buf(),
        source: Box::new(err),
    })
}

fn parse_json<T>(bytes: &[u8], path: &Path) -> Result<T, ManifestError>
where
    T: DeserializeOwned,
{
    serde_json::from_slice(bytes).map_err(|err| ManifestError::Parse {
        path: path.to_path_buf(),
        source: Box::new(err),
    })
}

enum ManifestFormat {
    Markdown,
    Json,
    Yaml,
}

fn manifest_format(path: &Path) -> Option<ManifestFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" => Some(ManifestFormat::Markdown),
        "json" => Some(ManifestFormat::Json),
        "yaml" | "yml" => Some(ManifestFormat::Yaml),
        _ => None,
    }
}

pub(crate) fn split_front_matter(
    contents: &str,
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    let mut lines = contents.lines();
    match lines.next() {
        Some(line) if line.trim() == "---" => {}
        _ => {
            return Err("missing YAML front matter delimiter".into());
        }
    }

    let mut front = String::new();
    let mut closing_found = false;
    for line in &mut lines {
        if line.trim() == "---" {
            closing_found = true;
            break;
        }
        front.push_str(line);
        front.push('\n');
    }

    if !closing_found {
        return Err("missing closing YAML front matter delimiter".into());
    }

    let body = lines.collect::<Vec<_>>().join("\n");
    Ok((front.trim().to_owned(), body))
}

/// Discovery sources surfaced by the loader API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryTarget {
    /// Directory or file inside the project tree.
    ProjectDir(PathBuf),
    /// Inline CLI JSON manifest.
    CliJson {
        /// Manifest payload provided by the CLI.
        manifest: Value,
        /// Optional diagnostic label (e.g., flag alias).
        label: Option<String>,
    },
    /// File-backed CLI manifest override.
    CliManifestFile {
        /// Path to the manifest file.
        path: PathBuf,
        /// Optional diagnostic label.
        label: Option<String>,
    },
    /// User-level manifests.
    UserDir(PathBuf),
    /// Plugin-managed manifests.
    PluginDir {
        /// Path to the manifest.
        path: PathBuf,
        /// Identifier for the plugin providing the manifest.
        plugin: PluginId,
    },
}

impl DiscoveryTarget {
    /// Priority tier for this target.
    #[must_use]
    pub fn priority(&self) -> DiscoveryPriority {
        match self {
            Self::ProjectDir(_) => DiscoveryPriority::Project,
            Self::CliJson { .. } => DiscoveryPriority::Cli,
            Self::CliManifestFile { .. } => DiscoveryPriority::Cli,
            Self::UserDir(_) => DiscoveryPriority::User,
            Self::PluginDir { .. } => DiscoveryPriority::Plugin,
        }
    }
}

const WATCH_THREAD_NAME: &str = "manifest-watch";
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(500);
const WATCH_DEBOUNCE_WINDOW: Duration = Duration::from_millis(250);

fn start_watch(targets: &[DiscoveryTarget]) -> Result<LoaderWatch, ManifestError> {
    let (sender, receiver) = async_channel::unbounded();
    let mut watch_configs = Vec::new();

    for target in targets {
        match target {
            DiscoveryTarget::CliJson { .. } => enqueue_cli_scope(&sender, target.clone()),
            DiscoveryTarget::CliManifestFile { .. }
            | DiscoveryTarget::ProjectDir(_)
            | DiscoveryTarget::UserDir(_)
            | DiscoveryTarget::PluginDir { .. } => {
                watch_configs.push(TargetWatch::new(target.clone())?)
            }
        }
    }

    let thread = WatchWorker::spawn(watch_configs, sender.clone())?;
    drop(sender);

    Ok(LoaderWatch { receiver, thread })
}

/// Inline CLI overrides are static; emit a single event so callers know the scope exists.
fn enqueue_cli_scope(sender: &AsyncSender<LoaderEvent>, scope: DiscoveryTarget) {
    let _ = sender.try_send(LoaderEvent { scope });
}

struct WatchThreadHandle {
    shutdown: mpsc::Sender<()>,
    join_handle: thread::JoinHandle<()>,
}

impl WatchThreadHandle {
    fn close(self) {
        let _ = self.shutdown.send(());
        let _ = self.join_handle.join();
    }
}

#[derive(Clone)]
struct TargetWatch {
    target: DiscoveryTarget,
    subject: WatchSubject,
    watch_root: PathBuf,
    recursive_mode: RecursiveMode,
    debounce_key: DebounceKey,
    display_path: PathBuf,
}

impl TargetWatch {
    fn new(target: DiscoveryTarget) -> Result<Self, ManifestError> {
        let (abs_path, debounce_key, hint) = match &target {
            DiscoveryTarget::ProjectDir(path) => {
                let abs = make_absolute(path)?;
                (
                    abs.clone(),
                    DebounceKey::Project(abs),
                    SubjectHint::InferFromPath,
                )
            }
            DiscoveryTarget::UserDir(path) => {
                let abs = make_absolute(path)?;
                (
                    abs.clone(),
                    DebounceKey::User(abs),
                    SubjectHint::InferFromPath,
                )
            }
            DiscoveryTarget::PluginDir { path, plugin } => {
                let abs = make_absolute(path)?;
                (
                    abs.clone(),
                    DebounceKey::Plugin {
                        path: abs,
                        plugin: plugin.clone(),
                    },
                    SubjectHint::Directory,
                )
            }
            DiscoveryTarget::CliManifestFile { path, .. } => {
                let abs = make_absolute(path)?;
                (abs.clone(), DebounceKey::Cli(abs), SubjectHint::File)
            }
            DiscoveryTarget::CliJson { .. } => {
                return Err(ManifestError::Inline(
                    "cli discovery targets do not support watch registration".into(),
                ));
            }
        };
        let SubjectConfig {
            subject,
            watch_root,
            recursive_mode,
            display_path,
        } = classify_subject(&abs_path, hint)?;
        Ok(Self {
            target,
            subject,
            watch_root,
            recursive_mode,
            debounce_key,
            display_path,
        })
    }
}

#[derive(Clone)]
enum WatchSubject {
    Directory(PathBuf),
    File(PathBuf),
}

impl WatchSubject {
    fn matches(&self, path: &Path) -> bool {
        match self {
            Self::Directory(root) => path.starts_with(root),
            Self::File(expected) => path == expected,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DebounceKey {
    Project(PathBuf),
    User(PathBuf),
    Plugin { path: PathBuf, plugin: PluginId },
    Cli(PathBuf),
}

enum SubjectHint {
    Directory,
    InferFromPath,
    File,
}

struct SubjectConfig {
    subject: WatchSubject,
    watch_root: PathBuf,
    recursive_mode: RecursiveMode,
    display_path: PathBuf,
}

struct WatchWorker {
    sender: AsyncSender<LoaderEvent>,
    configs: Vec<TargetWatch>,
    debounce: Arc<Mutex<HashMap<DebounceKey, Instant>>>,
    watchers: Vec<RecommendedWatcher>,
}

impl WatchWorker {
    fn spawn(
        configs: Vec<TargetWatch>,
        sender: AsyncSender<LoaderEvent>,
    ) -> Result<Option<WatchThreadHandle>, ManifestError> {
        if configs.is_empty() {
            return Ok(None);
        }

        let (ready_tx, ready_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name(WATCH_THREAD_NAME.into())
            .spawn(move || {
                let mut worker = WatchWorker::new(sender, configs);
                worker.run(ready_tx, shutdown_rx);
            })
            .map_err(|err| {
                ManifestError::Inline(format!("failed to spawn manifest watch worker: {err}"))
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Some(WatchThreadHandle {
                shutdown: shutdown_tx,
                join_handle: handle,
            })),
            Ok(Err(err)) => {
                let _ = shutdown_tx.send(());
                let _ = handle.join();
                Err(err)
            }
            Err(err) => {
                let _ = shutdown_tx.send(());
                let _ = handle.join();
                Err(ManifestError::Inline(format!(
                    "manifest watch worker exited early: {err}"
                )))
            }
        }
    }

    fn new(sender: AsyncSender<LoaderEvent>, configs: Vec<TargetWatch>) -> Self {
        Self {
            sender,
            configs,
            debounce: Arc::new(Mutex::new(HashMap::new())),
            watchers: Vec::new(),
        }
    }

    fn run(
        &mut self,
        ready_tx: mpsc::Sender<Result<(), ManifestError>>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        let init_result = self.start();
        let success = init_result.is_ok();
        let _ = ready_tx.send(init_result);
        if !success {
            return;
        }
        let _ = shutdown_rx.recv();
    }

    fn start(&mut self) -> Result<(), ManifestError> {
        for config in mem::take(&mut self.configs) {
            let watcher = self.spawn_single_watcher(config)?;
            self.watchers.push(watcher);
        }
        Ok(())
    }

    fn spawn_single_watcher(
        &self,
        config: TargetWatch,
    ) -> Result<RecommendedWatcher, ManifestError> {
        let TargetWatch {
            target,
            subject,
            watch_root,
            recursive_mode,
            debounce_key,
            display_path,
        } = config;
        let sender = self.sender.clone();
        let debounce = Arc::clone(&self.debounce);
        let mut watcher = notify::recommended_watcher(move |event| {
            handle_notify_event(&sender, &subject, &target, &debounce, &debounce_key, event);
        })
        .map_err(|err| ManifestError::WatchSetup {
            path: display_path.clone(),
            source: Box::new(err),
        })?;
        watcher
            .configure(NotifyConfig::default().with_poll_interval(WATCH_POLL_INTERVAL))
            .map_err(|err| ManifestError::WatchSetup {
                path: display_path.clone(),
                source: Box::new(err),
            })?;
        watcher
            .watch(&watch_root, recursive_mode)
            .map_err(|err| ManifestError::WatchSetup {
                path: display_path,
                source: Box::new(err),
            })?;
        Ok(watcher)
    }
}

fn handle_notify_event(
    sender: &AsyncSender<LoaderEvent>,
    subject: &WatchSubject,
    target: &DiscoveryTarget,
    debounce: &Arc<Mutex<HashMap<DebounceKey, Instant>>>,
    debounce_key: &DebounceKey,
    event: Result<NotifyEvent, notify::Error>,
) {
    match event {
        Ok(event) => {
            if !event.paths.iter().any(|path| subject.matches(path)) {
                return;
            }
            if !should_emit(debounce, debounce_key) {
                return;
            }
            let _ = sender.try_send(LoaderEvent {
                scope: target.clone(),
            });
        }
        Err(err) => {
            warn!("manifest watcher error: {err}");
        }
    }
}

fn should_emit(debounce: &Arc<Mutex<HashMap<DebounceKey, Instant>>>, key: &DebounceKey) -> bool {
    let now = Instant::now();
    let mut guard = match debounce.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.get_mut(key) {
        Some(previous) => {
            if now.duration_since(*previous) < WATCH_DEBOUNCE_WINDOW {
                false
            } else {
                *previous = now;
                true
            }
        }
        None => {
            guard.insert(key.clone(), now);
            true
        }
    }
}

fn classify_subject(path: &Path, hint: SubjectHint) -> Result<SubjectConfig, ManifestError> {
    let metadata = fs::metadata(path);
    let (subject, recursive_mode) = match metadata {
        Ok(meta) if meta.is_file() || matches!(hint, SubjectHint::File) => (
            WatchSubject::File(path.to_path_buf()),
            RecursiveMode::Recursive,
        ),
        Ok(_) => (
            WatchSubject::Directory(path.to_path_buf()),
            RecursiveMode::Recursive,
        ),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if matches!(hint, SubjectHint::InferFromPath) && looks_like_manifest_file(path) {
                (
                    WatchSubject::File(path.to_path_buf()),
                    RecursiveMode::Recursive,
                )
            } else if matches!(hint, SubjectHint::File) {
                (
                    WatchSubject::File(path.to_path_buf()),
                    RecursiveMode::Recursive,
                )
            } else {
                (
                    WatchSubject::Directory(path.to_path_buf()),
                    RecursiveMode::Recursive,
                )
            }
        }
        Err(err) => {
            return Err(watch_setup_error(path.to_path_buf(), err));
        }
    };

    let watch_root = match &subject {
        WatchSubject::Directory(dir) => nearest_existing_ancestor(dir),
        WatchSubject::File(file) => {
            let parent = file
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            nearest_existing_ancestor(&parent)
        }
    };

    Ok(SubjectConfig {
        subject,
        watch_root,
        recursive_mode,
        display_path: path.to_path_buf(),
    })
}

fn make_absolute(path: &Path) -> Result<PathBuf, ManifestError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd = env::current_dir().map_err(|err| watch_setup_error(path.to_path_buf(), err))?;
        cwd.join(path)
    };
    Ok(normalize_watch_path(&absolute).unwrap_or(absolute))
}

fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path;
    loop {
        if current.exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return current.to_path_buf(),
        }
    }
}

fn looks_like_manifest_file(path: &Path) -> bool {
    manifest_format(path).is_some()
}

fn watch_setup_error(
    path: PathBuf,
    err: impl std::error::Error + Send + Sync + 'static,
) -> ManifestError {
    ManifestError::WatchSetup {
        path,
        source: Box::new(err),
    }
}

fn record_validation_issue(
    err: ManifestError,
    scope: Option<DiscoveryScope>,
    issues: &mut Vec<LoaderIssue>,
) -> Result<(), ManifestError> {
    match err {
        ManifestError::Validation {
            path,
            issues: validation,
        } => {
            if validation.as_slice().is_empty() {
                issues.push(LoaderIssue {
                    path: Some(path),
                    scope,
                    message: "manifest validation failed".into(),
                });
            } else {
                for issue in validation.into_vec() {
                    issues.push(LoaderIssue {
                        path: Some(path.clone()),
                        scope: scope.clone(),
                        message: issue.to_string(),
                    });
                }
            }
            Ok(())
        }
        other => Err(other),
    }
}

fn record_duplicate_issue(
    err: ManifestError,
    issues: &mut Vec<LoaderIssue>,
) -> Result<(), ManifestError> {
    match err {
        ManifestError::DuplicateId {
            agent_id,
            first,
            second,
        } => {
            let skipped_scope = second;
            issues.push(LoaderIssue {
                path: scope_path(&skipped_scope),
                scope: Some(skipped_scope.clone()),
                message: format!(
                    "agent id {agent_id} skipped: {} conflicts with {}",
                    scope_summary(&skipped_scope),
                    scope_summary(&first),
                ),
            });
            Ok(())
        }
        other => Err(other),
    }
}

fn scope_summary(scope: &DiscoveryScope) -> String {
    match scope {
        DiscoveryScope::Project { path } => {
            format!("project ({})", path.display())
        }
        DiscoveryScope::CliJson { label } => {
            format!("cli ({})", label.clone().unwrap_or_else(|| "inline".into()))
        }
        DiscoveryScope::User { path } => format!("user ({})", path.display()),
        DiscoveryScope::Plugin { path, plugin_id } => {
            format!("plugin {} ({})", plugin_id.as_str(), path.display())
        }
        DiscoveryScope::BuiltIn { agent } => format!("built-in ({agent:?})"),
    }
}

fn scope_path(scope: &DiscoveryScope) -> Option<PathBuf> {
    match scope {
        DiscoveryScope::Project { path }
        | DiscoveryScope::User { path }
        | DiscoveryScope::Plugin { path, .. } => Some(path.clone()),
        DiscoveryScope::CliJson { .. } | DiscoveryScope::BuiltIn { .. } => None,
    }
}

#[cfg(all(test, feature = "loader"))]
mod tests {
    use super::*;
    use crate::PermissionMode;
    use crate::ToolName;
    use crate::ToolScope;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn sorts_by_priority() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let project = dir.path().join("project.md");
        fs::write(
            &project,
            b"---\nid: project\nname: Project\ndescription: P\nbody: text\n---\nBody",
        )?;
        let user = dir.path().join("user.md");
        fs::write(
            &user,
            b"---\nid: user\nname: User\ndescription: U\nbody: text\n---\nBody",
        )?;

        let loader = FsManifestLoader::new();
        let outcome = loader.load(&[
            DiscoveryTarget::UserDir(user),
            DiscoveryTarget::ProjectDir(project),
            DiscoveryTarget::CliJson {
                manifest: json!({
                    "id": "cli",
                    "name": "Cli",
                    "description": "cli",
                    "body": "Body"
                }),
                label: Some("flag".into()),
            },
        ])?;
        assert!(outcome.issues.is_empty(), "test fixtures should be valid");
        let ids: Vec<_> = outcome.manifests.into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["project", "cli", "user"]);
        Ok(())
    }

    #[test]
    fn discovers_markdown_cli_and_prioritizes_scopes() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let project_dir = temp.path().join("project");
        let user_dir = temp.path().join("user");
        let plugin_dir = temp.path().join("plugin");
        fs::create_dir_all(&project_dir)?;
        fs::create_dir_all(&user_dir)?;
        fs::create_dir_all(&plugin_dir)?;

        let project_manifest = project_dir.join("release.md");
        let project_contents = r#"---
id: release-captain
name: Release Captain
description: Prepares releases
model: claude-3-sonnet
permissionMode: plan
tools:
  - edit
  - shell
skills:
  - releases
---
You are the release captain.
"#;
        fs::write(&project_manifest, project_contents)?;

        let user_unique = user_dir.join("inspector.yaml");
        let user_unique_yaml = r#"
id: inspector
name: Inspector
description: Checks dependencies
body: |
  Inspect dependencies before shipping.
"#;
        fs::write(&user_unique, user_unique_yaml)?;

        let user_conflict = user_dir.join("release.md");
        fs::write(
            &user_conflict,
            r#"---
id: release-captain
name: Duplicate Release
description: Conflicts with project
---
Duplicate body
"#,
        )?;

        let plugin_id = PluginId::new("deploy-kit");
        let plugin_unique = plugin_dir.join("observer.json");
        let plugin_unique_payload = json!({
            "id": "observer",
            "name": "Observer",
            "description": "Watches deploys",
            "body": "Watch the deploy dashboard."
        });
        fs::write(
            &plugin_unique,
            serde_json::to_vec_pretty(&plugin_unique_payload)?,
        )?;
        let plugin_conflict = plugin_dir.join("adhoc.md");
        fs::write(
            &plugin_conflict,
            r#"---
id: adhoc-helper
name: Plugin Override
description: Should be skipped
---
Plugin duplicate
"#,
        )?;

        let cli_payload = json!({
            "id": "adhoc-helper",
            "name": "Adhoc Helper",
            "description": "Runs adhoc commands",
            "body": "Run adhoc commands."
        });

        let loader = FsManifestLoader::new();
        let outcome = loader.load(&[
            DiscoveryTarget::ProjectDir(project_dir),
            DiscoveryTarget::CliJson {
                manifest: cli_payload.clone(),
                label: Some("flag".into()),
            },
            DiscoveryTarget::UserDir(user_dir),
            DiscoveryTarget::PluginDir {
                path: plugin_dir,
                plugin: plugin_id.clone(),
            },
        ])?;

        let ids: Vec<_> = outcome.manifests.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["release-captain", "adhoc-helper", "inspector", "observer"]
        );

        let project_digest = compute_digest(project_contents.as_bytes());
        let cli_digest = compute_digest(&serde_json::to_vec(&cli_payload)?);
        let plugin_digest =
            compute_digest(&fs::read(&plugin_unique).expect("plugin manifest readable"));

        let project_manifest_loaded = &outcome.manifests[0];
        assert_eq!(project_manifest_loaded.body, "You are the release captain.");
        assert_eq!(
            project_manifest_loaded.tool_scope,
            ToolScope::restricted(vec![ToolName::from("edit"), ToolName::from("shell")])
        );
        assert_eq!(
            project_manifest_loaded.permission_mode,
            PermissionMode::Plan
        );
        assert_eq!(
            project_manifest_loaded.skills,
            vec![String::from("releases")]
        );
        assert_eq!(
            project_manifest_loaded.source,
            Some(DiscoveryScope::Project {
                path: project_manifest.clone(),
            })
        );
        assert_eq!(
            project_manifest_loaded.digest.as_deref(),
            Some(project_digest.as_str())
        );

        let cli_manifest_loaded = &outcome.manifests[1];
        assert_eq!(
            cli_manifest_loaded.source,
            Some(DiscoveryScope::CliJson {
                label: Some("flag".into())
            })
        );
        assert_eq!(
            cli_manifest_loaded.digest.as_deref(),
            Some(cli_digest.as_str())
        );

        let user_manifest_loaded = &outcome.manifests[2];
        assert_eq!(
            user_manifest_loaded.source,
            Some(DiscoveryScope::User { path: user_unique })
        );

        let plugin_manifest_loaded = &outcome.manifests[3];
        assert_eq!(
            plugin_manifest_loaded.source,
            Some(DiscoveryScope::Plugin {
                path: plugin_unique,
                plugin_id: plugin_id.clone(),
            })
        );
        assert_eq!(
            plugin_manifest_loaded.digest.as_deref(),
            Some(plugin_digest.as_str())
        );

        assert_eq!(outcome.issues.len(), 2);
        let expected_user_scope = DiscoveryScope::User {
            path: user_conflict.clone(),
        };
        let expected_plugin_scope = DiscoveryScope::Plugin {
            path: plugin_conflict.clone(),
            plugin_id: plugin_id.clone(),
        };
        let expected_user_message = format!(
            "agent id release-captain skipped: user ({}) conflicts with project ({})",
            user_conflict.display(),
            project_manifest.display()
        );
        let expected_plugin_message = format!(
            "agent id adhoc-helper skipped: plugin {} ({}) conflicts with cli (flag)",
            plugin_id.as_str(),
            plugin_conflict.display()
        );
        assert!(outcome.issues.contains(&LoaderIssue {
            path: Some(user_conflict),
            scope: Some(expected_user_scope),
            message: expected_user_message,
        }));
        assert!(outcome.issues.contains(&LoaderIssue {
            path: Some(plugin_conflict),
            scope: Some(expected_plugin_scope),
            message: expected_plugin_message,
        }));

        Ok(())
    }

    #[test]
    fn cli_manifest_file_loads_latest_contents() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let cli_path = temp.path().join("cli.json");
        fs::write(
            &cli_path,
            serde_json::to_vec(&json!({
                "id": "cli-file",
                "name": "CLI File",
                "description": "initial",
                "body": "initial"
            }))?,
        )?;

        let target = DiscoveryTarget::CliManifestFile {
            path: cli_path.clone(),
            label: Some("cli.json".into()),
        };
        let loader = FsManifestLoader::new();
        let outcome = loader.load(std::slice::from_ref(&target))?;
        assert_eq!(outcome.manifests.len(), 1);
        assert_eq!(outcome.manifests[0].description, "initial");

        fs::write(
            &cli_path,
            serde_json::to_vec(&json!({
                "id": "cli-file",
                "name": "CLI File",
                "description": "updated",
                "body": "updated"
            }))?,
        )?;
        let refreshed = loader.load(std::slice::from_ref(&target))?;
        assert_eq!(refreshed.manifests.len(), 1);
        assert_eq!(refreshed.manifests[0].description, "updated");
        assert_eq!(
            refreshed.manifests[0].source,
            Some(DiscoveryScope::CliJson {
                label: Some("cli.json".into())
            })
        );
        Ok(())
    }

    #[test]
    fn validation_errors_surface_with_scope_details() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let invalid = temp.path().join("broken.yaml");
        let invalid_yaml = r#"
id: broken
name: ""
description: Needs a name
body: |
  Prompt body
"#;
        fs::write(&invalid, invalid_yaml)?;

        let loader = FsManifestLoader::new();
        let outcome = loader.load(&[DiscoveryTarget::ProjectDir(invalid.clone())])?;
        assert!(outcome.manifests.is_empty());
        assert_eq!(outcome.issues.len(), 1);
        let issue = &outcome.issues[0];
        assert_eq!(issue.path, Some(invalid.clone()));
        assert_eq!(issue.scope, Some(DiscoveryScope::Project { path: invalid }));
        assert_eq!(issue.message, "missing required field: name");
        Ok(())
    }
}

#[cfg(all(test, feature = "loader"))]
mod watch_tests {
    use super::*;
    use serde_json::json;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;
    use tempfile::tempdir;

    #[test]
    fn project_watch_emits_events_for_manifest_changes() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let project_dir = temp.path().join("project");
        fs::create_dir_all(&project_dir)?;
        let loader = FsManifestLoader::new();
        let watch = loader.watch(&[DiscoveryTarget::ProjectDir(project_dir.clone())])?;

        let manifest = project_dir.join("alpha.md");
        fs::write(
            &manifest,
            b"---\nid: a\nname: A\ndescription: test\n---\nbody",
        )?;

        let event =
            recv_with_timeout(&watch, Duration::from_secs(5)).expect("project change event");
        assert_eq!(event.scope, DiscoveryTarget::ProjectDir(project_dir));
        Ok(())
    }

    #[test]
    fn user_watch_emits_events_for_manifest_changes() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let user_dir = temp.path().join("user");
        fs::create_dir_all(&user_dir)?;
        let loader = FsManifestLoader::new();
        let watch = loader.watch(&[DiscoveryTarget::UserDir(user_dir.clone())])?;

        let manifest = user_dir.join("beta.yaml");
        fs::write(
            &manifest,
            b"id: beta\nname: B\ndescription: test\nbody: body",
        )?;

        let event = recv_with_timeout(&watch, Duration::from_secs(5)).expect("user change event");
        assert_eq!(event.scope, DiscoveryTarget::UserDir(user_dir));
        Ok(())
    }

    #[test]
    fn plugin_watch_emits_events_for_nested_changes() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let plugin_dir = temp.path().join("plugin");
        fs::create_dir_all(&plugin_dir)?;
        let nested = plugin_dir.join("nested");
        fs::create_dir_all(&nested)?;
        let plugin_id = PluginId::new("demo");
        let loader = FsManifestLoader::new();
        let watch = loader.watch(&[DiscoveryTarget::PluginDir {
            path: plugin_dir.clone(),
            plugin: plugin_id.clone(),
        }])?;

        let manifest = nested.join("gamma.json");
        fs::write(
            &manifest,
            serde_json::to_vec(&json!({
                "id": "gamma",
                "name": "Gamma",
                "description": "test",
                "body": "body"
            }))?,
        )?;

        let event = recv_with_timeout(&watch, Duration::from_secs(5)).expect("plugin change event");
        assert_eq!(
            event.scope,
            DiscoveryTarget::PluginDir {
                path: plugin_dir,
                plugin: plugin_id
            }
        );
        Ok(())
    }

    #[test]
    fn cli_manifest_file_watch_detects_edits_and_recreation(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let manifest_path = temp.path().join("cli.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec(&json!({
                "id": "cli-inline",
                "name": "CLI Inline",
                "description": "initial",
                "body": "initial"
            }))?,
        )?;

        let loader = FsManifestLoader::new();
        let target = DiscoveryTarget::CliManifestFile {
            path: manifest_path.clone(),
            label: Some("cli.json".into()),
        };
        let watch = loader.watch(std::slice::from_ref(&target))?;

        fs::write(
            &manifest_path,
            serde_json::to_vec(&json!({
                "id": "cli-inline",
                "name": "CLI Inline",
                "description": "updated",
                "body": "updated"
            }))?,
        )?;
        let first = recv_with_timeout(&watch, Duration::from_secs(5)).expect("edit event");
        assert_eq!(first.scope, target);

        fs::remove_file(&manifest_path)?;
        thread::sleep(Duration::from_millis(300));
        fs::write(
            &manifest_path,
            serde_json::to_vec(&json!({
                "id": "cli-inline",
                "name": "CLI Inline",
                "description": "recreated",
                "body": "recreated"
            }))?,
        )?;
        let second = recv_with_timeout(&watch, Duration::from_secs(5)).expect("recreate event");
        assert_eq!(second.scope, target);

        Ok(())
    }

    #[test]
    fn cli_targets_emit_static_events() -> Result<(), Box<dyn std::error::Error>> {
        let loader = FsManifestLoader::new();
        let target = DiscoveryTarget::CliJson {
            manifest: json!({
                "id": "cli",
                "name": "Cli",
                "description": "cli",
                "body": "body"
            }),
            label: Some("flag".into()),
        };
        let watch = loader.watch(std::slice::from_ref(&target))?;
        let event = recv_with_timeout(&watch, Duration::from_secs(2)).expect("cli event");
        assert_eq!(event.scope, target);

        assert!(
            recv_with_timeout(&watch, Duration::from_millis(200)).is_none(),
            "cli targets should not emit additional events"
        );
        Ok(())
    }

    fn recv_with_timeout(watch: &LoaderWatch, timeout: Duration) -> Option<LoaderEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            match watch.try_recv() {
                Ok(event) => return Some(event),
                Err(LoaderWatchTryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(LoaderWatchTryRecvError::Closed) => return None,
            }
        }
    }
}
