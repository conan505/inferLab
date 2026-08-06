use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::{
    RaftError,
    model::{PersistentState, TraceEvent},
};

#[derive(Debug)]
pub(crate) struct StableStorage {
    path: PathBuf,
}

impl StableStorage {
    pub(crate) fn new(path: impl AsRef<Path>) -> Result<Self, RaftError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(RaftError::storage)?;
        }
        Ok(Self { path })
    }

    pub(crate) fn load(&self) -> Result<PersistentState, RaftError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistentState::default());
            }
            Err(error) => return Err(RaftError::storage(error)),
        };
        let state = serde_json::from_slice::<PersistentState>(&bytes).map_err(|error| {
            RaftError::Storage(format!(
                "cannot decode persistent state {}: {error}",
                self.path.display()
            ))
        })?;
        validate_persistent_state(&state)?;
        Ok(state)
    }

    pub(crate) fn save(&self, state: &PersistentState) -> Result<(), RaftError> {
        validate_persistent_state(state)?;
        let bytes = serde_json::to_vec(state)
            .map_err(|error| RaftError::Storage(format!("serialize Raft state: {error}")))?;
        let temporary = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(RaftError::storage)?;
        file.write_all(&bytes).map_err(RaftError::storage)?;
        file.sync_all().map_err(RaftError::storage)?;
        fs::rename(&temporary, &self.path).map_err(RaftError::storage)?;
        if let Some(parent) = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(RaftError::storage)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct EventJournal {
    file: Mutex<File>,
}

impl EventJournal {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, RaftError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(RaftError::storage)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(RaftError::storage)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    pub(crate) fn record(&self, event: &TraceEvent) -> Result<(), RaftError> {
        let mut bytes = serde_json::to_vec(event)
            .map_err(|error| RaftError::Storage(format!("serialize trace event: {error}")))?;
        bytes.push(b'\n');
        let mut file = self
            .file
            .lock()
            .map_err(|_| RaftError::Storage("event journal mutex was poisoned".to_owned()))?;
        file.write_all(&bytes).map_err(RaftError::storage)?;
        file.sync_data().map_err(RaftError::storage)
    }
}

fn validate_persistent_state(state: &PersistentState) -> Result<(), RaftError> {
    if !state.cluster_id.is_empty() {
        crate::model::validate_cluster_id(&state.cluster_id).map_err(|error| {
            RaftError::Storage(format!("invalid persisted cluster identity: {error}"))
        })?;
    }
    if state.commit_index > u64::try_from(state.log.len()).unwrap_or(u64::MAX) {
        return Err(RaftError::Storage(format!(
            "commit_index {} exceeds log length {}",
            state.commit_index,
            state.log.len()
        )));
    }
    for (offset, entry) in state.log.iter().enumerate() {
        let expected = u64::try_from(offset).unwrap_or(u64::MAX).saturating_add(1);
        if entry.index != expected || entry.term == 0 {
            return Err(RaftError::Storage(format!(
                "invalid log entry at offset {offset}: index={}, term={}",
                entry.index, entry.term
            )));
        }
    }
    Ok(())
}
