//! Small single-writer project store. The adapter must authenticate node IDs
//! before calling this module. No browser credentials or network I/O here.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const MAX_MEMBERS: usize = 64;
const MAX_TASKS: usize = 256;
pub const DEFAULT_TASK_PAGE: usize = 20;
const MAX_TASK_PAGE: usize = 64;
const MAX_ARTIFACTS: usize = 512;
const MAX_MESSAGES: usize = 256;
const MAX_MESSAGE_PAGE: usize = 64;
const MAX_BLOB: u64 = 16 * 1024 * 1024;
const MAX_PROJECT_BLOBS: u64 = 256 * 1024 * 1024;
const STATE_LIMIT: u64 = 2 * 1024 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Status {
    Pending,
    Claimed {
        by: String,
        lease: u64,
        until: u64,
    },
    Complete {
        by: String,
        lease: u64,
        result: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact: Option<TaskArtifact>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskArtifact {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub detail: String,
    pub assigned_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub status: Status,
}

#[derive(Serialize)]
pub struct TaskEntry<'a> {
    pub id: &'a str,
    pub detail: &'a str,
    pub assigned_to: Option<&'a str>,
    pub created_by: Option<&'a str>,
    pub status: &'a Status,
}

#[derive(Serialize)]
pub struct TaskPage<'a> {
    pub tasks: Vec<TaskEntry<'a>>,
    pub next_after: Option<&'a str>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub sha256: String,
    pub bytes: u64,
    pub publisher: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMessage {
    pub seq: u64,
    pub sender: String,
    pub request_id: String,
    pub body: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u8,
    owner: String,
    members: BTreeSet<String>,
    tasks: BTreeMap<String, Task>,
    artifacts: BTreeMap<String, Artifact>,
    next_lease: u64,
    #[serde(default)]
    messages: Vec<ProjectMessage>,
}

// Own a project directory from one process; not a distributed or multi-writer store.
pub struct ProjectStore {
    dir: PathBuf,
    state: State,
    _writer: File,
}

/// A single bounded temporary upload. Dropping it discards incomplete bytes.
pub struct ArtifactStage {
    path: PathBuf,
    file: Option<File>,
    declared: u64,
    written: u64,
    complete: bool,
    hash: Sha256,
}

impl Drop for ArtifactStage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl ArtifactStage {
    pub async fn ingest(&mut self, mut input: impl AsyncRead + Unpin) -> Result<()> {
        let mut file =
            tokio::fs::File::from_std(self.file.take().context("upload already started")?);
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = input.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            self.written += n as u64;
            ensure!(
                self.written <= self.declared,
                "artifact exceeds declared size"
            );
            file.write_all(&buf[..n]).await?;
            self.hash.update(&buf[..n]);
        }
        ensure!(
            self.written == self.declared,
            "artifact shorter than declared"
        );
        file.sync_all().await?;
        self.file = Some(file.into_std().await);
        self.complete = true;
        Ok(())
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

impl ProjectStore {
    /// `dir` and `owner` come from operator configuration, never from an HTTP request.
    pub fn open(dir: impl AsRef<Path>, owner: &str) -> Result<Self> {
        ensure!(valid_id(owner), "invalid owner ID");
        let dir = dir.as_ref().to_owned();
        let mut create = fs::DirBuilder::new();
        create.recursive(true);
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
            create.mode(0o700);
            options.mode(0o600);
        }
        create.create(&dir)?;
        let writer = options.open(dir.join(".project.lock"))?;
        writer
            .try_lock()
            .context("project is already open for writing")?;
        let state_path = dir.join("project.json");
        let state = match File::open(&state_path) {
            Ok(file) => {
                ensure!(
                    file.metadata()?.len() <= STATE_LIMIT,
                    "project state exceeds its limit"
                );
                let state: State = serde_json::from_reader(file.take(STATE_LIMIT + 1))
                    .context("parse project state")?;
                ensure!(
                    state.version == 1 && state.owner == owner && state.members.contains(owner),
                    "project version or ownership mismatch"
                );
                ensure!(
                    state.members.len() <= MAX_MEMBERS
                        && state.tasks.len() <= MAX_TASKS
                        && state.artifacts.len() <= MAX_ARTIFACTS
                        && state.messages.len() <= MAX_MESSAGES,
                    "project state exceeds its limits"
                );
                state
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State {
                version: 1,
                owner: owner.to_owned(),
                members: [owner.to_owned()].into(),
                tasks: BTreeMap::new(),
                artifacts: BTreeMap::new(),
                next_lease: 1,
                messages: Vec::new(),
            },
            Err(e) => return Err(e.into()),
        };
        let mut store = Self {
            dir,
            state,
            _writer: writer,
        };
        if !state_path.exists() {
            store.update(|_| Ok(()))?;
        }
        store.recover_uploads()?;
        Ok(store)
    }

    fn recover_uploads(&self) -> Result<()> {
        // The process-wide project lock is held: no active uploads exist.
        let blob_dir = self.dir.join("blobs");
        let entries = match fs::read_dir(&blob_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let referenced: BTreeSet<_> = self
            .state
            .artifacts
            .values()
            .map(|a| a.sha256.as_str())
            .collect();
        let mut changed = false;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let stale = name.strip_prefix(".upload.").is_some_and(|tail| {
                tail.split_once('.').is_some_and(|(id, counter)| {
                    id.parse::<u32>().is_ok() && counter.parse::<u64>().is_ok()
                })
            });
            let blob_name = name.len() == 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if stale || (blob_name && !referenced.contains(name)) {
                fs::remove_file(entry.path())?;
                changed = true;
            }
        }
        if changed {
            File::open(blob_dir)?.sync_all()?;
        }
        Ok(())
    }

    fn member(&self, actor: &str) -> Result<()> {
        ensure!(self.state.members.contains(actor), "project access denied");
        Ok(())
    }

    pub fn check_access(&self, actor: &str) -> Result<()> {
        self.member(actor)
    }

    fn update<T>(&mut self, apply: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut next = self.state.clone();
        let result = apply(&mut next)?;
        let bytes = serde_json::to_vec(&next)?;
        ensure!(
            bytes.len() as u64 <= STATE_LIMIT,
            "project state exceeds its limit"
        );
        let temp = self.dir.join(format!(
            ".project.{}.{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut flags = OpenOptions::new();
        flags.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            flags.mode(0o600);
        }
        let mut file = flags.open(&temp)?;
        let write = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp, self.dir.join("project.json"))?;
            Ok(())
        })();
        if let Err(e) = write {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
        self.state = next;
        File::open(&self.dir)?.sync_all()?;
        Ok(result)
    }

    pub fn grant(&mut self, actor: &str, node: &str) -> Result<()> {
        ensure!(
            actor == self.state.owner && valid_id(node),
            "membership change denied"
        );
        if self.state.members.contains(node) {
            return Ok(());
        }
        self.update(|s| {
            ensure!(s.members.len() < MAX_MEMBERS, "member limit reached");
            s.members.insert(node.to_owned());
            Ok(())
        })
    }

    pub fn revoke(&mut self, actor: &str, node: &str) -> Result<()> {
        ensure!(
            actor == self.state.owner && node != actor,
            "membership change denied"
        );
        if !self.state.members.contains(node) {
            return Ok(());
        }
        self.update(|s| {
            s.members.remove(node);
            for task in s.tasks.values_mut() {
                if task.assigned_to.as_deref() == Some(node) {
                    task.assigned_to = None;
                }
                if matches!(&task.status, Status::Claimed { by, .. } if by == node) {
                    task.status = Status::Pending;
                }
            }
            Ok(())
        })
    }

    pub fn create_task(
        &mut self,
        actor: &str,
        id: &str,
        detail: &str,
        assigned: Option<&str>,
    ) -> Result<()> {
        self.member(actor)?;
        ensure!(
            valid_id(id) && !detail.is_empty() && detail.len() <= 2048,
            "invalid task"
        );
        if let Some(node) = assigned {
            self.member(node)?;
        }
        self.update(|s| {
            ensure!(s.tasks.len() < MAX_TASKS, "task limit reached");
            ensure!(!s.tasks.contains_key(id), "task exists");
            s.tasks.insert(
                id.to_owned(),
                Task {
                    detail: detail.to_owned(),
                    assigned_to: assigned.map(str::to_owned),
                    created_by: Some(actor.to_owned()),
                    status: Status::Pending,
                },
            );
            Ok(())
        })
    }

    fn task_visible_to(&self, actor: &str, task: &Task) -> bool {
        actor == self.state.owner
            || task.created_by.as_deref() == Some(actor)
            || task.assigned_to.as_deref().is_none_or(|node| node == actor)
    }

    pub fn task(&self, actor: &str, id: &str) -> Result<&Task> {
        self.member(actor)?;
        self.state
            .tasks
            .get(id)
            .filter(|task| self.task_visible_to(actor, task))
            .context("task not found")
    }

    /// IDs are sorted lexicographically; the cursor is exclusive and is never taken
    /// from a hidden task. Pages reflect current access, not a snapshot.
    pub fn tasks(&self, actor: &str, after: Option<&str>, limit: usize) -> Result<TaskPage<'_>> {
        self.member(actor)?;
        ensure!(after.is_none_or(valid_id), "invalid task cursor");
        ensure!((1..=MAX_TASK_PAGE).contains(&limit), "invalid task limit");
        let mut visible = self.state.tasks.iter().filter(|(id, task)| {
            after.is_none_or(|cursor| id.as_str() > cursor) && self.task_visible_to(actor, task)
        });
        let tasks = visible
            .by_ref()
            .take(limit)
            .map(|(id, task)| TaskEntry {
                id,
                detail: &task.detail,
                assigned_to: task.assigned_to.as_deref(),
                created_by: task.created_by.as_deref(),
                status: &task.status,
            })
            .collect::<Vec<_>>();
        let next_after = if visible.next().is_some() {
            tasks.last().map(|entry| entry.id)
        } else {
            None
        };
        Ok(TaskPage { tasks, next_after })
    }

    /// Clock and expiry come from the service, not the claimant.
    pub fn claim(&mut self, actor: &str, id: &str, now: u64, duration: u64) -> Result<u64> {
        let task = self.task(actor, id)?;
        ensure!(
            task.assigned_to.as_deref().is_none_or(|node| node == actor),
            "task assigned to another member"
        );
        ensure!((1..=3600).contains(&duration), "invalid lease duration");
        let until = now.checked_add(duration).context("invalid clock")?;
        ensure!(
            matches!(task.status, Status::Pending)
                || matches!(task.status, Status::Claimed { until, .. } if until <= now),
            "task cannot be claimed"
        );
        self.update(|s| {
            let lease = s.next_lease;
            s.next_lease = lease.checked_add(1).context("lease counter exhausted")?;
            s.tasks.get_mut(id).context("task not found")?.status = Status::Claimed {
                by: actor.to_owned(),
                lease,
                until,
            };
            Ok(lease)
        })
    }

    pub fn complete(
        &mut self,
        actor: &str,
        id: &str,
        lease: u64,
        now: u64,
        result: &str,
    ) -> Result<()> {
        self.complete_with_artifact(actor, id, lease, now, result, None)
    }

    pub fn complete_with_artifact(
        &mut self,
        actor: &str,
        id: &str,
        lease: u64,
        now: u64,
        result: &str,
        artifact_name: Option<&str>,
    ) -> Result<()> {
        let task = self.task(actor, id)?;
        ensure!(!result.is_empty() && result.len() <= 2048, "invalid result");
        if matches!(&task.status, Status::Complete { by, lease: l, result: saved, artifact }
            if by == actor && *l == lease && saved == result
                && artifact.as_ref().map(|a| a.name.as_str()) == artifact_name)
        {
            return Ok(());
        }
        ensure!(
            matches!(&task.status, Status::Claimed { by, lease: l, until }
            if by == actor && *l == lease && *until > now),
            "stale or unauthorized claim"
        );
        let artifact = if let Some(name) = artifact_name {
            ensure!(valid_id(name), "invalid result artifact name");
            let item = self.artifact(actor, name)?;
            // Publication hashes the upload. Confirm its committed file is still
            // present and the size matches before persisting the pinned metadata.
            self.open_artifact(actor, name)?;
            Some(TaskArtifact {
                name: name.to_owned(),
                bytes: item.bytes,
                sha256: item.sha256.clone(),
            })
        } else {
            None
        };
        self.update(|s| {
            s.tasks.get_mut(id).context("task not found")?.status = Status::Complete {
                by: actor.to_owned(),
                lease,
                result: result.to_owned(),
                artifact,
            };
            Ok(())
        })
    }

    /// Register metadata only after the storage adapter has verified the bytes and digest.
    fn publish_verified(
        &mut self,
        actor: &str,
        name: &str,
        digest: &str,
        bytes: u64,
    ) -> Result<()> {
        self.member(actor)?;
        ensure!(
            valid_id(name)
                && digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid artifact"
        );
        let entry = Artifact {
            sha256: digest.to_owned(),
            bytes,
            publisher: actor.to_owned(),
        };
        if let Some(existing) = self.state.artifacts.get(name) {
            ensure!(*existing == entry, "artifact name already published");
            return Ok(());
        }
        self.update(|s| {
            ensure!(s.artifacts.len() < MAX_ARTIFACTS, "artifact limit reached");
            s.artifacts.insert(name.to_owned(), entry);
            Ok(())
        })
    }

    pub fn artifact(&self, actor: &str, name: &str) -> Result<&Artifact> {
        self.member(actor)?;
        self.state.artifacts.get(name).context("artifact not found")
    }

    fn check_artifact_slot(&self, actor: &str, name: &str, length: u64) -> Result<()> {
        self.member(actor)?;
        ensure!(
            valid_id(name) && length <= MAX_BLOB,
            "invalid artifact name or size"
        );
        if let Some(item) = self.state.artifacts.get(name) {
            ensure!(
                item.publisher == actor && item.bytes == length,
                "artifact name already published"
            );
        } else {
            ensure!(
                self.state.artifacts.len() < MAX_ARTIFACTS,
                "artifact limit reached"
            );
            let used = self
                .state
                .artifacts
                .values()
                .fold(length, |total, item| total.saturating_add(item.bytes));
            ensure!(used <= MAX_PROJECT_BLOBS, "project storage quota reached");
        }
        Ok(())
    }

    pub fn stage_artifact(&self, actor: &str, name: &str, length: u64) -> Result<ArtifactStage> {
        self.check_artifact_slot(actor, name, length)?;
        let dir = self.dir.join("blobs");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!(
            ".upload.{}.{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut flags = OpenOptions::new();
        flags.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            flags.mode(0o600);
        }
        let file = flags.open(&path)?;
        Ok(ArtifactStage {
            path,
            file: Some(file),
            declared: length,
            written: 0,
            complete: false,
            hash: Sha256::new(),
        })
    }

    pub fn finish_artifact(
        &mut self,
        actor: &str,
        name: &str,
        staged: ArtifactStage,
    ) -> Result<Artifact> {
        let bytes = staged.declared;
        self.check_artifact_slot(actor, name, bytes)?;
        let dir = self.dir.join("blobs");
        ensure!(
            staged.path.parent() == Some(dir.as_path()),
            "wrong project for artifact"
        );
        ensure!(
            staged.complete && staged.written == bytes,
            "incomplete artifact"
        );
        let digest = hex::encode(staged.hash.clone().finalize());
        if let Some(item) = self.state.artifacts.get(name) {
            ensure!(item.sha256 == digest, "artifact name already published");
            return Ok(item.clone());
        }
        let path = dir.join(&digest);
        let linked = match fs::hard_link(&staged.path, &path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    path.metadata()?.len() == bytes,
                    "stored artifact is inconsistent"
                );
                false
            }
            Err(e) => return Err(e.into()),
        };
        let recorded = File::open(&dir)
            .and_then(|d| d.sync_all())
            .map_err(anyhow::Error::from)
            .and_then(|_| self.publish_verified(actor, name, &digest, bytes));
        if let Err(e) = recorded {
            if linked && !self.state.artifacts.values().any(|a| a.sha256 == digest) {
                let _ = fs::remove_file(path);
            }
            return Err(e);
        }
        Ok(self.artifact(actor, name)?.clone())
    }

    #[cfg(test)]
    fn save_artifact(
        &mut self,
        actor: &str,
        name: &str,
        length: u64,
        input: impl Read,
    ) -> Result<Artifact> {
        let mut staged = self.stage_artifact(actor, name, length)?;
        let mut input = input.take(length + 1);
        let mut buffer = [0; 65536];
        let file = staged.file.as_mut().context("missing stage file")?;
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            staged.written += n as u64;
            ensure!(staged.written <= length, "artifact exceeds declared size");
            staged.hash.update(&buffer[..n]);
            file.write_all(&buffer[..n])?;
        }
        file.sync_all()?;
        staged.complete = true;
        self.finish_artifact(actor, name, staged)
    }

    pub fn open_artifact(&self, actor: &str, name: &str) -> Result<File> {
        let item = self.artifact(actor, name)?;
        ensure!(
            item.sha256.len() == 64 && item.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid stored artifact ID"
        );
        let file = File::open(self.dir.join("blobs").join(&item.sha256))?;
        ensure!(
            file.metadata()?.len() == item.bytes,
            "stored artifact is inconsistent"
        );
        Ok(file)
    }

    /// Publish plain text to this project; retries reuse the sender's request ID.
    /// Receiving or reading a message never grants an execution or service right.
    pub fn post_message(&mut self, actor: &str, request_id: &str, body: &str) -> Result<u64> {
        self.member(actor)?;
        ensure!(
            valid_id(request_id) && !body.trim().is_empty() && body.len() <= 2048,
            "invalid project message"
        );
        if let Some(prior) = self
            .state
            .messages
            .iter()
            .find(|m| m.sender == actor && m.request_id == request_id)
        {
            ensure!(prior.body == body, "message request ID already used");
            return Ok(prior.seq);
        }
        self.update(|s| {
            ensure!(s.messages.len() < MAX_MESSAGES, "project mailbox full");
            let seq = s
                .messages
                .last()
                .map_or(0, |m| m.seq)
                .checked_add(1)
                .context("message sequence exhausted")?;
            s.messages.push(ProjectMessage {
                seq,
                sender: actor.to_owned(),
                request_id: request_id.to_owned(),
                body: body.to_owned(),
            });
            Ok(seq)
        })
    }

    /// Read without consuming; offline nodes keep the last successfully handled cursor.
    pub fn messages_after(
        &self,
        actor: &str,
        after: u64,
        limit: usize,
    ) -> Result<&[ProjectMessage]> {
        self.member(actor)?;
        ensure!(
            (1..=MAX_MESSAGE_PAGE).contains(&limit),
            "invalid message page size"
        );
        let messages = &self.state.messages;
        let start = messages.partition_point(|m| m.seq <= after);
        Ok(&messages[start..messages.len().min(start + limit)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let root = std::env::temp_dir();
            loop {
                let p = root.join(format!(
                    "bdn-collab-{}-{}",
                    std::process::id(),
                    TEMP_ID.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&p) {
                    Ok(()) => return Self(p),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("{e}"),
                }
            }
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn project_boundaries_and_artifact_metadata_survive_a_restart() -> Result<()> {
        let dir = TestDir::new();
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        assert!(ProjectStore::open(&dir.0, "owner").is_err());
        assert!(p.grant("outsider", "agent-b").is_err());
        p.grant("owner", "agent-a")?;
        p.grant("owner", "agent-b")?;
        p.create_task(
            "agent-a",
            "build",
            "Build the checked revision",
            Some("agent-b"),
        )?;
        assert!(p.claim("agent-a", "build", 20, 30).is_err());
        let lease = p.claim("agent-b", "build", 20, 30)?;
        let hash = "ab".repeat(32);
        p.publish_verified("agent-b", "binary-v1", &hash, 1024)?;
        assert!(
            p.publish_verified("agent-a", "binary-v1", &hash, 1024)
                .is_err()
        );
        assert!(p.artifact("outsider", "binary-v1").is_err());
        assert!(p.task("outsider", "build").is_err());
        drop(p);
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        assert_eq!(p.artifact("agent-a", "binary-v1")?.sha256, hash);
        p.complete("agent-b", "build", lease, 30, "binary-v1")?;
        p.complete("agent-b", "build", lease, 999, "binary-v1")?;
        assert!(
            matches!(&p.task("agent-a", "build")?.status, Status::Complete { result, .. } if result == "binary-v1")
        );
        Ok(())
    }

    #[test]
    fn task_pages_hide_unrelated_assignments_and_resume_after_visible_ids() -> Result<()> {
        let dir = TestDir::new();
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        for node in ["a", "b", "c"] {
            p.grant("owner", node)?;
        }
        p.create_task("a", "01-open", "Visible to project members", None)?;
        p.create_task("a", "02-hidden", "Private assignment", Some("c"))?;
        p.create_task("a", "03-targeted", "Assigned work", Some("b"))?;
        p.create_task("b", "04-created", "B's own delegation", Some("c"))?;
        p.create_task("a", "05-hidden", "Another private assignment", Some("c"))?;

        let first = p.tasks("b", None, 1)?;
        assert_eq!(first.tasks[0].id, "01-open");
        assert_eq!(first.next_after, Some("01-open"));
        let rest = p.tasks("b", first.next_after, 2)?;
        assert_eq!(
            rest.tasks.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            ["03-targeted", "04-created"]
        );
        assert_eq!(rest.tasks[0].created_by, Some("a"));
        assert_eq!(rest.tasks[0].assigned_to, Some("b"));
        assert_eq!(rest.next_after, None);
        assert_eq!(p.tasks("owner", None, 64)?.tasks.len(), 5);
        assert_eq!(p.tasks("a", None, 64)?.tasks.len(), 4);
        assert!(p.tasks("b", Some("04-created"), 64)?.tasks.is_empty());
        assert!(p.tasks("b", Some("not/a/cursor"), 1).is_err());
        assert!(p.tasks("b", None, 0).is_err());
        assert!(p.tasks("b", None, 65).is_err());
        assert!(p.tasks("outsider", None, 1).is_err());
        assert!(p.task("b", "02-hidden").is_err());
        assert!(p.claim("b", "02-hidden", 1, 60).is_err());
        assert!(p.claim("b", "04-created", 1, 60).is_err());
        p.claim("b", "03-targeted", 1, 60)?;
        drop(p);

        let mut p = ProjectStore::open(&dir.0, "owner")?;
        assert_eq!(p.task("a", "03-targeted")?.created_by.as_deref(), Some("a"));
        assert!(p.task("c", "03-targeted").is_err());
        p.revoke("owner", "b")?;
        assert!(p.tasks("b", None, 20).is_err());
        assert!(matches!(
            p.task("a", "03-targeted")?.status,
            Status::Pending
        ));
        Ok(())
    }

    #[test]
    fn tasks_without_legacy_creator_metadata_remain_readable() -> Result<()> {
        let dir = TestDir::new();
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        p.grant("owner", "b")?;
        p.grant("owner", "other")?;
        p.create_task("owner", "legacy", "Read old persisted states", Some("b"))?;
        p.update(|s| {
            s.tasks.get_mut("legacy").unwrap().created_by = None;
            Ok(())
        })?;
        drop(p);
        let p = ProjectStore::open(&dir.0, "owner")?;
        let page = p.tasks("b", None, 20)?;
        let json = serde_json::to_value(page)?;
        assert_eq!(json["tasks"][0]["id"], "legacy");
        assert!(json["tasks"][0]["created_by"].is_null());
        assert!(p.tasks("other", None, 20)?.tasks.is_empty());
        assert!(p.task("other", "legacy").is_err());
        Ok(())
    }

    #[test]
    fn task_completion_pins_a_published_artifact_without_consuming_failed_claim() -> Result<()> {
        let dir = TestDir::new();
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        p.grant("owner", "b")?;
        p.create_task("owner", "build", "Produce a binary", Some("b"))?;
        let lease = p.claim("b", "build", 10, 30)?;
        for name in ["missing", "../invalid"] {
            assert!(
                p.complete_with_artifact("b", "build", lease, 11, "Built", Some(name))
                    .is_err()
            );
        }
        let payload = b"committed build output";
        let item = p.save_artifact("b", "bin-v1", payload.len() as u64, &payload[..])?;
        let inconsistent = p.save_artifact("b", "corrupt", 3, &b"bad"[..])?;
        fs::write(dir.0.join("blobs").join(inconsistent.sha256), b"")?;
        assert!(
            p.complete_with_artifact("b", "build", lease, 12, "Built", Some("corrupt"))
                .is_err()
        );
        assert!(
            matches!(&p.task("b", "build")?.status, Status::Claimed { lease: l, .. } if *l == lease)
        );
        p.complete_with_artifact("b", "build", lease, 13, "Built", Some("bin-v1"))?;
        drop(p);

        let mut p = ProjectStore::open(&dir.0, "owner")?;
        let task = serde_json::to_value(p.task("owner", "build")?)?;
        assert_eq!(
            task["status"]["complete"]["artifact"],
            serde_json::json!({"name": "bin-v1", "bytes": payload.len(), "sha256": item.sha256})
        );
        let mut downloaded = Vec::new();
        p.open_artifact("owner", "bin-v1")?
            .read_to_end(&mut downloaded)?;
        assert_eq!(downloaded, payload);
        p.complete_with_artifact("b", "build", lease, 99, "Built", Some("bin-v1"))?;
        assert!(p.complete("b", "build", lease, 99, "Built").is_err());
        assert!(
            p.complete_with_artifact("b", "build", lease, 99, "Different summary", Some("bin-v1"))
                .is_err()
        );
        assert!(
            p.complete_with_artifact("b", "build", lease, 99, "Built", Some("corrupt"))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn text_only_completed_status_keeps_its_legacy_wire_shape() -> Result<()> {
        let legacy = serde_json::json!({"complete": {"by": "b", "lease": 7, "result": "Text"}});
        let status: Status = serde_json::from_value(legacy.clone())?;
        assert_eq!(serde_json::to_value(status)?, legacy);
        Ok(())
    }

    #[test]
    fn claim_is_exclusive_and_stale_worker_cannot_overwrite_new_result() -> Result<()> {
        let dir = TestDir::new();
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        p.grant("owner", "a")?;
        p.grant("owner", "b")?;
        p.create_task("owner", "work", "Accept one claim", None)?;
        let shared = Arc::new(Mutex::new(p));
        let ready = Barrier::new(2);
        let winners = std::thread::scope(|s| {
            let shared = &shared;
            let ready = &ready;
            let claims = ["a", "b"].map(|node| {
                s.spawn(move || {
                    ready.wait();
                    shared
                        .lock()
                        .unwrap()
                        .claim(node, "work", 100, 5)
                        .ok()
                        .map(|lease| (node, lease))
                })
            });
            claims
                .into_iter()
                .filter_map(|j| j.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(winners.len(), 1);
        let (first, old) = winners[0];
        let next = if first == "a" { "b" } else { "a" };
        let mut p = shared.lock().unwrap();
        let lease = p.claim(next, "work", 105, 5)?;
        assert_ne!(lease, old);
        assert!(p.complete(first, "work", old, 106, "stale").is_err());
        p.complete(next, "work", lease, 106, "new")?;
        Ok(())
    }

    #[test]
    fn removing_a_member_invalidates_its_pending_rights() -> Result<()> {
        let dir = TestDir::new();
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        p.grant("owner", "former")?;
        p.create_task("owner", "test", "Carry out test", Some("former"))?;
        let old = p.claim("former", "test", 10, 30)?;
        p.revoke("owner", "former")?;
        let lease = p.claim("owner", "test", 11, 10)?;
        assert_ne!(old, lease);
        assert!(p.complete("former", "test", old, 11, "untrusted").is_err());
        p.grant("owner", "former")?;
        assert!(
            p.complete("former", "test", old, 11, "stale after readmission")
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn offline_mailbox_replays_in_order_without_duplicates() -> Result<()> {
        let dir = TestDir::new();
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        p.grant("owner", "a")?;
        p.grant("owner", "b")?;
        let first = p.post_message("a", "send-1", "Build ready")?;
        assert_eq!(p.post_message("a", "send-1", "Build ready")?, first);
        assert!(p.post_message("a", "send-1", "changed").is_err());
        assert!(
            p.post_message("outsider", "send-1", "do not admit")
                .is_err()
        );
        let second = p.post_message("b", "send-1", "Acknowledged")?;
        drop(p);
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        assert_eq!(p.messages_after("b", 0, 1)?[0].seq, first);
        assert_eq!(p.messages_after("b", first, 1)?[0].seq, second);
        assert!(p.messages_after("b", second, 1)?.is_empty());
        assert!(p.messages_after("outsider", 0, 1).is_err());
        p.revoke("owner", "b")?;
        assert!(p.messages_after("b", 0, 1).is_err());
        Ok(())
    }

    #[test]
    fn artifact_bytes_verify_after_restart_and_remain_private() -> Result<()> {
        let dir = TestDir::new();
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        p.grant("owner", "a")?;
        p.grant("owner", "b")?;
        let payload = vec![0x43; 256 * 1024 + 3];
        let size = payload.len() as u64;
        assert!(
            p.save_artifact("outsider", "result", size, &payload[..])
                .is_err()
        );
        assert!(
            p.save_artifact("a", "result", size + 1, &payload[..])
                .is_err()
        );
        assert!(
            p.save_artifact("a", "result", size - 1, &payload[..])
                .is_err()
        );
        assert!(
            p.save_artifact("a", "result", MAX_BLOB + 1, &payload[..])
                .is_err()
        );
        let saved = p.save_artifact("a", "result", size, &payload[..])?;
        assert_eq!(saved.sha256, hex::encode(Sha256::digest(&payload)));
        assert_eq!(p.save_artifact("a", "result", size, &payload[..])?, saved);
        assert!(p.save_artifact("b", "result", size, &payload[..]).is_err());
        drop(p);
        let mut p = ProjectStore::open(&dir.0, "owner")?;
        let mut read = Vec::new();
        p.open_artifact("b", "result")?.read_to_end(&mut read)?;
        assert_eq!(read, payload);
        assert!(p.open_artifact("outsider", "result").is_err());
        p.revoke("owner", "b")?;
        assert!(p.open_artifact("b", "result").is_err());
        Ok(())
    }
}
