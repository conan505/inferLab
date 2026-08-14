use std::{collections::BTreeMap, fmt, sync::OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokenizers::Tokenizer;

use crate::{LockedFile, VerifiedBundle};

const TOKENIZER_REPORT_SCHEMA: &str = "inferlab.production-tokenizer.v1";
const SERIALIZATION_VERSION: &str = "1.0";
const BASE_VOCABULARY_SIZE: usize = 50_254;
const MERGE_COUNT: usize = 50_009;
const ADDED_TOKEN_ENTRIES: usize = 25;
const DEFINED_TOKEN_IDS: u32 = 50_277;
const MODEL_ROWS: u32 = 50_304;
const MAX_CONTEXT_TOKENS: usize = 2_048;
const END_OF_TEXT_ID: u32 = 0;
const PADDING_SPECIAL_ID: u32 = 1;
const END_OF_TEXT: &str = "<|endoftext|>";
const PADDING_SPECIAL: &str = "<|padding|>";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiteralSpecialMode {
    RecognizeConfigured,
    EncodeAsText,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EncodeOptions {
    pub literal_specials: LiteralSpecialMode,
    pub add_special_tokens: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodeSpecialMode {
    PreserveConfigured,
    SkipConfigured,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenizerErrorKind {
    TokenizerJsonInvalid,
    TokenizerConfigInvalid,
    SpecialTokensInvalid,
    PipelineMismatch,
    VocabularyMismatch,
    ConstructionFailed,
    EncodeFailed,
    DecodeFailed,
    ContextLengthExceeded,
    AlignmentOnlyModelRow,
    TokenIdOutOfRange,
    UndefinedTokenId,
    InvalidUtf8TokenSequence,
    DecoderMismatch,
    SpecialPolicyViolation,
}

impl TokenizerErrorKind {
    pub const fn code(self) -> &'static str {
        match self {
            Self::TokenizerJsonInvalid => "tokenizer_json_invalid",
            Self::TokenizerConfigInvalid => "tokenizer_config_invalid",
            Self::SpecialTokensInvalid => "special_tokens_invalid",
            Self::PipelineMismatch => "pipeline_mismatch",
            Self::VocabularyMismatch => "vocabulary_mismatch",
            Self::ConstructionFailed => "construction_failed",
            Self::EncodeFailed => "encode_failed",
            Self::DecodeFailed => "decode_failed",
            Self::ContextLengthExceeded => "context_length_exceeded",
            Self::AlignmentOnlyModelRow => "alignment_only_model_row",
            Self::TokenIdOutOfRange => "token_id_out_of_range",
            Self::UndefinedTokenId => "undefined_token_id",
            Self::InvalidUtf8TokenSequence => "invalid_utf8_token_sequence",
            Self::DecoderMismatch => "decoder_mismatch",
            Self::SpecialPolicyViolation => "special_policy_violation",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenizerError {
    kind: TokenizerErrorKind,
}

impl TokenizerError {
    const fn new(kind: TokenizerErrorKind) -> Self {
        Self { kind }
    }

    pub const fn kind(self) -> TokenizerErrorKind {
        self.kind
    }
}

impl fmt::Display for TokenizerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "production tokenizer failed: {}",
            self.kind.code()
        )
    }
}

impl std::error::Error for TokenizerError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TokenizerReport {
    pub schema: &'static str,
    pub serialization_version: String,
    pub normalizer: String,
    pub pre_tokenizer: String,
    pub decoder: String,
    pub model: String,
    pub base_vocabulary_entries: u32,
    pub merge_entries: u32,
    pub added_token_entries: u32,
    pub defined_token_ids: u32,
    pub defined_token_id_max: u32,
    pub model_rows: u32,
    pub alignment_only_model_rows: u32,
    pub alignment_only_model_row_start: u32,
    pub alignment_only_model_row_end: u32,
    pub max_context_tokens: u32,
    pub add_prefix_space: bool,
    pub trim_offsets: bool,
    pub use_regex: bool,
    pub truncation_enabled: bool,
    pub padding_enabled: bool,
    pub pad_token_configured: bool,
    pub post_processor_special_token_entries: u32,
    pub upstream_clean_up_tokenization_spaces: bool,
    pub runtime_cleanup_applied: bool,
}

#[derive(Clone)]
pub struct ProductionTokenizer {
    recognize_configured: Tokenizer,
    encode_as_text: Tokenizer,
    report: TokenizerReport,
}

impl fmt::Debug for ProductionTokenizer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionTokenizer")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl VerifiedBundle {
    /// Construct the production tokenizer exclusively from authenticated bytes.
    pub fn production_tokenizer(&self) -> Result<ProductionTokenizer, TokenizerError> {
        if self.report().architecture.vocab_size != u64::from(MODEL_ROWS)
            || self.report().architecture.max_position_embeddings
                != u64::try_from(MAX_CONTEXT_TOKENS)
                    .map_err(|_| TokenizerError::new(TokenizerErrorKind::PipelineMismatch))?
        {
            return Err(TokenizerError::new(TokenizerErrorKind::PipelineMismatch));
        }
        ProductionTokenizer::from_verified_bytes(
            self.bytes(LockedFile::Tokenizer),
            self.bytes(LockedFile::TokenizerConfig),
            self.bytes(LockedFile::SpecialTokens),
        )
    }
}

impl ProductionTokenizer {
    fn from_verified_bytes(
        tokenizer_bytes: &[u8],
        tokenizer_config_bytes: &[u8],
        special_tokens_bytes: &[u8],
    ) -> Result<Self, TokenizerError> {
        let document: TokenizerDocument = serde_json::from_slice(tokenizer_bytes)
            .map_err(|_| TokenizerError::new(TokenizerErrorKind::TokenizerJsonInvalid))?;
        validate_tokenizer_document(&document)?;
        let tokenizer_config: TokenizerConfigDocument =
            serde_json::from_slice(tokenizer_config_bytes)
                .map_err(|_| TokenizerError::new(TokenizerErrorKind::TokenizerConfigInvalid))?;
        validate_tokenizer_config(&tokenizer_config, &document.added_tokens)?;
        let special_tokens: SpecialTokensDocument = serde_json::from_slice(special_tokens_bytes)
            .map_err(|_| TokenizerError::new(TokenizerErrorKind::SpecialTokensInvalid))?;
        validate_special_tokens(&special_tokens)?;

        let mut recognize_configured = Tokenizer::from_bytes(tokenizer_bytes)
            .map_err(|_| TokenizerError::new(TokenizerErrorKind::ConstructionFailed))?;
        if recognize_configured.get_truncation().is_some()
            || recognize_configured.get_padding().is_some()
            || recognize_configured.get_encode_special_tokens()
        {
            return Err(TokenizerError::new(TokenizerErrorKind::PipelineMismatch));
        }
        recognize_configured.set_encode_special_tokens(false);
        validate_runtime_vocabulary(&recognize_configured)?;

        let mut encode_as_text = recognize_configured.clone();
        encode_as_text.set_encode_special_tokens(true);
        validate_special_modes(&recognize_configured, &encode_as_text)?;

        Ok(Self {
            recognize_configured,
            encode_as_text,
            report: canonical_report(),
        })
    }

    pub fn report(&self) -> &TokenizerReport {
        &self.report
    }

    /// Encode the complete input, then enforce the model context boundary.
    pub fn encode(&self, input: &str, options: EncodeOptions) -> Result<Vec<u32>, TokenizerError> {
        let tokenizer = match options.literal_specials {
            LiteralSpecialMode::RecognizeConfigured => &self.recognize_configured,
            LiteralSpecialMode::EncodeAsText => &self.encode_as_text,
        };
        let encoding = tokenizer
            .encode(input, options.add_special_tokens)
            .map_err(|_| TokenizerError::new(TokenizerErrorKind::EncodeFailed))?;
        let ids = encoding.get_ids().to_vec();
        if matches!(options.literal_specials, LiteralSpecialMode::EncodeAsText)
            && ids
                .iter()
                .any(|id| *id == END_OF_TEXT_ID || *id == PADDING_SPECIAL_ID)
        {
            return Err(TokenizerError::new(
                TokenizerErrorKind::SpecialPolicyViolation,
            ));
        }
        if ids.iter().any(|id| *id >= DEFINED_TOKEN_IDS) {
            return Err(TokenizerError::new(TokenizerErrorKind::VocabularyMismatch));
        }
        if ids.len() > MAX_CONTEXT_TOKENS {
            return Err(TokenizerError::new(
                TokenizerErrorKind::ContextLengthExceeded,
            ));
        }
        Ok(ids)
    }

    pub fn decode(
        &self,
        ids: &[u32],
        configured_specials: DecodeSpecialMode,
    ) -> Result<String, TokenizerError> {
        for id in ids {
            validate_decode_id(*id)?;
        }
        if ids.len() > MAX_CONTEXT_TOKENS {
            return Err(TokenizerError::new(
                TokenizerErrorKind::ContextLengthExceeded,
            ));
        }

        let skip_specials = matches!(configured_specials, DecodeSpecialMode::SkipConfigured);
        let mut tokens = Vec::with_capacity(ids.len());
        for id in ids {
            if skip_specials && (*id == END_OF_TEXT_ID || *id == PADDING_SPECIAL_ID) {
                continue;
            }
            tokens.push(
                self.recognize_configured
                    .id_to_token(*id)
                    .ok_or_else(|| TokenizerError::new(TokenizerErrorKind::UndefinedTokenId))?,
            );
        }
        let strict = strict_byte_level_decode(&tokens)?;
        let maintained = self
            .recognize_configured
            .decode(ids, skip_specials)
            .map_err(|_| TokenizerError::new(TokenizerErrorKind::DecodeFailed))?;
        if maintained != strict {
            return Err(TokenizerError::new(TokenizerErrorKind::DecoderMismatch));
        }
        Ok(strict)
    }
}

fn canonical_report() -> TokenizerReport {
    TokenizerReport {
        schema: TOKENIZER_REPORT_SCHEMA,
        serialization_version: SERIALIZATION_VERSION.to_owned(),
        normalizer: "NFC".to_owned(),
        pre_tokenizer: "ByteLevel".to_owned(),
        decoder: "ByteLevel".to_owned(),
        model: "BPE".to_owned(),
        base_vocabulary_entries: BASE_VOCABULARY_SIZE as u32,
        merge_entries: MERGE_COUNT as u32,
        added_token_entries: ADDED_TOKEN_ENTRIES as u32,
        defined_token_ids: DEFINED_TOKEN_IDS,
        defined_token_id_max: DEFINED_TOKEN_IDS - 1,
        model_rows: MODEL_ROWS,
        alignment_only_model_rows: MODEL_ROWS - DEFINED_TOKEN_IDS,
        alignment_only_model_row_start: DEFINED_TOKEN_IDS,
        alignment_only_model_row_end: MODEL_ROWS - 1,
        max_context_tokens: MAX_CONTEXT_TOKENS as u32,
        add_prefix_space: false,
        trim_offsets: true,
        use_regex: true,
        truncation_enabled: false,
        padding_enabled: false,
        pad_token_configured: false,
        post_processor_special_token_entries: 0,
        upstream_clean_up_tokenization_spaces: true,
        runtime_cleanup_applied: false,
    }
}

fn validate_decode_id(id: u32) -> Result<(), TokenizerError> {
    if id < DEFINED_TOKEN_IDS {
        Ok(())
    } else if id < MODEL_ROWS {
        Err(TokenizerError::new(
            TokenizerErrorKind::AlignmentOnlyModelRow,
        ))
    } else {
        Err(TokenizerError::new(TokenizerErrorKind::TokenIdOutOfRange))
    }
}

fn strict_byte_level_decode(tokens: &[String]) -> Result<String, TokenizerError> {
    let decoder = official_byte_decoder();
    let mut bytes = Vec::new();
    for token in tokens {
        let mapped = token
            .chars()
            .map(|character| decoder.get(&character).copied())
            .collect::<Option<Vec<_>>>();
        if let Some(mapped) = mapped {
            bytes.extend(mapped);
        } else {
            bytes.extend_from_slice(token.as_bytes());
        }
    }
    String::from_utf8(bytes)
        .map_err(|_| TokenizerError::new(TokenizerErrorKind::InvalidUtf8TokenSequence))
}

fn official_byte_decoder() -> &'static BTreeMap<char, u8> {
    static DECODER: OnceLock<BTreeMap<char, u8>> = OnceLock::new();
    DECODER.get_or_init(|| {
        let mut bytes = (b'!'..=b'~')
            .chain(b'\xA1'..=b'\xAC')
            .chain(b'\xAE'..=b'\xFF')
            .collect::<Vec<_>>();
        let mut codepoints = bytes
            .iter()
            .map(|byte| u32::from(*byte))
            .collect::<Vec<_>>();
        let mut extra = 0_u32;
        for byte in 0_u8..=u8::MAX {
            if !bytes.contains(&byte) {
                bytes.push(byte);
                codepoints.push(256 + extra);
                extra += 1;
            }
        }
        bytes
            .into_iter()
            .zip(codepoints)
            .map(|(byte, codepoint)| {
                (
                    char::from_u32(codepoint).expect("official byte mapping is valid Unicode"),
                    byte,
                )
            })
            .collect()
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenizerDocument {
    version: String,
    truncation: Option<Value>,
    padding: Option<Value>,
    added_tokens: Vec<AddedTokenDocument>,
    normalizer: Value,
    pre_tokenizer: Value,
    post_processor: Value,
    decoder: Value,
    model: BpeDocument,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct AddedTokenDocument {
    id: u32,
    content: String,
    single_word: bool,
    lstrip: bool,
    rstrip: bool,
    normalized: bool,
    special: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BpeDocument {
    #[serde(rename = "type")]
    kind: String,
    dropout: Option<f64>,
    unk_token: Option<String>,
    continuing_subword_prefix: Option<String>,
    end_of_word_suffix: Option<String>,
    fuse_unk: bool,
    byte_fallback: bool,
    ignore_merges: bool,
    vocab: BTreeMap<String, u32>,
    merges: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenizerConfigDocument {
    add_bos_token: bool,
    add_eos_token: bool,
    add_prefix_space: bool,
    added_tokens_decoder: BTreeMap<String, ConfigAddedToken>,
    bos_token: String,
    clean_up_tokenization_spaces: bool,
    eos_token: String,
    model_max_length: Value,
    pad_token: Option<String>,
    tokenizer_class: String,
    unk_token: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ConfigAddedToken {
    content: String,
    lstrip: bool,
    normalized: bool,
    rstrip: bool,
    single_word: bool,
    special: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpecialTokensDocument {
    bos_token: SpecialTokenDocument,
    eos_token: SpecialTokenDocument,
    unk_token: SpecialTokenDocument,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct SpecialTokenDocument {
    content: String,
    lstrip: bool,
    normalized: bool,
    rstrip: bool,
    single_word: bool,
}

fn validate_tokenizer_document(document: &TokenizerDocument) -> Result<(), TokenizerError> {
    let byte_level = json!({
        "type": "ByteLevel",
        "add_prefix_space": false,
        "trim_offsets": true,
        "use_regex": true
    });
    let post_processor = json!({
        "type": "TemplateProcessing",
        "single": [{"Sequence": {"id": "A", "type_id": 0}}],
        "pair": [
            {"Sequence": {"id": "A", "type_id": 0}},
            {"Sequence": {"id": "B", "type_id": 1}}
        ],
        "special_tokens": {}
    });
    if document.version != SERIALIZATION_VERSION
        || document.truncation.is_some()
        || document.padding.is_some()
        || document.normalizer != json!({"type": "NFC"})
        || document.pre_tokenizer != byte_level
        || document.decoder != byte_level
        || document.post_processor != post_processor
        || document.model.kind != "BPE"
        || document.model.dropout.is_some()
        || document.model.unk_token.is_some()
        || document.model.continuing_subword_prefix.is_some()
        || document.model.end_of_word_suffix.is_some()
        || document.model.fuse_unk
        || document.model.byte_fallback
        || document.model.ignore_merges
    {
        return Err(TokenizerError::new(TokenizerErrorKind::PipelineMismatch));
    }
    validate_vocabulary_document(&document.model)?;
    validate_added_tokens(&document.added_tokens)
}

fn validate_vocabulary_document(model: &BpeDocument) -> Result<(), TokenizerError> {
    if model.vocab.len() != BASE_VOCABULARY_SIZE || model.merges.len() != MERGE_COUNT {
        return Err(TokenizerError::new(TokenizerErrorKind::VocabularyMismatch));
    }
    let mut ids = vec![false; BASE_VOCABULARY_SIZE];
    for id in model.vocab.values() {
        let index = usize::try_from(*id)
            .map_err(|_| TokenizerError::new(TokenizerErrorKind::VocabularyMismatch))?;
        let seen = ids
            .get_mut(index)
            .ok_or_else(|| TokenizerError::new(TokenizerErrorKind::VocabularyMismatch))?;
        if *seen {
            return Err(TokenizerError::new(TokenizerErrorKind::VocabularyMismatch));
        }
        *seen = true;
    }
    if ids.iter().any(|seen| !seen)
        || model.vocab.get(END_OF_TEXT) != Some(&END_OF_TEXT_ID)
        || model.vocab.get(PADDING_SPECIAL) != Some(&PADDING_SPECIAL_ID)
        || model.merges.iter().any(|merge| merge.is_empty())
    {
        return Err(TokenizerError::new(TokenizerErrorKind::VocabularyMismatch));
    }
    Ok(())
}

fn validate_added_tokens(tokens: &[AddedTokenDocument]) -> Result<(), TokenizerError> {
    if tokens.len() != ADDED_TOKEN_ENTRIES {
        return Err(TokenizerError::new(TokenizerErrorKind::VocabularyMismatch));
    }
    for (index, token) in tokens.iter().enumerate() {
        let expected = if index == 0 {
            AddedTokenDocument {
                id: END_OF_TEXT_ID,
                content: END_OF_TEXT.to_owned(),
                single_word: false,
                lstrip: false,
                rstrip: false,
                normalized: false,
                special: true,
            }
        } else if index == 1 {
            AddedTokenDocument {
                id: PADDING_SPECIAL_ID,
                content: PADDING_SPECIAL.to_owned(),
                single_word: false,
                lstrip: false,
                rstrip: false,
                normalized: false,
                special: true,
            }
        } else {
            let offset = u32::try_from(index - 2)
                .map_err(|_| TokenizerError::new(TokenizerErrorKind::VocabularyMismatch))?;
            AddedTokenDocument {
                id: BASE_VOCABULARY_SIZE as u32 + offset,
                content: " ".repeat(26 - index),
                single_word: false,
                lstrip: false,
                rstrip: false,
                normalized: true,
                special: false,
            }
        };
        if token != &expected {
            return Err(TokenizerError::new(TokenizerErrorKind::VocabularyMismatch));
        }
    }
    Ok(())
}

fn validate_tokenizer_config(
    config: &TokenizerConfigDocument,
    added_tokens: &[AddedTokenDocument],
) -> Result<(), TokenizerError> {
    let model_max_length_matches = config
        .model_max_length
        .as_f64()
        .is_some_and(|value| value.is_finite() && value == 1.0e30);
    if !model_max_length_matches
        || config.add_bos_token
        || config.add_eos_token
        || config.add_prefix_space
        || !config.clean_up_tokenization_spaces
        || config.pad_token.is_some()
        || config.bos_token != END_OF_TEXT
        || config.eos_token != END_OF_TEXT
        || config.unk_token != END_OF_TEXT
        || config.tokenizer_class != "GPTNeoXTokenizer"
        || config.added_tokens_decoder.len() != ADDED_TOKEN_ENTRIES
    {
        return Err(TokenizerError::new(
            TokenizerErrorKind::TokenizerConfigInvalid,
        ));
    }
    for token in added_tokens {
        let configured = config
            .added_tokens_decoder
            .get(&token.id.to_string())
            .ok_or_else(|| TokenizerError::new(TokenizerErrorKind::TokenizerConfigInvalid))?;
        let expected = ConfigAddedToken {
            content: token.content.clone(),
            lstrip: token.lstrip,
            normalized: token.normalized,
            rstrip: token.rstrip,
            single_word: token.single_word,
            special: token.special,
        };
        if configured != &expected {
            return Err(TokenizerError::new(
                TokenizerErrorKind::TokenizerConfigInvalid,
            ));
        }
    }
    Ok(())
}

fn validate_special_tokens(tokens: &SpecialTokensDocument) -> Result<(), TokenizerError> {
    for token in [&tokens.bos_token, &tokens.eos_token, &tokens.unk_token] {
        if token.content != END_OF_TEXT
            || token.lstrip
            || token.normalized
            || token.rstrip
            || token.single_word
        {
            return Err(TokenizerError::new(
                TokenizerErrorKind::SpecialTokensInvalid,
            ));
        }
    }
    Ok(())
}

fn validate_runtime_vocabulary(tokenizer: &Tokenizer) -> Result<(), TokenizerError> {
    if tokenizer.get_vocab_size(false) != BASE_VOCABULARY_SIZE
        || tokenizer.get_vocab_size(true) != DEFINED_TOKEN_IDS as usize
        || tokenizer.get_added_tokens_decoder().len() != ADDED_TOKEN_ENTRIES
    {
        return Err(TokenizerError::new(TokenizerErrorKind::VocabularyMismatch));
    }
    for id in 0..DEFINED_TOKEN_IDS {
        if tokenizer.id_to_token(id).is_none() {
            return Err(TokenizerError::new(TokenizerErrorKind::UndefinedTokenId));
        }
    }
    for id in DEFINED_TOKEN_IDS..MODEL_ROWS {
        if tokenizer.id_to_token(id).is_some() {
            return Err(TokenizerError::new(TokenizerErrorKind::VocabularyMismatch));
        }
    }
    Ok(())
}

fn validate_special_modes(
    recognize_configured: &Tokenizer,
    encode_as_text: &Tokenizer,
) -> Result<(), TokenizerError> {
    let recognized = recognize_configured
        .encode(END_OF_TEXT, false)
        .map_err(|_| TokenizerError::new(TokenizerErrorKind::ConstructionFailed))?;
    let textual = encode_as_text
        .encode(END_OF_TEXT, false)
        .map_err(|_| TokenizerError::new(TokenizerErrorKind::ConstructionFailed))?;
    let recognized_padding = recognize_configured
        .encode(PADDING_SPECIAL, false)
        .map_err(|_| TokenizerError::new(TokenizerErrorKind::ConstructionFailed))?;
    let textual_padding = encode_as_text
        .encode(PADDING_SPECIAL, false)
        .map_err(|_| TokenizerError::new(TokenizerErrorKind::ConstructionFailed))?;
    if recognized.get_ids() != [END_OF_TEXT_ID]
        || textual.get_ids().contains(&END_OF_TEXT_ID)
        || textual.get_ids().is_empty()
        || recognized_padding.get_ids() != [PADDING_SPECIAL_ID]
        || textual_padding.get_ids().contains(&PADDING_SPECIAL_ID)
        || textual_padding.get_ids().is_empty()
    {
        return Err(TokenizerError::new(
            TokenizerErrorKind::SpecialPolicyViolation,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
