//! Atomic private file persistence for the recovery ledger.
use super::super::Failure;
use super::model::{Ledger, RevocationTarget, random_hex};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;

pub(super) const LEDGER_LIMIT: u64 = 1024 * 1024;

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing state directory")?;
    let tmp = parent.join(format!(".autorecovery-{}", random_hex()?));
    let result = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .mode(0o600)
            .create_new(true)
            .write(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub(super) struct LedgerStore {
    file: PathBuf,
    state: Mutex<Ledger>,
    io: Arc<Semaphore>,
}

impl LedgerStore {
    pub(super) fn open(space: &Path) -> Result<Self> {
        let file = space.join("autorecovery-v1.json");
        let state = match File::open(&file) {
            Ok(f) => {
                let meta = f.metadata()?;
                ensure!(
                    meta.is_file()
                        && meta.len() <= LEDGER_LIMIT
                        && meta.permissions().mode() & 0o077 == 0,
                    "invalid recovery state file"
                );
                serde_json::from_reader(f.take(LEDGER_LIMIT + 1))
                    .context("invalid recovery state")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self::from_state(file, state))
    }

    pub(super) fn from_state(file: PathBuf, state: Ledger) -> Self {
        Self {
            file,
            state: Mutex::new(state),
            io: Arc::new(Semaphore::new(4)),
        }
    }

    fn persist(&self, state: &Ledger) -> Result<()> {
        let body = serde_json::to_vec(state)?;
        ensure!(
            body.len() as u64 <= LEDGER_LIMIT,
            "recovery state limit exceeded"
        );
        atomic_write(&self.file, &body)
    }

    pub(super) async fn access<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&mut Ledger) -> std::result::Result<(T, bool), Failure> + Send + 'static,
    ) -> std::result::Result<T, Failure> {
        let permit = self
            .io
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::Busy)?;
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut current = this.state.lock().map_err(|_| Failure::Internal)?;
            let mut next = current.clone();
            let (answer, changed) = f(&mut next)?;
            if changed {
                this.persist(&next).map_err(|_| Failure::Internal)?;
                *current = next;
            }
            Ok(answer)
        })
        .await
        .map_err(|_| Failure::Internal)?
    }

    /// Remove a node synchronously (used inside `bdn-collab`'s blocking `Operation::Revoke`).
    pub(super) fn remove_sync(&self, actor: &str) -> Result<Option<RevocationTarget>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("recovery ledger lock unavailable"))?;
        let Some(existing) = state.nodes.get(actor) else {
            return Ok(None);
        };
        let target = RevocationTarget {
            static_client: Some(existing.static_client.clone()),
            binding: existing.binding.clone(),
        };
        let mut next = state.clone();
        next.nodes.remove(actor);
        self.persist(&next)?;
        *state = next;
        Ok(Some(target))
    }

    pub(super) async fn remove(
        self: &Arc<Self>,
        actor: String,
    ) -> std::result::Result<Option<RevocationTarget>, Failure> {
        self.access(move |s| {
            let Some(removed) = s.nodes.remove(&actor) else {
                return Ok((None, false));
            };
            Ok((
                Some(RevocationTarget {
                    static_client: Some(removed.static_client),
                    binding: removed.binding,
                }),
                true,
            ))
        })
        .await
    }
}
