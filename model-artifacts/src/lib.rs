//! Offline, fail-closed verification for pinned public model artifacts.

mod error;
mod lock;
mod tokenizer;
mod verify;

pub use error::{ArtifactError, ArtifactErrorKind, LockedFile};
pub use lock::{
    ArchitectureLock, CheckpointLock, FileLock, ModelLock, SourceLock, load_pinned_lock,
    validate_pinned_lock,
};
pub use tokenizer::{
    DecodeSpecialMode, EncodeOptions, LiteralSpecialMode, ProductionTokenizer, TokenizerError,
    TokenizerErrorKind, TokenizerReport,
};
pub use verify::{ArchitectureReport, CheckpointReport, VerificationReport, VerifiedBundle};

/// Verify the immutable v0.32 Pythia bundle without contacting the network.
pub fn load_pinned_pythia(
    lock_path: impl AsRef<std::path::Path>,
    asset_directory: impl AsRef<std::path::Path>,
) -> Result<VerifiedBundle, ArtifactError> {
    let lock = load_pinned_lock(lock_path)?;
    validate_pinned_lock(&lock)?;
    verify::verify_bundle(&lock, asset_directory.as_ref())
}
