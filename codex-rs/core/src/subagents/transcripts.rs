use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use serde::Deserialize;
use serde::Serialize;
use time::OffsetDateTime;
use time::macros::format_description;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::warn;

use crate::RolloutRecorder;
use crate::default_client::originator;
use crate::git_info::collect_git_info;
use crate::rollout::policy::is_persisted_response_item;

const SUBAGENT_DIR: &str = "subagents";
const RUNS_DIR: &str = "runs";
const INDEX_FILE: &str = "index.json";
const RESUME_TOKEN_FILE: &str = "resume.token";
const MAX_INDEX_RUNS: usize = 10;

#[derive(Clone)]
pub(crate) struct SubagentTranscriptStore {
    inner: Arc<TranscriptStoreInner>,
}

#[derive(Debug)]
struct TranscriptStoreInner {
    codex_home: PathBuf,
}

impl SubagentTranscriptStore {
    pub fn new(codex_home: PathBuf) -> Self {
        Self {
            inner: Arc::new(TranscriptStoreInner { codex_home }),
        }
    }

    pub async fn start_run(
        &self,
        agent_id: &str,
        session_source: &SessionSource,
        cwd: &Path,
        instructions: Option<&str>,
        model_provider: Option<&str>,
    ) -> std::io::Result<SubagentTranscript> {
        let run_id = ThreadId::new();
        let agent_dir = self.agent_dir(agent_id);
        let run_dir = agent_dir.join(RUNS_DIR).join(run_id.to_string());
        fs::create_dir_all(&run_dir).await?;
        let transcript_path = run_dir.join(format!("agent-{run_id}.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&transcript_path)
            .await?;

        let session_meta = SessionMeta {
            id: run_id,
            timestamp: format_timestamp(OffsetDateTime::now_utc())?,
            cwd: cwd.to_path_buf(),
            originator: originator().value.clone(),
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            instructions: instructions.map(ToOwned::to_owned),
            source: session_source.clone(),
            model_provider: model_provider.map(ToOwned::to_owned),
        };

        let writer = TranscriptWriter::spawn(file, session_meta, cwd.to_path_buf());

        Ok(SubagentTranscript {
            inner: Arc::new(SubagentTranscriptInner {
                agent_id: agent_id.to_string(),
                session_source: session_source.clone(),
                model_provider: model_provider.map(ToOwned::to_owned),
                run_id,
                transcript_path,
                run_dir,
                writer,
                store: Arc::clone(&self.inner),
                finished: Mutex::new(false),
                event_count: Mutex::new(0),
            }),
        })
    }

    pub async fn resume_history(
        &self,
        token: &SubagentResumeToken,
    ) -> std::io::Result<Vec<ResponseItem>> {
        let history = match RolloutRecorder::get_rollout_history(&token.transcript_path).await? {
            crate::InitialHistory::Resumed(resumed) => resumed.history,
            crate::InitialHistory::Forked(items) => items,
            crate::InitialHistory::New => Vec::new(),
        };
        let mut responses = Vec::new();
        for item in history {
            if let RolloutItem::ResponseItem(response) = item {
                responses.push(response);
            }
        }
        Ok(responses)
    }

    pub fn agent_dir(&self, agent_id: &str) -> PathBuf {
        self.inner.codex_home.join(SUBAGENT_DIR).join(agent_id)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SubagentTranscript {
    inner: Arc<SubagentTranscriptInner>,
}

impl SubagentTranscript {
    pub async fn record_items(&self, items: &[RolloutItem]) -> std::io::Result<()> {
        self.inner.writer.record_items(items).await?;
        let mut count = self.inner.event_count.lock().await;
        *count += items.len() as u64;
        Ok(())
    }

    pub async fn finish(&self) -> std::io::Result<Option<String>> {
        let mut finished = self.inner.finished.lock().await;
        if *finished {
            return Ok(None);
        }
        self.inner.writer.shutdown().await?;
        let token = SubagentResumeToken {
            agent_id: self.inner.agent_id.clone(),
            run_id: self.inner.run_id,
            transcript_path: self.inner.transcript_path.clone(),
            generated_at: format_timestamp(OffsetDateTime::now_utc())?,
            event_count: *self.inner.event_count.lock().await,
        };
        let encoded = token.encode()?;
        let summary = TranscriptRunSummary {
            agent_id: self.inner.agent_id.clone(),
            run_id: self.inner.run_id.to_string(),
            transcript_path: self.inner.transcript_path.clone(),
            resume_token: encoded.clone(),
            updated_at: token.generated_at.clone(),
            event_count: token.event_count,
            session_source: self.inner.session_source.clone(),
            provider: self.inner.model_provider.clone(),
        };
        self.inner
            .store
            .write_resume_artifacts(
                &self.inner.agent_id,
                &self.inner.run_dir,
                &encoded,
                &summary,
            )
            .await?;
        *finished = true;
        Ok(Some(encoded))
    }
}

#[derive(Debug)]
struct SubagentTranscriptInner {
    agent_id: String,
    session_source: SessionSource,
    model_provider: Option<String>,
    run_id: ThreadId,
    transcript_path: PathBuf,
    run_dir: PathBuf,
    writer: TranscriptWriter,
    store: Arc<TranscriptStoreInner>,
    finished: Mutex<bool>,
    event_count: Mutex<u64>,
}

impl TranscriptStoreInner {
    async fn write_resume_artifacts(
        &self,
        agent_id: &str,
        run_dir: &Path,
        token: &str,
        summary: &TranscriptRunSummary,
    ) -> std::io::Result<()> {
        let agent_dir = self.codex_home.join(SUBAGENT_DIR).join(agent_id);
        fs::create_dir_all(&agent_dir).await?;
        let token_path = run_dir.join(RESUME_TOKEN_FILE);
        fs::write(&token_path, token).await?;
        let run_index_path = run_dir.join(INDEX_FILE);
        fs::write(&run_index_path, serde_json::to_vec_pretty(summary)?).await?;
        let latest_token_path = agent_dir.join(RESUME_TOKEN_FILE);
        fs::write(latest_token_path, token).await?;
        let index_path = agent_dir.join(INDEX_FILE);
        let mut index = read_index(&index_path).await.unwrap_or_default();
        index.add_run(summary.clone());
        fs::write(&index_path, serde_json::to_vec_pretty(&index)?).await
    }
}

async fn read_index(path: &Path) -> std::io::Result<TranscriptIndex> {
    match fs::read(path).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(TranscriptIndex::default()),
        Err(err) => Err(err),
    }
}

#[derive(Debug)]
struct TranscriptWriter {
    tx: mpsc::Sender<TranscriptCmd>,
}

enum TranscriptCmd {
    Add(Vec<RolloutItem>),
    Shutdown { ack: oneshot::Sender<()> },
}

impl TranscriptWriter {
    fn spawn(file: tokio::fs::File, session_meta: SessionMeta, cwd: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::channel::<TranscriptCmd>(256);
        tokio::spawn(async move {
            let mut writer = TranscriptJsonlWriter { file };
            let git_info = collect_git_info(&cwd).await;
            let meta_line = SessionMetaLine {
                meta: session_meta,
                git: git_info,
            };
            if let Err(err) = writer
                .write_rollout_item(RolloutItem::SessionMeta(meta_line))
                .await
            {
                warn!(error = %err, "failed to write transcript meta line");
            }
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    TranscriptCmd::Add(items) => {
                        for item in items {
                            if is_persisted_response_item(&item)
                                && let Err(err) = writer.write_rollout_item(item).await
                            {
                                warn!(error = %err, "failed to persist subagent transcript item");
                            }
                        }
                    }
                    TranscriptCmd::Shutdown { ack } => {
                        if let Err(err) = writer.flush().await {
                            warn!(error = %err, "failed to flush transcript writer");
                        }
                        let _ = ack.send(());
                        break;
                    }
                }
            }
        });
        Self { tx }
    }

    async fn record_items(&self, items: &[RolloutItem]) -> std::io::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.tx
            .send(TranscriptCmd::Add(items.to_vec()))
            .await
            .map_err(|err| {
                std::io::Error::other(format!("failed to queue transcript writes: {err}"))
            })
    }

    async fn shutdown(&self) -> std::io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(TranscriptCmd::Shutdown { ack: tx })
            .await
            .map_err(|err| {
                std::io::Error::other(format!("failed to queue transcript shutdown: {err}"))
            })?;
        rx.await.map_err(|err| {
            std::io::Error::other(format!("failed to await transcript shutdown: {err}"))
        })
    }
}

struct TranscriptJsonlWriter {
    file: tokio::fs::File,
}

impl TranscriptJsonlWriter {
    async fn write_rollout_item(&mut self, item: RolloutItem) -> std::io::Result<()> {
        let timestamp = format_timestamp(OffsetDateTime::now_utc())?;
        let line = crate::protocol::RolloutLine { timestamp, item };
        self.write_line(&line).await
    }

    async fn write_line(&mut self, item: &impl serde::Serialize) -> std::io::Result<()> {
        let mut json = serde_json::to_string(item)?;
        json.push('\n');
        self.file.write_all(json.as_bytes()).await?;
        Ok(())
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush().await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TranscriptIndex {
    pub runs: Vec<TranscriptRunSummary>,
}

impl TranscriptIndex {
    fn add_run(&mut self, summary: TranscriptRunSummary) {
        self.runs.insert(0, summary);
        if self.runs.len() > MAX_INDEX_RUNS {
            self.runs.truncate(MAX_INDEX_RUNS);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptRunSummary {
    pub agent_id: String,
    pub run_id: String,
    pub transcript_path: PathBuf,
    pub resume_token: String,
    pub updated_at: String,
    pub event_count: u64,
    #[serde(default = "default_session_source")]
    pub session_source: SessionSource,
    #[serde(default)]
    pub provider: Option<String>,
}

fn default_session_source() -> SessionSource {
    SessionSource::Unknown
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResumeToken {
    pub agent_id: String,
    pub run_id: ThreadId,
    pub transcript_path: PathBuf,
    pub generated_at: String,
    pub event_count: u64,
}

impl SubagentResumeToken {
    pub fn encode(&self) -> std::io::Result<String> {
        let json = serde_json::to_vec(self)?;
        Ok(URL_SAFE_NO_PAD.encode(json))
    }

    pub fn decode(token: &str) -> std::io::Result<Self> {
        let bytes = URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|err| std::io::Error::other(format!("invalid base64 token: {err}")))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

fn format_timestamp(dt: OffsetDateTime) -> std::io::Result<String> {
    let fmt =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    dt.format(&fmt)
        .map_err(|err| std::io::Error::other(format!("failed to format timestamp: {err}")))
}
