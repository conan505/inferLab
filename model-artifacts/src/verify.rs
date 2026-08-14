use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{File, Metadata, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::{
    ffi::{CStr, CString},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    ptr,
};

use safetensors::{Dtype, SafeTensors};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ArchitectureLock, ArtifactError, ArtifactErrorKind, LockedFile, ModelLock,
    lock::validate_structure,
};

const REPORT_SCHEMA: &str = "inferlab.public-model-verification.v1";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOKENIZER_BYTES: u64 = 4 * 1024 * 1024;
const MAX_AUXILIARY_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArchitectureReport {
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
    pub torch_dtype: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckpointReport {
    pub file: String,
    pub format: String,
    pub sha256: String,
    pub header_bytes: u64,
    pub header_sha256: String,
    pub dtype: String,
    pub tensor_count: u64,
    pub element_count: u64,
    pub data_bytes: u64,
    pub finite_payload: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VerificationReport {
    pub schema: &'static str,
    pub repository: String,
    pub revision: String,
    pub license: String,
    pub verified_files: u64,
    pub verified_bytes: u64,
    pub architecture: ArchitectureReport,
    pub checkpoint: CheckpointReport,
}

pub struct VerifiedBundle {
    report: VerificationReport,
    files: BTreeMap<LockedFile, Vec<u8>>,
}

impl VerifiedBundle {
    pub fn report(&self) -> &VerificationReport {
        &self.report
    }

    /// Return bytes only after the complete six-file bundle has verified.
    pub fn bytes(&self, file: LockedFile) -> &[u8] {
        self.files
            .get(&file)
            .map(Vec::as_slice)
            .expect("a verified bundle retains every locked file")
    }

    pub fn checkpoint(&self) -> Result<SafeTensors<'_>, ArtifactError> {
        SafeTensors::deserialize(self.bytes(LockedFile::Checkpoint)).map_err(|_| {
            ArtifactError::for_file(ArtifactErrorKind::CheckpointInvalid, LockedFile::Checkpoint)
        })
    }
}

impl fmt::Debug for VerifiedBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedBundle")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

pub(crate) fn verify_bundle(
    lock: &ModelLock,
    asset_directory: &Path,
) -> Result<VerifiedBundle, ArtifactError> {
    validate_verification_lock(lock)?;
    let directory = AssetDirectory::open(asset_directory)?;
    directory.validate_inventory(lock)?;

    // Every byte is bounded, read, size-checked, and hash-checked before any
    // configuration or checkpoint parser sees it.
    let mut files = BTreeMap::new();
    for entry in &lock.files {
        let file = LockedFile::from_name(&entry.name)
            .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::LockInvalid))?;
        let bytes = directory.read_locked_file(file, entry.bytes)?;
        if sha256_hex(&bytes) != entry.sha256 {
            return Err(ArtifactError::for_file(
                ArtifactErrorKind::HashMismatch,
                file,
            ));
        }
        files.insert(file, bytes);
    }

    // Inventory and every file open are anchored to one descriptor. Reject a
    // renamed/replaced or mutated directory before any parser sees the bytes.
    directory.validate_generation()?;

    let config: PythiaConfig = serde_json::from_slice(
        files
            .get(&LockedFile::Config)
            .expect("validated inventory contains config"),
    )
    .map_err(|_| ArtifactError::for_file(ArtifactErrorKind::ConfigInvalid, LockedFile::Config))?;
    validate_config(&config, &lock.architecture)?;

    validate_checkpoint(
        files
            .get(&LockedFile::Checkpoint)
            .expect("validated inventory contains checkpoint"),
        lock,
    )?;

    let verified_bytes = lock.files.iter().try_fold(0_u64, |total, file| {
        total
            .checked_add(file.bytes)
            .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))
    })?;
    let checkpoint_sha256 = lock
        .files
        .iter()
        .find(|entry| entry.name == LockedFile::Checkpoint.name())
        .map(|entry| entry.sha256.clone())
        .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::LockInvalid))?;
    let architecture = &lock.architecture;
    let report = VerificationReport {
        schema: REPORT_SCHEMA,
        repository: lock.source.repository.clone(),
        revision: lock.source.revision.clone(),
        license: lock.source.license.clone(),
        verified_files: lock.files.len() as u64,
        verified_bytes,
        architecture: ArchitectureReport {
            model_type: architecture.model_type.clone(),
            architecture: architecture.architecture.clone(),
            vocab_size: architecture.vocab_size,
            max_position_embeddings: architecture.max_position_embeddings,
            hidden_size: architecture.hidden_size,
            intermediate_size: architecture.intermediate_size,
            num_attention_heads: architecture.num_attention_heads,
            num_hidden_layers: architecture.num_hidden_layers,
            bos_token_id: architecture.bos_token_id,
            eos_token_id: architecture.eos_token_id,
            hidden_act: architecture.hidden_act.clone(),
            torch_dtype: architecture.torch_dtype.clone(),
        },
        checkpoint: CheckpointReport {
            file: lock.checkpoint.file.clone(),
            format: lock.checkpoint.format.clone(),
            sha256: checkpoint_sha256,
            header_bytes: lock.checkpoint.header_bytes,
            header_sha256: lock.checkpoint.header_sha256.clone(),
            dtype: lock.checkpoint.dtype.clone(),
            tensor_count: lock.checkpoint.tensor_count,
            element_count: lock.checkpoint.element_count,
            data_bytes: lock.checkpoint.data_bytes,
            finite_payload: true,
        },
    };
    Ok(VerifiedBundle { report, files })
}

fn validate_verification_lock(lock: &ModelLock) -> Result<(), ArtifactError> {
    validate_structure(lock)?;
    let architecture = &lock.architecture;
    if architecture.model_type != "gpt_neox"
        || architecture.architecture != "GPTNeoXForCausalLM"
        || architecture.hidden_act != "gelu"
        || architecture.torch_dtype != "float16"
        || !architecture.attention_bias
        || architecture.tie_word_embeddings
        || !architecture.use_parallel_residual
        || architecture.vocab_size == 0
        || architecture.max_position_embeddings == 0
        || architecture.hidden_size == 0
        || architecture.intermediate_size == 0
        || architecture.num_attention_heads == 0
        || architecture.num_hidden_layers == 0
        || architecture.bos_token_id >= architecture.vocab_size
        || architecture.eos_token_id >= architecture.vocab_size
        || !architecture.layer_norm_eps.is_finite()
        || architecture.layer_norm_eps <= 0.0
        || !architecture.rotary_pct.is_finite()
        || architecture.rotary_pct <= 0.0
        || architecture.rotary_pct > 1.0
        || !architecture
            .hidden_size
            .is_multiple_of(architecture.num_attention_heads)
    {
        return Err(ArtifactError::new(ArtifactErrorKind::LockInvalid));
    }
    if architecture
        .hidden_size
        .checked_mul(4)
        .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))?
        != architecture.intermediate_size
    {
        return Err(ArtifactError::new(ArtifactErrorKind::LockInvalid));
    }
    let head_size = architecture.hidden_size / architecture.num_attention_heads;
    let rotary_dimensions = head_size as f64 * architecture.rotary_pct;
    if rotary_dimensions.fract() != 0.0 || !(rotary_dimensions as u64).is_multiple_of(2) {
        return Err(ArtifactError::new(ArtifactErrorKind::LockInvalid));
    }

    for entry in &lock.files {
        let file = LockedFile::from_name(&entry.name)
            .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::LockInvalid))?;
        if entry.bytes > maximum_bytes(file) {
            return Err(ArtifactError::new(ArtifactErrorKind::LockInvalid));
        }
    }

    let schema = expected_tensors(architecture)?;
    let element_count = schema.values().try_fold(0_u64, |total, tensor| {
        total
            .checked_add(tensor.elements)
            .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))
    })?;
    let data_bytes = element_count
        .checked_mul(2)
        .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))?;
    let expected_file_bytes = 8_u64
        .checked_add(lock.checkpoint.header_bytes)
        .and_then(|value| value.checked_add(data_bytes))
        .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))?;
    let checkpoint_file = lock
        .files
        .iter()
        .find(|entry| entry.name == LockedFile::Checkpoint.name())
        .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::LockInvalid))?;
    if lock.checkpoint.tensor_count != schema.len() as u64
        || lock.checkpoint.element_count != element_count
        || lock.checkpoint.data_bytes != data_bytes
        || checkpoint_file.bytes != expected_file_bytes
    {
        return Err(ArtifactError::new(ArtifactErrorKind::LockInvalid));
    }
    Ok(())
}

struct AssetDirectory {
    descriptor: File,
    path: PathBuf,
    initial_metadata: Metadata,
}

impl AssetDirectory {
    fn open(path: &Path) -> Result<Self, ArtifactError> {
        let path_metadata = std::fs::symlink_metadata(path)
            .map_err(|_| ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe))?;
        if path_metadata.file_type().is_symlink() || !path_metadata.file_type().is_dir() {
            return Err(ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe));
        }

        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(
            libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        );
        let descriptor = options
            .open(path)
            .map_err(|_| ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe))?;
        let descriptor_metadata = descriptor
            .metadata()
            .map_err(|_| ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe))?;
        if !descriptor_metadata.file_type().is_dir()
            || !same_object(&path_metadata, &descriptor_metadata)
        {
            return Err(ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe));
        }

        Ok(Self {
            descriptor,
            path: path.to_owned(),
            initial_metadata: descriptor_metadata,
        })
    }

    fn validate_inventory(&self, lock: &ModelLock) -> Result<(), ArtifactError> {
        let actual = self.descriptor_inventory()?;
        let expected = lock
            .files
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<BTreeSet<_>>();
        if actual != expected {
            return Err(ArtifactError::new(ArtifactErrorKind::InventoryMismatch));
        }
        Ok(())
    }

    fn read_locked_file(
        &self,
        locked_file: LockedFile,
        expected_bytes: u64,
    ) -> Result<Vec<u8>, ArtifactError> {
        let mut file = self.open_child(locked_file)?;
        let before = file.metadata().map_err(|_| {
            ArtifactError::for_file(ArtifactErrorKind::FileUnavailable, locked_file)
        })?;
        if !before.file_type().is_file() {
            return Err(ArtifactError::for_file(
                ArtifactErrorKind::FileUnsafe,
                locked_file,
            ));
        }
        if before.len() != expected_bytes {
            return Err(ArtifactError::for_file(
                ArtifactErrorKind::SizeMismatch,
                locked_file,
            ));
        }

        let capacity = usize::try_from(expected_bytes)
            .map_err(|_| ArtifactError::for_file(ArtifactErrorKind::SizeMismatch, locked_file))?;
        let read_limit = expected_bytes.checked_add(1).ok_or_else(|| {
            ArtifactError::for_file(ArtifactErrorKind::ArithmeticOverflow, locked_file)
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        file.by_ref()
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|_| {
                ArtifactError::for_file(ArtifactErrorKind::FileUnavailable, locked_file)
            })?;
        let after = file.metadata().map_err(|_| {
            ArtifactError::for_file(ArtifactErrorKind::FileUnavailable, locked_file)
        })?;
        if !same_file_generation(&before, &after) {
            return Err(ArtifactError::for_file(
                ArtifactErrorKind::FileUnsafe,
                locked_file,
            ));
        }
        if bytes.len() as u64 != expected_bytes {
            return Err(ArtifactError::for_file(
                ArtifactErrorKind::SizeMismatch,
                locked_file,
            ));
        }
        Ok(bytes)
    }

    fn validate_generation(&self) -> Result<(), ArtifactError> {
        let descriptor_metadata = self
            .descriptor
            .metadata()
            .map_err(|_| ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe))?;
        let path_metadata = std::fs::symlink_metadata(&self.path)
            .map_err(|_| ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe))?;
        if path_metadata.file_type().is_symlink()
            || !path_metadata.file_type().is_dir()
            || !same_object(&descriptor_metadata, &path_metadata)
            || !same_directory_generation(&self.initial_metadata, &descriptor_metadata)
        {
            return Err(ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn open_child(&self, locked_file: LockedFile) -> Result<File, ArtifactError> {
        let name = CString::new(locked_file.name()).map_err(|_| {
            ArtifactError::for_file(ArtifactErrorKind::FileUnavailable, locked_file)
        })?;
        // SAFETY: the directory fd is live, the C string is NUL-terminated,
        // and a successful fd is immediately transferred into `File`.
        let descriptor = unsafe {
            libc::openat(
                self.descriptor.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            let kind = if error.raw_os_error() == Some(libc::ELOOP) {
                ArtifactErrorKind::FileUnsafe
            } else {
                ArtifactErrorKind::FileUnavailable
            };
            return Err(ArtifactError::for_file(kind, locked_file));
        }
        // SAFETY: openat returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    #[cfg(not(unix))]
    fn open_child(&self, locked_file: LockedFile) -> Result<File, ArtifactError> {
        OpenOptions::new()
            .read(true)
            .open(self.path.join(locked_file.name()))
            .map_err(|_| ArtifactError::for_file(ArtifactErrorKind::FileUnavailable, locked_file))
    }

    #[cfg(unix)]
    fn descriptor_inventory(&self) -> Result<BTreeSet<String>, ArtifactError> {
        // fdopendir owns its fd, so duplicate the pinned descriptor first.
        // SAFETY: fcntl only observes the live directory descriptor.
        let duplicate =
            unsafe { libc::fcntl(self.descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe));
        }
        // SAFETY: duplicate is a valid owned directory descriptor. fdopendir
        // assumes ownership on success; close it ourselves on failure.
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            // SAFETY: fdopendir did not consume the descriptor on failure.
            unsafe { libc::close(duplicate) };
            return Err(ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe));
        }
        let stream = DirectoryStream(stream);
        let mut actual = BTreeSet::new();
        loop {
            reset_errno();
            // SAFETY: DirectoryStream keeps the DIR pointer live exclusively.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                if current_errno() != 0 {
                    return Err(ArtifactError::new(ArtifactErrorKind::InventoryMismatch));
                }
                break;
            }
            // SAFETY: readdir returns a dirent whose d_name is NUL-terminated
            // and remains valid until the next operation on this stream.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
                .to_str()
                .map_err(|_| ArtifactError::new(ArtifactErrorKind::InventoryMismatch))?;
            if name != "." && name != ".." {
                actual.insert(name.to_owned());
            }
        }
        Ok(actual)
    }

    #[cfg(not(unix))]
    fn descriptor_inventory(&self) -> Result<BTreeSet<String>, ArtifactError> {
        let mut actual = BTreeSet::new();
        for entry in std::fs::read_dir(&self.path)
            .map_err(|_| ArtifactError::new(ArtifactErrorKind::AssetDirectoryUnsafe))?
        {
            let entry =
                entry.map_err(|_| ArtifactError::new(ArtifactErrorKind::InventoryMismatch))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| ArtifactError::new(ArtifactErrorKind::InventoryMismatch))?;
            actual.insert(name);
        }
        Ok(actual)
    }
}

#[cfg(unix)]
struct DirectoryStream(*mut libc::DIR);

#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the stream from fdopendir.
        unsafe { libc::closedir(self.0) };
    }
}

#[cfg(unix)]
fn errno_location() -> *mut libc::c_int {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    // SAFETY: libc exposes a thread-local errno pointer on BSD targets.
    unsafe {
        libc::__error()
    }

    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    // SAFETY: libc exposes a thread-local errno pointer on Linux-like targets.
    unsafe {
        libc::__errno_location()
    }
}

#[cfg(unix)]
fn reset_errno() {
    // SAFETY: errno_location returns the current thread's writable errno.
    unsafe { ptr::write(errno_location(), 0) };
}

#[cfg(unix)]
fn current_errno() -> libc::c_int {
    // SAFETY: errno_location returns the current thread's readable errno.
    unsafe { ptr::read(errno_location()) }
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

fn same_directory_generation(left: &Metadata, right: &Metadata) -> bool {
    same_file_generation(left, right)
}

fn maximum_bytes(file: LockedFile) -> u64 {
    match file {
        LockedFile::Config => MAX_CONFIG_BYTES,
        LockedFile::Checkpoint => MAX_CHECKPOINT_BYTES,
        LockedFile::Tokenizer => MAX_TOKENIZER_BYTES,
        LockedFile::Readme | LockedFile::SpecialTokens | LockedFile::TokenizerConfig => {
            MAX_AUXILIARY_BYTES
        }
    }
}

fn validate_checkpoint(bytes: &[u8], lock: &ModelLock) -> Result<(), ArtifactError> {
    let file = LockedFile::Checkpoint;
    let header_prefix: [u8; 8] = bytes
        .get(..8)
        .and_then(|prefix| prefix.try_into().ok())
        .ok_or_else(|| ArtifactError::for_file(ArtifactErrorKind::CheckpointInvalid, file))?;
    let header_bytes = u64::from_le_bytes(header_prefix);
    if header_bytes != lock.checkpoint.header_bytes {
        return Err(ArtifactError::for_file(
            ArtifactErrorKind::HeaderMismatch,
            file,
        ));
    }
    let header_end =
        8_usize
            .checked_add(usize::try_from(header_bytes).map_err(|_| {
                ArtifactError::for_file(ArtifactErrorKind::ArithmeticOverflow, file)
            })?)
            .ok_or_else(|| ArtifactError::for_file(ArtifactErrorKind::ArithmeticOverflow, file))?;
    let header = bytes
        .get(8..header_end)
        .ok_or_else(|| ArtifactError::for_file(ArtifactErrorKind::CheckpointInvalid, file))?;
    if sha256_hex(header) != lock.checkpoint.header_sha256 {
        return Err(ArtifactError::for_file(
            ArtifactErrorKind::HeaderMismatch,
            file,
        ));
    }

    let (_, metadata) = SafeTensors::read_metadata(bytes)
        .map_err(|_| ArtifactError::for_file(ArtifactErrorKind::CheckpointInvalid, file))?;
    let tensors = metadata.tensors();
    let expected = expected_tensors(&lock.architecture)?;
    if tensors.len() != expected.len()
        || tensors.keys().collect::<BTreeSet<_>>() != expected.keys().collect::<BTreeSet<_>>()
    {
        return Err(ArtifactError::for_file(
            ArtifactErrorKind::TensorInventoryMismatch,
            file,
        ));
    }
    // Classify schema failures deterministically. A dtype change can alter the
    // serializer's ordering and therefore offsets, so dtype and shape take
    // precedence over derived layout mismatches.
    if expected.keys().any(|name| {
        tensors
            .get(name)
            .is_none_or(|tensor| tensor.dtype != Dtype::F16)
    }) {
        return Err(ArtifactError::for_file(
            ArtifactErrorKind::TensorDtypeMismatch,
            file,
        ));
    }
    if expected.iter().any(|(name, expected_tensor)| {
        tensors
            .get(name)
            .is_none_or(|tensor| tensor.shape != expected_tensor.shape)
    }) {
        return Err(ArtifactError::for_file(
            ArtifactErrorKind::TensorShapeMismatch,
            file,
        ));
    }
    for (name, expected_tensor) in &expected {
        let tensor = tensors.get(name).ok_or_else(|| {
            ArtifactError::for_file(ArtifactErrorKind::TensorInventoryMismatch, file)
        })?;
        if tensor.data_offsets != (expected_tensor.start, expected_tensor.end) {
            return Err(ArtifactError::for_file(
                ArtifactErrorKind::TensorOffsetMismatch,
                file,
            ));
        }
    }

    let parsed = SafeTensors::deserialize(bytes)
        .map_err(|_| ArtifactError::for_file(ArtifactErrorKind::CheckpointInvalid, file))?;
    for name in expected.keys() {
        let tensor = parsed
            .tensor(name)
            .map_err(|_| ArtifactError::for_file(ArtifactErrorKind::CheckpointInvalid, file))?;
        if !f16_payload_is_finite(tensor.data()) {
            return Err(ArtifactError::for_file(
                ArtifactErrorKind::NonFiniteTensor,
                file,
            ));
        }
    }
    Ok(())
}

fn f16_payload_is_finite(bytes: &[u8]) -> bool {
    bytes.len().is_multiple_of(2)
        && bytes.chunks_exact(2).all(|chunk| {
            let bits = u16::from_le_bytes([chunk[0], chunk[1]]);
            bits & 0x7c00 != 0x7c00
        })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PythiaConfig {
    architectures: Vec<String>,
    attention_bias: bool,
    attention_dropout: f64,
    bos_token_id: u64,
    classifier_dropout: f64,
    eos_token_id: u64,
    hidden_act: String,
    hidden_dropout: f64,
    hidden_size: u64,
    initializer_range: f64,
    intermediate_size: u64,
    layer_norm_eps: f64,
    max_position_embeddings: u64,
    model_type: String,
    num_attention_heads: u64,
    num_hidden_layers: u64,
    rope_scaling: Option<serde_json::Value>,
    rotary_emb_base: u64,
    rotary_pct: f64,
    tie_word_embeddings: bool,
    torch_dtype: String,
    transformers_version: String,
    use_cache: bool,
    use_parallel_residual: bool,
    vocab_size: u64,
}

fn validate_config(
    config: &PythiaConfig,
    expected: &ArchitectureLock,
) -> Result<(), ArtifactError> {
    let matches_lock = config.architectures == [expected.architecture.as_str()]
        && config.attention_bias == expected.attention_bias
        && config.bos_token_id == expected.bos_token_id
        && config.eos_token_id == expected.eos_token_id
        && config.hidden_act == expected.hidden_act
        && config.hidden_size == expected.hidden_size
        && config.intermediate_size == expected.intermediate_size
        && config.layer_norm_eps == expected.layer_norm_eps
        && config.max_position_embeddings == expected.max_position_embeddings
        && config.model_type == expected.model_type
        && config.num_attention_heads == expected.num_attention_heads
        && config.num_hidden_layers == expected.num_hidden_layers
        && config.rotary_emb_base == expected.rotary_emb_base
        && config.rotary_pct == expected.rotary_pct
        && config.tie_word_embeddings == expected.tie_word_embeddings
        && config.torch_dtype == expected.torch_dtype
        && config.use_parallel_residual == expected.use_parallel_residual
        && config.vocab_size == expected.vocab_size;
    let fixed_invariants = config.attention_dropout == 0.0
        && config.classifier_dropout == 0.1
        && config.hidden_dropout == 0.0
        && config.initializer_range == 0.02
        && config.rope_scaling.is_none()
        && config.transformers_version == "4.40.0"
        && config.use_cache;
    if !matches_lock || !fixed_invariants {
        return Err(ArtifactError::for_file(
            ArtifactErrorKind::ConfigMismatch,
            LockedFile::Config,
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ExpectedTensor {
    shape: Vec<usize>,
    elements: u64,
    start: usize,
    end: usize,
}

fn expected_tensors(
    architecture: &ArchitectureLock,
) -> Result<BTreeMap<String, ExpectedTensor>, ArtifactError> {
    let hidden = usize_value(architecture.hidden_size)?;
    let intermediate = usize_value(architecture.intermediate_size)?;
    let vocab = usize_value(architecture.vocab_size)?;
    let layers = usize_value(architecture.num_hidden_layers)?;
    let triple_hidden = hidden
        .checked_mul(3)
        .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))?;
    let mut shapes = BTreeMap::<String, Vec<usize>>::new();
    shapes.insert("embed_out.weight".to_owned(), vec![vocab, hidden]);
    shapes.insert("gpt_neox.embed_in.weight".to_owned(), vec![vocab, hidden]);
    shapes.insert("gpt_neox.final_layer_norm.bias".to_owned(), vec![hidden]);
    shapes.insert("gpt_neox.final_layer_norm.weight".to_owned(), vec![hidden]);
    for layer in 0..layers {
        let prefix = format!("gpt_neox.layers.{layer}");
        shapes.insert(format!("{prefix}.attention.dense.bias"), vec![hidden]);
        shapes.insert(
            format!("{prefix}.attention.dense.weight"),
            vec![hidden, hidden],
        );
        shapes.insert(
            format!("{prefix}.attention.query_key_value.bias"),
            vec![triple_hidden],
        );
        shapes.insert(
            format!("{prefix}.attention.query_key_value.weight"),
            vec![triple_hidden, hidden],
        );
        shapes.insert(format!("{prefix}.input_layernorm.bias"), vec![hidden]);
        shapes.insert(format!("{prefix}.input_layernorm.weight"), vec![hidden]);
        shapes.insert(format!("{prefix}.mlp.dense_4h_to_h.bias"), vec![hidden]);
        shapes.insert(
            format!("{prefix}.mlp.dense_4h_to_h.weight"),
            vec![hidden, intermediate],
        );
        shapes.insert(
            format!("{prefix}.mlp.dense_h_to_4h.bias"),
            vec![intermediate],
        );
        shapes.insert(
            format!("{prefix}.mlp.dense_h_to_4h.weight"),
            vec![intermediate, hidden],
        );
        shapes.insert(
            format!("{prefix}.post_attention_layernorm.bias"),
            vec![hidden],
        );
        shapes.insert(
            format!("{prefix}.post_attention_layernorm.weight"),
            vec![hidden],
        );
    }

    let mut offset = 0_usize;
    let mut expected = BTreeMap::new();
    for (name, shape) in shapes {
        let elements = shape.iter().try_fold(1_usize, |product, dimension| {
            product
                .checked_mul(*dimension)
                .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))
        })?;
        let byte_length = elements
            .checked_mul(2)
            .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))?;
        let end = offset
            .checked_add(byte_length)
            .ok_or_else(|| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))?;
        expected.insert(
            name,
            ExpectedTensor {
                shape,
                elements: u64::try_from(elements)
                    .map_err(|_| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))?,
                start: offset,
                end,
            },
        );
        offset = end;
    }
    Ok(expected)
}

fn usize_value(value: u64) -> Result<usize, ArtifactError> {
    usize::try_from(value).map_err(|_| ArtifactError::new(ArtifactErrorKind::ArithmeticOverflow))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests;
