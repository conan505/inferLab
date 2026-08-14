use std::fmt;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LockedFile {
    Readme,
    Config,
    Checkpoint,
    SpecialTokens,
    Tokenizer,
    TokenizerConfig,
}

impl LockedFile {
    pub const ALL: [Self; 6] = [
        Self::Readme,
        Self::Config,
        Self::Checkpoint,
        Self::SpecialTokens,
        Self::Tokenizer,
        Self::TokenizerConfig,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Readme => "README.md",
            Self::Config => "config.json",
            Self::Checkpoint => "model.safetensors",
            Self::SpecialTokens => "special_tokens_map.json",
            Self::Tokenizer => "tokenizer.json",
            Self::TokenizerConfig => "tokenizer_config.json",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.name() == name)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactErrorKind {
    LockUnavailable,
    LockUnsafe,
    LockOversize,
    LockInvalid,
    LockMismatch,
    AssetDirectoryUnsafe,
    InventoryMismatch,
    FileUnavailable,
    FileUnsafe,
    SizeMismatch,
    HashMismatch,
    ConfigInvalid,
    ConfigMismatch,
    CheckpointInvalid,
    HeaderMismatch,
    TensorInventoryMismatch,
    TensorDtypeMismatch,
    TensorShapeMismatch,
    TensorOffsetMismatch,
    NonFiniteTensor,
    ArithmeticOverflow,
}

impl ArtifactErrorKind {
    const fn code(self) -> &'static str {
        match self {
            Self::LockUnavailable => "lock_unavailable",
            Self::LockUnsafe => "lock_unsafe",
            Self::LockOversize => "lock_oversize",
            Self::LockInvalid => "lock_invalid",
            Self::LockMismatch => "lock_mismatch",
            Self::AssetDirectoryUnsafe => "asset_directory_unsafe",
            Self::InventoryMismatch => "inventory_mismatch",
            Self::FileUnavailable => "file_unavailable",
            Self::FileUnsafe => "file_unsafe",
            Self::SizeMismatch => "size_mismatch",
            Self::HashMismatch => "hash_mismatch",
            Self::ConfigInvalid => "config_invalid",
            Self::ConfigMismatch => "config_mismatch",
            Self::CheckpointInvalid => "checkpoint_invalid",
            Self::HeaderMismatch => "header_mismatch",
            Self::TensorInventoryMismatch => "tensor_inventory_mismatch",
            Self::TensorDtypeMismatch => "tensor_dtype_mismatch",
            Self::TensorShapeMismatch => "tensor_shape_mismatch",
            Self::TensorOffsetMismatch => "tensor_offset_mismatch",
            Self::NonFiniteTensor => "non_finite_tensor",
            Self::ArithmeticOverflow => "arithmetic_overflow",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactError {
    kind: ArtifactErrorKind,
    file: Option<LockedFile>,
}

impl ArtifactError {
    pub(crate) const fn new(kind: ArtifactErrorKind) -> Self {
        Self { kind, file: None }
    }

    pub(crate) const fn for_file(kind: ArtifactErrorKind, file: LockedFile) -> Self {
        Self {
            kind,
            file: Some(file),
        }
    }

    pub const fn kind(self) -> ArtifactErrorKind {
        self.kind
    }

    pub const fn file(self) -> Option<LockedFile> {
        self.file
    }
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "model artifact verification failed: {}",
            self.kind.code()
        )?;
        if let Some(file) = self.file {
            write!(formatter, " ({})", file.name())?;
        }
        Ok(())
    }
}

impl std::error::Error for ArtifactError {}
