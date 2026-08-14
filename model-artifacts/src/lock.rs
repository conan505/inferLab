use std::{
    fs::{Metadata, OpenOptions},
    io::Read,
    path::Path,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use serde::{Deserialize, Serialize};

use crate::{ArtifactError, ArtifactErrorKind, LockedFile};

pub(crate) const LOCK_SCHEMA: &str = "inferlab.public-model-lock.v1";
pub(crate) const REPOSITORY: &str = "EleutherAI/pythia-14m";
pub(crate) const REVISION: &str = "cf967c0a9a04383db6f7b1108d86b2962634b4ac";
pub(crate) const LICENSE: &str = "Apache-2.0";
const LOCK_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLock {
    pub schema: String,
    pub source: SourceLock,
    pub files: Vec<FileLock>,
    pub checkpoint: CheckpointLock,
    pub architecture: ArchitectureLock,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLock {
    pub repository: String,
    pub revision: String,
    pub license: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileLock {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointLock {
    pub file: String,
    pub format: String,
    pub header_bytes: u64,
    pub header_sha256: String,
    pub dtype: String,
    pub tensor_count: u64,
    pub element_count: u64,
    pub data_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArchitectureLock {
    pub model_type: String,
    pub architecture: String,
    pub vocab_size: u64,
    pub max_position_embeddings: u64,
    pub hidden_size: u64,
    pub intermediate_size: u64,
    pub num_attention_heads: u64,
    pub num_hidden_layers: u64,
    pub bos_token_id: u64,
    pub eos_token_id: u64,
    pub hidden_act: String,
    pub layer_norm_eps: f64,
    pub rotary_pct: f64,
    pub rotary_emb_base: u64,
    pub attention_bias: bool,
    pub tie_word_embeddings: bool,
    pub use_parallel_residual: bool,
    pub torch_dtype: String,
}

pub fn load_pinned_lock(path: impl AsRef<Path>) -> Result<ModelLock, ArtifactError> {
    let bytes = read_lock(path.as_ref())?;
    let lock = serde_json::from_slice(&bytes)
        .map_err(|_| ArtifactError::new(ArtifactErrorKind::LockInvalid))?;
    validate_structure(&lock)?;
    Ok(lock)
}

pub fn validate_pinned_lock(lock: &ModelLock) -> Result<(), ArtifactError> {
    validate_structure(lock)?;
    if lock.source.repository != REPOSITORY
        || lock.source.revision != REVISION
        || lock.source.license != LICENSE
        || lock.architecture != canonical_architecture()
        || lock.files != canonical_files()
        || lock.checkpoint != canonical_checkpoint()
    {
        return Err(ArtifactError::new(ArtifactErrorKind::LockMismatch));
    }
    Ok(())
}

pub(crate) fn validate_structure(lock: &ModelLock) -> Result<(), ArtifactError> {
    if lock.schema != LOCK_SCHEMA
        || lock.files.len() != LockedFile::ALL.len()
        || lock.source.repository.is_empty()
        || !is_lower_hex(&lock.source.revision, 40)
        || lock.source.license.is_empty()
    {
        return Err(ArtifactError::new(ArtifactErrorKind::LockInvalid));
    }
    for (entry, expected) in lock.files.iter().zip(LockedFile::ALL) {
        if entry.name != expected.name() || entry.bytes == 0 || !is_lower_hex(&entry.sha256, 64) {
            return Err(ArtifactError::new(ArtifactErrorKind::LockInvalid));
        }
    }
    if lock.checkpoint.file != LockedFile::Checkpoint.name()
        || lock.checkpoint.format != "safetensors"
        || lock.checkpoint.dtype != "F16"
        || !is_lower_hex(&lock.checkpoint.header_sha256, 64)
    {
        return Err(ArtifactError::new(ArtifactErrorKind::LockInvalid));
    }
    Ok(())
}

fn canonical_files() -> Vec<FileLock> {
    [
        (
            "README.md",
            10_560,
            "d1f2cf1d5181daedeaa70208ddd5cc5251867bde9acf6db7bb45a2265e25e163",
        ),
        (
            "config.json",
            698,
            "f97f966a66c444890ed461fff2a51eefb15d74303df05b948124719f199b0b17",
        ),
        (
            "model.safetensors",
            28_143_920,
            "116a02532db461f91386a5b20f942ff2c8d4de7341e21b55caafc3d7b25f49a1",
        ),
        (
            "special_tokens_map.json",
            441,
            "10b8c8852c1e1f70b54d9aff61728408c28971c0e97a6c5a7b2debbd1d3e9c0c",
        ),
        (
            "tokenizer.json",
            2_114_042,
            "870f4e2baa6b683221fa52004d5d6f40ab8c9d31961617304b78c910c2c3caf2",
        ),
        (
            "tokenizer_config.json",
            4_834,
            "eee017c5bd133137f45907bd0a6e781e2ccd1a533734b7ed2a2f2f4446659809",
        ),
    ]
    .into_iter()
    .map(|(name, bytes, sha256)| FileLock {
        name: name.to_owned(),
        bytes,
        sha256: sha256.to_owned(),
    })
    .collect()
}

fn canonical_checkpoint() -> CheckpointLock {
    CheckpointLock {
        file: "model.safetensors".to_owned(),
        format: "safetensors".to_owned(),
        header_bytes: 8_488,
        header_sha256: "da85647d12efa36759dba812776603f6989559e6bf75446d3273c5fd0fe0e11d"
            .to_owned(),
        dtype: "F16".to_owned(),
        tensor_count: 76,
        element_count: 14_067_712,
        data_bytes: 28_135_424,
    }
}

fn canonical_architecture() -> ArchitectureLock {
    ArchitectureLock {
        model_type: "gpt_neox".to_owned(),
        architecture: "GPTNeoXForCausalLM".to_owned(),
        vocab_size: 50_304,
        max_position_embeddings: 2_048,
        hidden_size: 128,
        intermediate_size: 512,
        num_attention_heads: 4,
        num_hidden_layers: 6,
        bos_token_id: 0,
        eos_token_id: 0,
        hidden_act: "gelu".to_owned(),
        layer_norm_eps: 0.00001,
        rotary_pct: 0.25,
        rotary_emb_base: 10_000,
        attention_bias: true,
        tie_word_embeddings: false,
        use_parallel_residual: true,
        torch_dtype: "float16".to_owned(),
    }
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn read_lock(path: &Path) -> Result<Vec<u8>, ArtifactError> {
    let path_metadata = std::fs::symlink_metadata(path)
        .map_err(|_| ArtifactError::new(ArtifactErrorKind::LockUnavailable))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.file_type().is_file() {
        return Err(ArtifactError::new(ArtifactErrorKind::LockUnsafe));
    }
    if path_metadata.len() > LOCK_LIMIT {
        return Err(ArtifactError::new(ArtifactErrorKind::LockOversize));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = options.open(path).map_err(|error| {
        if error.raw_os_error() == Some(libc::ELOOP) {
            ArtifactError::new(ArtifactErrorKind::LockUnsafe)
        } else {
            ArtifactError::new(ArtifactErrorKind::LockUnavailable)
        }
    })?;
    let descriptor_metadata = file
        .metadata()
        .map_err(|_| ArtifactError::new(ArtifactErrorKind::LockUnavailable))?;
    if !descriptor_metadata.file_type().is_file()
        || !same_object(&path_metadata, &descriptor_metadata)
    {
        return Err(ArtifactError::new(ArtifactErrorKind::LockUnsafe));
    }
    if descriptor_metadata.len() > LOCK_LIMIT {
        return Err(ArtifactError::new(ArtifactErrorKind::LockOversize));
    }

    let capacity = usize::try_from(descriptor_metadata.len())
        .map_err(|_| ArtifactError::new(ArtifactErrorKind::LockOversize))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref()
        .take(LOCK_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ArtifactError::new(ArtifactErrorKind::LockUnavailable))?;
    if bytes.len() as u64 > LOCK_LIMIT {
        return Err(ArtifactError::new(ArtifactErrorKind::LockOversize));
    }
    let after = file
        .metadata()
        .map_err(|_| ArtifactError::new(ArtifactErrorKind::LockUnavailable))?;
    let path_after = std::fs::symlink_metadata(path)
        .map_err(|_| ArtifactError::new(ArtifactErrorKind::LockUnsafe))?;
    if path_after.file_type().is_symlink()
        || !path_after.file_type().is_file()
        || !same_object(&after, &path_after)
        || !same_file_generation(&descriptor_metadata, &after)
        || bytes.len() as u64 != descriptor_metadata.len()
    {
        return Err(ArtifactError::new(ArtifactErrorKind::LockUnsafe));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn same_object(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_object(left: &Metadata, right: &Metadata) -> bool {
    left.file_type() == right.file_type() && left.len() == right.len()
}

#[cfg(unix)]
fn same_file_generation(left: &Metadata, right: &Metadata) -> bool {
    same_object(left, right)
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_file_generation(left: &Metadata, right: &Metadata) -> bool {
    same_object(left, right) && left.modified().ok() == right.modified().ok()
}
