use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use safetensors::{Dtype, tensor::TensorView};
use serde_json::json;

use super::{
    AssetDirectory, expected_tensors, sha256_hex, validate_verification_lock, verify_bundle,
};
use crate::{
    ArchitectureLock, ArtifactErrorKind, CheckpointLock, FileLock, LockedFile, ModelLock,
    SourceLock, load_pinned_lock, validate_pinned_lock,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

#[test]
fn committed_lock_matches_the_exact_pinned_release_contract() {
    let lock_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../models/public/pythia-14m-v0.32.lock.json");
    let lock = load_pinned_lock(lock_path).expect("committed lock parses");
    validate_pinned_lock(&lock).expect("committed lock is the canonical release lock");
    validate_verification_lock(&lock).expect("canonical schema algebra is internally consistent");
    assert_eq!(lock.source.license, "Apache-2.0");
    assert_eq!(lock.files.len(), 6);
    assert_eq!(
        lock.files.iter().map(|file| file.bytes).sum::<u64>(),
        30_274_495
    );
    assert_eq!(lock.checkpoint.tensor_count, 76);
    assert_eq!(lock.checkpoint.element_count, 14_067_712);
}

#[test]
fn lock_reader_is_bounded_strict_and_pinned_to_the_public_source() {
    let directory = TestDirectory::new();
    let corrupt = directory.path.join("corrupt.lock.json");
    fs::write(&corrupt, b"{").expect("write corrupt lock");
    assert_eq!(
        load_pinned_lock(&corrupt).expect_err("corrupt lock").kind(),
        ArtifactErrorKind::LockInvalid
    );

    let oversized = directory.path.join("oversized.lock.json");
    fs::write(&oversized, vec![b' '; 64 * 1024 + 1]).expect("write oversized lock");
    assert_eq!(
        load_pinned_lock(&oversized)
            .expect_err("oversized lock")
            .kind(),
        ArtifactErrorKind::LockOversize
    );

    let missing = directory.path.join("missing.lock.json");
    assert_eq!(
        load_pinned_lock(&missing).expect_err("missing lock").kind(),
        ArtifactErrorKind::LockUnavailable
    );

    let canonical =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../models/public/pythia-14m-v0.32.lock.json");
    let mut unknown: serde_json::Value =
        serde_json::from_slice(&fs::read(&canonical).expect("canonical lock"))
            .expect("canonical lock JSON");
    unknown["unexpected"] = json!(true);
    let unknown_path = directory.path.join("unknown.lock.json");
    fs::write(
        &unknown_path,
        serde_json::to_vec(&unknown).expect("unknown lock JSON"),
    )
    .expect("write unknown lock");
    assert_eq!(
        load_pinned_lock(&unknown_path)
            .expect_err("unknown lock field")
            .kind(),
        ArtifactErrorKind::LockInvalid
    );

    unknown
        .as_object_mut()
        .expect("lock object")
        .remove("unexpected");
    unknown["source"]["revision"] = json!("CF967c0a9a04383db6f7b1108d86b2962634b4ac");
    let invalid_source_path = directory.path.join("invalid-source.lock.json");
    fs::write(
        &invalid_source_path,
        serde_json::to_vec(&unknown).expect("invalid source lock JSON"),
    )
    .expect("write invalid source lock");
    assert_eq!(
        load_pinned_lock(&invalid_source_path)
            .expect_err("revision must be lowercase hex")
            .kind(),
        ArtifactErrorKind::LockInvalid
    );

    let mut mismatched = load_pinned_lock(canonical).expect("canonical lock parses");
    mismatched.source.license = "apache-2.0".to_owned();
    assert_eq!(
        validate_pinned_lock(&mismatched)
            .expect_err("SPDX case is exact")
            .kind(),
        ArtifactErrorKind::LockMismatch
    );
}

#[cfg(unix)]
#[test]
fn lock_reader_rejects_symlinks_and_fifos_without_blocking() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new();
    let target = directory.path.join("target.lock.json");
    fs::write(&target, b"{}").expect("write symlink target");
    let linked = directory.path.join("linked.lock.json");
    symlink(&target, &linked).expect("create lock symlink");
    assert_eq!(
        load_pinned_lock(&linked)
            .expect_err("symlinked lock")
            .kind(),
        ArtifactErrorKind::LockUnsafe
    );

    let fifo = directory.path.join("fifo.lock.json");
    make_fifo(&fifo);
    assert_eq!(
        load_pinned_lock(&fifo).expect_err("FIFO lock").kind(),
        ArtifactErrorKind::LockUnsafe
    );
}

#[test]
fn finite_exact_small_bundle_verifies_and_reports_without_paths() {
    let fixture = Fixture::new();
    let bundle = fixture.verify().expect("fixture verifies");
    let report = bundle.report();
    assert_eq!(report.verified_files, 6);
    assert_eq!(
        report.verified_bytes,
        fixture
            .lock
            .files
            .iter()
            .map(|file| file.bytes)
            .sum::<u64>()
    );
    assert_eq!(report.checkpoint.tensor_count, 16);
    assert!(report.checkpoint.finite_payload);
    assert_eq!(
        bundle
            .checkpoint()
            .expect("checkpoint remains usable")
            .len(),
        16
    );
    let rendered = serde_json::to_string(report).expect("report JSON");
    assert!(!rendered.contains(&fixture.directory.path.display().to_string()));
    assert!(!format!("{bundle:?}").contains(&fixture.directory.path.display().to_string()));
}

#[test]
fn missing_or_extra_assets_reject_the_entire_inventory() {
    let missing = Fixture::new();
    fs::remove_file(missing.path(LockedFile::Readme)).expect("remove fixture file");
    assert_kind(missing.verify(), ArtifactErrorKind::InventoryMismatch);

    let extra = Fixture::new();
    fs::write(extra.directory.path.join("unexpected.json"), b"{}").expect("extra fixture");
    assert_kind(extra.verify(), ArtifactErrorKind::InventoryMismatch);
}

#[cfg(unix)]
#[test]
fn symlinked_asset_is_rejected_without_following_it() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    fs::remove_file(fixture.path(LockedFile::Config)).expect("remove config");
    symlink(
        fixture.path(LockedFile::Readme),
        fixture.path(LockedFile::Config),
    )
    .expect("create symlink");
    let error = fixture.verify().expect_err("symlink fails closed");
    assert_eq!(error.kind(), ArtifactErrorKind::FileUnsafe);
    assert_eq!(error.file(), Some(LockedFile::Config));
}

#[cfg(unix)]
#[test]
fn fifo_asset_is_rejected_without_blocking() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.path(LockedFile::Config)).expect("remove config");
    make_fifo(&fixture.path(LockedFile::Config));
    let error = fixture.verify().expect_err("FIFO fails closed");
    assert_eq!(error.kind(), ArtifactErrorKind::FileUnsafe);
    assert_eq!(error.file(), Some(LockedFile::Config));
}

#[cfg(unix)]
#[test]
fn symlinked_or_replaced_asset_directory_fails_closed() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let links = TestDirectory::new();
    let linked = links.path.join("assets");
    symlink(&fixture.directory.path, &linked).expect("link asset directory");
    assert_kind(
        verify_bundle(&fixture.lock, &linked),
        ArtifactErrorKind::AssetDirectoryUnsafe,
    );

    let pinned = AssetDirectory::open(&fixture.directory.path).expect("pin directory");
    pinned
        .validate_inventory(&fixture.lock)
        .expect("descriptor inventory");
    let moved = fixture.directory.path.with_extension("moved");
    fs::rename(&fixture.directory.path, &moved).expect("move pinned generation");
    fs::create_dir(&fixture.directory.path).expect("install replacement generation");
    let result = pinned.validate_generation();
    fs::remove_dir(&fixture.directory.path).expect("remove replacement generation");
    fs::rename(&moved, &fixture.directory.path).expect("restore fixture generation");
    assert_eq!(
        result.expect_err("replacement must fail").kind(),
        ArtifactErrorKind::AssetDirectoryUnsafe
    );
}

#[test]
fn size_and_hash_are_checked_before_parsing() {
    let size = Fixture::new();
    let mut readme = fs::read(size.path(LockedFile::Readme)).expect("read README");
    readme.push(b'x');
    fs::write(size.path(LockedFile::Readme), readme).expect("grow README");
    let error = size.verify().expect_err("wrong size fails");
    assert_eq!(error.kind(), ArtifactErrorKind::SizeMismatch);
    assert_eq!(error.file(), Some(LockedFile::Readme));

    let hash = Fixture::new();
    let mut config = fs::read(hash.path(LockedFile::Config)).expect("read config");
    config[0] ^= 1;
    fs::write(hash.path(LockedFile::Config), config).expect("corrupt config");
    let error = hash
        .verify()
        .expect_err("wrong hash fails before JSON parse");
    assert_eq!(error.kind(), ArtifactErrorKind::HashMismatch);
    assert_eq!(error.file(), Some(LockedFile::Config));
}

#[test]
fn verified_but_invalid_or_mismatched_config_is_rejected() {
    let mut invalid = Fixture::new();
    invalid.replace_and_refresh(LockedFile::Config, b"{".to_vec());
    assert_kind(invalid.verify(), ArtifactErrorKind::ConfigInvalid);

    let mut mismatch = Fixture::new();
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(mismatch.path(LockedFile::Config)).expect("config"))
            .expect("config JSON");
    config["use_cache"] = json!(false);
    mismatch.replace_and_refresh(
        LockedFile::Config,
        serde_json::to_vec(&config).expect("serialize config"),
    );
    assert_kind(mismatch.verify(), ArtifactErrorKind::ConfigMismatch);
}

#[test]
fn checkpoint_header_hash_and_parser_fail_closed() {
    let mut mismatch = Fixture::new();
    mismatch.lock.checkpoint.header_sha256 = "0".repeat(64);
    assert_kind(mismatch.verify(), ArtifactErrorKind::HeaderMismatch);

    let mut invalid = Fixture::new();
    let mut checkpoint = fs::read(invalid.path(LockedFile::Checkpoint)).expect("checkpoint");
    checkpoint[8] = b'!';
    invalid.replace_checkpoint_and_refresh(checkpoint);
    assert_kind(invalid.verify(), ArtifactErrorKind::CheckpointInvalid);
}

#[test]
fn tensor_dtype_shape_and_offsets_are_exact() {
    let mut inventory = Fixture::new();
    let checkpoint = rename_tensor_in_header(serialize_checkpoint(
        &inventory.lock.architecture,
        CheckpointMutation::None,
    ));
    inventory.replace_checkpoint_and_refresh(checkpoint);
    assert_kind(
        inventory.verify(),
        ArtifactErrorKind::TensorInventoryMismatch,
    );

    let mut dtype = Fixture::new();
    dtype.replace_checkpoint_and_refresh(serialize_checkpoint(
        &dtype.lock.architecture,
        CheckpointMutation::Dtype,
    ));
    assert_kind(dtype.verify(), ArtifactErrorKind::TensorDtypeMismatch);

    let mut shape = Fixture::new();
    shape.replace_checkpoint_and_refresh(serialize_checkpoint(
        &shape.lock.architecture,
        CheckpointMutation::Shape,
    ));
    assert_kind(shape.verify(), ArtifactErrorKind::TensorShapeMismatch);

    let mut offsets = Fixture::new();
    let checkpoint = swap_equal_tensor_offsets(serialize_checkpoint(
        &offsets.lock.architecture,
        CheckpointMutation::None,
    ));
    offsets.replace_checkpoint_and_refresh(checkpoint);
    assert_kind(offsets.verify(), ArtifactErrorKind::TensorOffsetMismatch);
}

#[test]
fn non_finite_f16_payload_is_rejected_after_structural_validation() {
    let mut fixture = Fixture::new();
    fixture.replace_checkpoint_and_refresh(serialize_checkpoint(
        &fixture.lock.architecture,
        CheckpointMutation::NonFinite,
    ));
    assert_kind(fixture.verify(), ArtifactErrorKind::NonFiniteTensor);
}

#[test]
fn checked_arithmetic_rejects_impossible_lock_dimensions() {
    let mut fixture = Fixture::new();
    fixture.lock.architecture.hidden_size = u64::MAX;
    fixture.lock.architecture.intermediate_size = 4;
    fixture.lock.architecture.num_attention_heads = 1;
    assert_kind(fixture.verify(), ArtifactErrorKind::ArithmeticOverflow);
}

#[test]
fn deterministic_errors_do_not_disclose_paths_or_parser_details() {
    let fixture = Fixture::new();
    let mut tokenizer = fs::read(fixture.path(LockedFile::Tokenizer)).expect("tokenizer");
    tokenizer[0] ^= 1;
    fs::write(fixture.path(LockedFile::Tokenizer), tokenizer).expect("corrupt tokenizer");
    let error = fixture.verify().expect_err("hash mismatch");
    assert_eq!(
        error.to_string(),
        "model artifact verification failed: hash_mismatch (tokenizer.json)"
    );
    assert!(
        !error
            .to_string()
            .contains(&fixture.directory.path.display().to_string())
    );
}

fn assert_kind(
    result: Result<super::VerifiedBundle, crate::ArtifactError>,
    kind: ArtifactErrorKind,
) {
    assert_eq!(result.expect_err("verification must fail").kind(), kind);
}

struct Fixture {
    directory: TestDirectory,
    lock: ModelLock,
}

impl Fixture {
    fn new() -> Self {
        let directory = TestDirectory::new();
        let architecture = fixture_architecture();
        let checkpoint = serialize_checkpoint(&architecture, CheckpointMutation::None);
        let config = fixture_config(&architecture);
        let files = BTreeMap::from([
            (LockedFile::Readme, b"fixture model card\n".to_vec()),
            (LockedFile::Config, config),
            (LockedFile::Checkpoint, checkpoint),
            (LockedFile::SpecialTokens, b"{}\n".to_vec()),
            (LockedFile::Tokenizer, b"{}\n".to_vec()),
            (LockedFile::TokenizerConfig, b"{}\n".to_vec()),
        ]);
        for (file, bytes) in &files {
            fs::write(directory.path.join(file.name()), bytes).expect("write fixture asset");
        }
        let checkpoint_bytes = files.get(&LockedFile::Checkpoint).expect("checkpoint");
        let header_bytes = u64::from_le_bytes(
            checkpoint_bytes[..8]
                .try_into()
                .expect("safetensors prefix"),
        );
        let header_end = 8 + header_bytes as usize;
        let schema = expected_tensors(&architecture).expect("fixture tensor schema");
        let element_count = schema.values().map(|tensor| tensor.elements).sum::<u64>();
        let lock = ModelLock {
            schema: "inferlab.public-model-lock.v1".to_owned(),
            source: SourceLock {
                repository: "EleutherAI/pythia-14m".to_owned(),
                revision: "cf967c0a9a04383db6f7b1108d86b2962634b4ac".to_owned(),
                license: "Apache-2.0".to_owned(),
            },
            files: LockedFile::ALL
                .into_iter()
                .map(|file| {
                    let bytes = &files[&file];
                    FileLock {
                        name: file.name().to_owned(),
                        bytes: bytes.len() as u64,
                        sha256: sha256_hex(bytes),
                    }
                })
                .collect(),
            checkpoint: CheckpointLock {
                file: "model.safetensors".to_owned(),
                format: "safetensors".to_owned(),
                header_bytes,
                header_sha256: sha256_hex(&checkpoint_bytes[8..header_end]),
                dtype: "F16".to_owned(),
                tensor_count: schema.len() as u64,
                element_count,
                data_bytes: element_count * 2,
            },
            architecture,
        };
        Self { directory, lock }
    }

    fn path(&self, file: LockedFile) -> PathBuf {
        self.directory.path.join(file.name())
    }

    fn verify(&self) -> Result<super::VerifiedBundle, crate::ArtifactError> {
        verify_bundle(&self.lock, &self.directory.path)
    }

    fn replace_and_refresh(&mut self, file: LockedFile, bytes: Vec<u8>) {
        fs::write(self.path(file), &bytes).expect("replace fixture file");
        let entry = self
            .lock
            .files
            .iter_mut()
            .find(|entry| entry.name == file.name())
            .expect("locked fixture file");
        entry.bytes = bytes.len() as u64;
        entry.sha256 = sha256_hex(&bytes);
    }

    fn replace_checkpoint_and_refresh(&mut self, bytes: Vec<u8>) {
        let header_bytes = u64::from_le_bytes(bytes[..8].try_into().expect("header prefix"));
        let header_end = 8 + header_bytes as usize;
        self.lock.checkpoint.header_bytes = header_bytes;
        self.lock.checkpoint.header_sha256 = sha256_hex(&bytes[8..header_end]);
        self.replace_and_refresh(LockedFile::Checkpoint, bytes);
    }
}

#[derive(Clone, Copy)]
enum CheckpointMutation {
    None,
    Dtype,
    Shape,
    NonFinite,
}

struct TensorStorage {
    name: String,
    dtype: Dtype,
    shape: Vec<usize>,
    data: Vec<u8>,
}

fn serialize_checkpoint(architecture: &ArchitectureLock, mutation: CheckpointMutation) -> Vec<u8> {
    let schema = expected_tensors(architecture).expect("fixture tensor schema");
    let changed_name = "gpt_neox.final_layer_norm.bias";
    let mut storage = schema
        .iter()
        .map(|(name, tensor)| {
            let dtype = if name == changed_name && matches!(mutation, CheckpointMutation::Dtype) {
                Dtype::BF16
            } else {
                Dtype::F16
            };
            let shape = if name == changed_name && matches!(mutation, CheckpointMutation::Shape) {
                vec![2, tensor.elements as usize / 2]
            } else {
                tensor.shape.clone()
            };
            let mut data = vec![0_u8; tensor.elements as usize * 2];
            for chunk in data.chunks_exact_mut(2) {
                chunk.copy_from_slice(&0x3c00_u16.to_le_bytes());
            }
            TensorStorage {
                name: name.clone(),
                dtype,
                shape,
                data,
            }
        })
        .collect::<Vec<_>>();
    if matches!(mutation, CheckpointMutation::NonFinite) {
        storage[0].data[..2].copy_from_slice(&0x7c00_u16.to_le_bytes());
    }
    let views = storage
        .iter()
        .map(|tensor| {
            (
                tensor.name.as_str(),
                TensorView::new(tensor.dtype, tensor.shape.clone(), &tensor.data)
                    .expect("valid fixture tensor"),
            )
        })
        .collect::<Vec<_>>();
    safetensors::serialize(views, None).expect("serialize fixture checkpoint")
}

fn swap_equal_tensor_offsets(checkpoint: Vec<u8>) -> Vec<u8> {
    let header_bytes = u64::from_le_bytes(checkpoint[..8].try_into().expect("header")) as usize;
    let data = checkpoint[8 + header_bytes..].to_vec();
    let mut header: serde_json::Value =
        serde_json::from_slice(&checkpoint[8..8 + header_bytes]).expect("header JSON");
    let first = "gpt_neox.final_layer_norm.bias";
    let second = "gpt_neox.final_layer_norm.weight";
    let first_offsets = header[first]["data_offsets"].clone();
    let second_offsets = header[second]["data_offsets"].clone();
    header[first]["data_offsets"] = second_offsets;
    header[second]["data_offsets"] = first_offsets;
    let mut encoded = serde_json::to_vec(&header).expect("header encoding");
    let padded = encoded.len().next_multiple_of(8);
    encoded.resize(padded, b' ');
    let mut rebuilt = (encoded.len() as u64).to_le_bytes().to_vec();
    rebuilt.extend_from_slice(&encoded);
    rebuilt.extend_from_slice(&data);
    rebuilt
}

fn rename_tensor_in_header(mut checkpoint: Vec<u8>) -> Vec<u8> {
    let header_bytes = u64::from_le_bytes(checkpoint[..8].try_into().expect("header")) as usize;
    let header = &mut checkpoint[8..8 + header_bytes];
    let original = b"gpt_neox.final_layer_norm.bias";
    let replacement = b"gpt_neox.final_layer_norm.biaz";
    let offset = header
        .windows(original.len())
        .position(|window| window == original)
        .expect("fixture tensor name");
    header[offset..offset + original.len()].copy_from_slice(replacement);
    checkpoint
}

fn fixture_architecture() -> ArchitectureLock {
    ArchitectureLock {
        model_type: "gpt_neox".to_owned(),
        architecture: "GPTNeoXForCausalLM".to_owned(),
        vocab_size: 8,
        max_position_embeddings: 16,
        hidden_size: 4,
        intermediate_size: 16,
        num_attention_heads: 1,
        num_hidden_layers: 1,
        bos_token_id: 0,
        eos_token_id: 0,
        hidden_act: "gelu".to_owned(),
        layer_norm_eps: 0.00001,
        rotary_pct: 0.5,
        rotary_emb_base: 10_000,
        attention_bias: true,
        tie_word_embeddings: false,
        use_parallel_residual: true,
        torch_dtype: "float16".to_owned(),
    }
}

fn fixture_config(architecture: &ArchitectureLock) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "architectures": [architecture.architecture],
        "attention_bias": architecture.attention_bias,
        "attention_dropout": 0.0,
        "bos_token_id": architecture.bos_token_id,
        "classifier_dropout": 0.1,
        "eos_token_id": architecture.eos_token_id,
        "hidden_act": architecture.hidden_act,
        "hidden_dropout": 0.0,
        "hidden_size": architecture.hidden_size,
        "initializer_range": 0.02,
        "intermediate_size": architecture.intermediate_size,
        "layer_norm_eps": architecture.layer_norm_eps,
        "max_position_embeddings": architecture.max_position_embeddings,
        "model_type": architecture.model_type,
        "num_attention_heads": architecture.num_attention_heads,
        "num_hidden_layers": architecture.num_hidden_layers,
        "rope_scaling": null,
        "rotary_emb_base": architecture.rotary_emb_base,
        "rotary_pct": architecture.rotary_pct,
        "tie_word_embeddings": architecture.tie_word_embeddings,
        "torch_dtype": architecture.torch_dtype,
        "transformers_version": "4.40.0",
        "use_cache": true,
        "use_parallel_residual": architecture.use_parallel_residual,
        "vocab_size": architecture.vocab_size
    }))
    .expect("fixture config JSON")
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "inferlab-model-artifacts-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create fixture directory");
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).expect("remove fixture directory");
    }
}

#[cfg(unix)]
fn make_fifo(path: &Path) {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let path = CString::new(path.as_os_str().as_bytes()).expect("FIFO path has no NUL");
    // SAFETY: path is a live NUL-terminated string and mode contains only
    // ordinary permission bits.
    let status = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
    assert_eq!(status, 0, "create FIFO");
}
