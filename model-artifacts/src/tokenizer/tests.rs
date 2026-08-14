use std::{collections::BTreeMap, fs, path::Path, sync::Arc, thread};

use tokenizers::{
    AddedToken, Tokenizer,
    models::bpe::{BPE, Vocab},
    normalizers::unicode::NFC,
    pre_tokenizers::byte_level::ByteLevel,
};

use super::{
    ADDED_TOKEN_ENTRIES, AddedTokenDocument, BASE_VOCABULARY_SIZE, BpeDocument, ConfigAddedToken,
    DEFINED_TOKEN_IDS, DecodeSpecialMode, END_OF_TEXT, END_OF_TEXT_ID, EncodeOptions,
    LiteralSpecialMode, MAX_CONTEXT_TOKENS, MERGE_COUNT, MODEL_ROWS, PADDING_SPECIAL,
    PADDING_SPECIAL_ID, ProductionTokenizer, SpecialTokenDocument, SpecialTokensDocument,
    TokenizerConfigDocument, TokenizerDocument, TokenizerErrorKind, canonical_report,
    official_byte_decoder, strict_byte_level_decode, validate_added_tokens,
    validate_special_tokens, validate_tokenizer_config, validate_tokenizer_document,
};

#[test]
fn exact_document_contract_accepts_only_the_pinned_pipeline_and_domains() {
    let mut document = synthetic_document();
    validate_tokenizer_document(&document).expect("exact structural contract");

    document.version = "2.0".to_owned();
    assert_error(
        validate_tokenizer_document(&document),
        TokenizerErrorKind::PipelineMismatch,
    );
    document = synthetic_document();
    document.pre_tokenizer["use_regex"] = serde_json::json!(false);
    assert_error(
        validate_tokenizer_document(&document),
        TokenizerErrorKind::PipelineMismatch,
    );
    document = synthetic_document();
    document.model.vocab.remove("token-2");
    assert_error(
        validate_tokenizer_document(&document),
        TokenizerErrorKind::VocabularyMismatch,
    );
    document = synthetic_document();
    document.added_tokens[2].normalized = false;
    assert_error(
        validate_added_tokens(&document.added_tokens),
        TokenizerErrorKind::VocabularyMismatch,
    );
}

#[test]
fn tokenizer_configuration_and_special_map_are_exact() {
    let added = synthetic_added_tokens();
    let mut config = synthetic_config(&added);
    validate_tokenizer_config(&config, &added).expect("exact tokenizer config");
    config.pad_token = Some(PADDING_SPECIAL.to_owned());
    assert_error(
        validate_tokenizer_config(&config, &added),
        TokenizerErrorKind::TokenizerConfigInvalid,
    );

    config = synthetic_config(&added);
    config.clean_up_tokenization_spaces = false;
    assert_error(
        validate_tokenizer_config(&config, &added),
        TokenizerErrorKind::TokenizerConfigInvalid,
    );

    let mut special = synthetic_special_tokens();
    validate_special_tokens(&special).expect("exact special map");
    special.eos_token.content = "drift".to_owned();
    assert_error(
        validate_special_tokens(&special),
        TokenizerErrorKind::SpecialTokensInvalid,
    );
}

#[test]
fn strict_byte_decoder_rejects_lossy_sequences_and_preserves_valid_replacement_text() {
    assert_error(
        strict_byte_level_decode(&["Ã".to_owned()]),
        TokenizerErrorKind::InvalidUtf8TokenSequence,
    );
    assert_eq!(
        strict_byte_level_decode(&["Ã".to_owned(), "©".to_owned()]).expect("cross-token UTF-8"),
        "é"
    );
    assert_eq!(
        strict_byte_level_decode(&["ï".to_owned(), "¿".to_owned(), "½".to_owned()])
            .expect("literal replacement character bytes"),
        "\u{fffd}"
    );
    assert_eq!(official_byte_decoder().len(), 256);
}

#[test]
fn encode_modes_are_explicit_separate_and_context_bounded_after_encoding() {
    let tokenizer = synthetic_runtime();
    let recognize = EncodeOptions {
        literal_specials: LiteralSpecialMode::RecognizeConfigured,
        add_special_tokens: false,
    };
    let as_text = EncodeOptions {
        literal_specials: LiteralSpecialMode::EncodeAsText,
        add_special_tokens: false,
    };
    assert_eq!(
        tokenizer
            .encode(END_OF_TEXT, recognize)
            .expect("recognize configured special"),
        [END_OF_TEXT_ID]
    );
    let textual = tokenizer
        .encode(END_OF_TEXT, as_text)
        .expect("encode special literal as text");
    assert!(!textual.is_empty());
    assert!(!textual.contains(&END_OF_TEXT_ID));
    assert_eq!(
        tokenizer
            .encode(PADDING_SPECIAL, recognize)
            .expect("recognize configured padding literal"),
        [PADDING_SPECIAL_ID]
    );
    let textual_padding = tokenizer
        .encode(PADDING_SPECIAL, as_text)
        .expect("encode padding literal as text");
    assert!(!textual_padding.is_empty());
    assert!(!textual_padding.contains(&PADDING_SPECIAL_ID));

    let with_postprocessor = tokenizer
        .encode(
            "ordinary",
            EncodeOptions {
                literal_specials: LiteralSpecialMode::RecognizeConfigured,
                add_special_tokens: true,
            },
        )
        .expect("explicit postprocessor mode");
    let without_postprocessor = tokenizer
        .encode("ordinary", recognize)
        .expect("explicit no-insertion mode");
    assert_eq!(with_postprocessor, without_postprocessor);

    assert_eq!(
        tokenizer
            .encode(&"a".repeat(MAX_CONTEXT_TOKENS), recognize)
            .expect("2048 tokens")
            .len(),
        MAX_CONTEXT_TOKENS
    );
    assert_error(
        tokenizer.encode(&"a".repeat(MAX_CONTEXT_TOKENS + 1), recognize),
        TokenizerErrorKind::ContextLengthExceeded,
    );
}

#[test]
fn decode_prevalidates_domains_and_never_returns_lossy_text() {
    let tokenizer = synthetic_runtime();
    assert_error(
        tokenizer.decode(&[127], DecodeSpecialMode::PreserveConfigured),
        TokenizerErrorKind::InvalidUtf8TokenSequence,
    );
    assert_eq!(
        tokenizer
            .decode(&[127, 104], DecodeSpecialMode::PreserveConfigured)
            .expect("cross-token UTF-8"),
        "é"
    );
    assert_eq!(
        tokenizer
            .decode(&[END_OF_TEXT_ID], DecodeSpecialMode::PreserveConfigured)
            .expect("preserve special"),
        END_OF_TEXT
    );
    assert_eq!(
        tokenizer
            .decode(&[END_OF_TEXT_ID], DecodeSpecialMode::SkipConfigured)
            .expect("skip special"),
        ""
    );
    assert_error(
        tokenizer.decode(&[DEFINED_TOKEN_IDS], DecodeSpecialMode::PreserveConfigured),
        TokenizerErrorKind::AlignmentOnlyModelRow,
    );
    assert_error(
        tokenizer.decode(&[MODEL_ROWS], DecodeSpecialMode::PreserveConfigured),
        TokenizerErrorKind::TokenIdOutOfRange,
    );
    assert_error(
        tokenizer.decode(
            &vec![2; MAX_CONTEXT_TOKENS + 1],
            DecodeSpecialMode::PreserveConfigured,
        ),
        TokenizerErrorKind::ContextLengthExceeded,
    );

    let replacement_ids = tokenizer
        .encode(
            "\u{fffd}",
            EncodeOptions {
                literal_specials: LiteralSpecialMode::EncodeAsText,
                add_special_tokens: false,
            },
        )
        .expect("encode replacement character");
    assert_eq!(
        tokenizer
            .decode(&replacement_ids, DecodeSpecialMode::PreserveConfigured)
            .expect("literal replacement character remains valid"),
        "\u{fffd}"
    );
}

#[test]
fn runtime_is_repeatable_send_sync_and_concurrent() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ProductionTokenizer>();

    let tokenizer = Arc::new(synthetic_runtime());
    let expected = tokenizer
        .encode(
            "concurrent é",
            EncodeOptions {
                literal_specials: LiteralSpecialMode::EncodeAsText,
                add_special_tokens: false,
            },
        )
        .expect("baseline encode");
    let workers = (0..8)
        .map(|_| {
            let tokenizer = Arc::clone(&tokenizer);
            let expected = expected.clone();
            thread::spawn(move || {
                for _ in 0..32 {
                    let ids = tokenizer
                        .encode(
                            "concurrent é",
                            EncodeOptions {
                                literal_specials: LiteralSpecialMode::EncodeAsText,
                                add_special_tokens: false,
                            },
                        )
                        .expect("concurrent encode");
                    assert_eq!(ids, expected);
                    assert_eq!(
                        tokenizer
                            .decode(&ids, DecodeSpecialMode::PreserveConfigured)
                            .expect("concurrent decode"),
                        "concurrent é"
                    );
                }
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().expect("worker completes");
    }
}

#[test]
fn reports_defined_ids_without_claiming_encoder_reachability_or_padding() {
    let report = canonical_report();
    assert_eq!(report.defined_token_ids, 50_277);
    assert_eq!(report.defined_token_id_max, 50_276);
    assert_eq!(report.model_rows, 50_304);
    assert_eq!(report.alignment_only_model_rows, 27);
    assert_eq!(report.alignment_only_model_row_start, 50_277);
    assert_eq!(report.alignment_only_model_row_end, 50_303);
    assert!(!report.pad_token_configured);
    assert!(report.upstream_clean_up_tokenization_spaces);
    assert!(!report.runtime_cleanup_applied);
    assert_eq!(report.post_processor_special_token_entries, 0);
}

#[test]
fn official_verified_tokenizer_can_be_exercised_without_checkpoint_weights() {
    let Some(directory) = std::env::var_os("INFERLAB_TOKENIZER_TEST_ASSETS") else {
        return;
    };
    let directory = Path::new(&directory);
    let tokenizer = ProductionTokenizer::from_verified_bytes(
        &fs::read(directory.join("tokenizer.json")).expect("tokenizer.json"),
        &fs::read(directory.join("tokenizer_config.json")).expect("tokenizer_config.json"),
        &fs::read(directory.join("special_tokens_map.json")).expect("special_tokens_map.json"),
    )
    .expect("official tokenizer contract");
    assert_eq!(
        tokenizer
            .encode(
                END_OF_TEXT,
                EncodeOptions {
                    literal_specials: LiteralSpecialMode::RecognizeConfigured,
                    add_special_tokens: false,
                }
            )
            .expect("official configured special"),
        [END_OF_TEXT_ID]
    );
    assert_error(
        tokenizer.decode(&[127], DecodeSpecialMode::PreserveConfigured),
        TokenizerErrorKind::InvalidUtf8TokenSequence,
    );
    assert_eq!(
        tokenizer
            .decode(&[127, 104], DecodeSpecialMode::PreserveConfigured)
            .expect("official cross-token UTF-8"),
        "é"
    );
    let ordinary = EncodeOptions {
        literal_specials: LiteralSpecialMode::EncodeAsText,
        add_special_tokens: false,
    };
    let boundary = tokenizer
        .encode(&" a".repeat(MAX_CONTEXT_TOKENS), ordinary)
        .expect("official 2048-token boundary");
    assert_eq!(boundary, vec![247; MAX_CONTEXT_TOKENS]);
    assert_error(
        tokenizer.encode(&" a".repeat(MAX_CONTEXT_TOKENS + 1), ordinary),
        TokenizerErrorKind::ContextLengthExceeded,
    );
    let replacement = tokenizer
        .encode("\u{fffd}", ordinary)
        .expect("official literal replacement character");
    assert_eq!(
        tokenizer
            .decode(&replacement, DecodeSpecialMode::PreserveConfigured)
            .expect("official literal replacement character round-trip"),
        "\u{fffd}"
    );
}

fn assert_error<T: std::fmt::Debug>(
    result: Result<T, super::TokenizerError>,
    expected: TokenizerErrorKind,
) {
    assert_eq!(result.expect_err("operation must fail").kind(), expected);
}

fn synthetic_document() -> TokenizerDocument {
    let mut vocab = BTreeMap::new();
    vocab.insert(END_OF_TEXT.to_owned(), END_OF_TEXT_ID);
    vocab.insert(PADDING_SPECIAL.to_owned(), PADDING_SPECIAL_ID);
    for id in 2..BASE_VOCABULARY_SIZE as u32 {
        vocab.insert(format!("token-{id}"), id);
    }
    TokenizerDocument {
        version: "1.0".to_owned(),
        truncation: None,
        padding: None,
        added_tokens: synthetic_added_tokens(),
        normalizer: serde_json::json!({"type": "NFC"}),
        pre_tokenizer: serde_json::json!({
            "type": "ByteLevel",
            "add_prefix_space": false,
            "trim_offsets": true,
            "use_regex": true
        }),
        post_processor: serde_json::json!({
            "type": "TemplateProcessing",
            "single": [{"Sequence": {"id": "A", "type_id": 0}}],
            "pair": [
                {"Sequence": {"id": "A", "type_id": 0}},
                {"Sequence": {"id": "B", "type_id": 1}}
            ],
            "special_tokens": {}
        }),
        decoder: serde_json::json!({
            "type": "ByteLevel",
            "add_prefix_space": false,
            "trim_offsets": true,
            "use_regex": true
        }),
        model: BpeDocument {
            kind: "BPE".to_owned(),
            dropout: None,
            unk_token: None,
            continuing_subword_prefix: None,
            end_of_word_suffix: None,
            fuse_unk: false,
            byte_fallback: false,
            ignore_merges: false,
            vocab,
            merges: vec!["a b".to_owned(); MERGE_COUNT],
        },
    }
}

fn synthetic_added_tokens() -> Vec<AddedTokenDocument> {
    (0..ADDED_TOKEN_ENTRIES)
        .map(|index| {
            if index == 0 {
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
                AddedTokenDocument {
                    id: BASE_VOCABULARY_SIZE as u32 + (index - 2) as u32,
                    content: " ".repeat(26 - index),
                    single_word: false,
                    lstrip: false,
                    rstrip: false,
                    normalized: true,
                    special: false,
                }
            }
        })
        .collect()
}

fn synthetic_config(added: &[AddedTokenDocument]) -> TokenizerConfigDocument {
    TokenizerConfigDocument {
        add_bos_token: false,
        add_eos_token: false,
        add_prefix_space: false,
        added_tokens_decoder: added
            .iter()
            .map(|token| {
                (
                    token.id.to_string(),
                    ConfigAddedToken {
                        content: token.content.clone(),
                        lstrip: token.lstrip,
                        normalized: token.normalized,
                        rstrip: token.rstrip,
                        single_word: token.single_word,
                        special: token.special,
                    },
                )
            })
            .collect(),
        bos_token: END_OF_TEXT.to_owned(),
        clean_up_tokenization_spaces: true,
        eos_token: END_OF_TEXT.to_owned(),
        model_max_length: serde_json::json!(1.0e30),
        pad_token: None,
        tokenizer_class: "GPTNeoXTokenizer".to_owned(),
        unk_token: END_OF_TEXT.to_owned(),
    }
}

fn synthetic_special_tokens() -> SpecialTokensDocument {
    let token = || SpecialTokenDocument {
        content: END_OF_TEXT.to_owned(),
        lstrip: false,
        normalized: false,
        rstrip: false,
        single_word: false,
    };
    SpecialTokensDocument {
        bos_token: token(),
        eos_token: token(),
        unk_token: token(),
    }
}

fn synthetic_runtime() -> ProductionTokenizer {
    let decoder = official_byte_decoder();
    let mut vocab = Vocab::default();
    vocab.insert(END_OF_TEXT.to_owned(), END_OF_TEXT_ID);
    vocab.insert(PADDING_SPECIAL.to_owned(), PADDING_SPECIAL_ID);
    vocab.insert("©".to_owned(), 104);
    vocab.insert("Ã".to_owned(), 127);
    let mut next_id = 2_u32;
    for character in decoder.keys() {
        if *character == '©' || *character == 'Ã' {
            continue;
        }
        while next_id == 104 || next_id == 127 {
            next_id += 1;
        }
        vocab.insert(character.to_string(), next_id);
        next_id += 1;
    }
    let model = BPE::builder()
        .vocab_and_merges(vocab, vec![])
        .build()
        .expect("synthetic BPE");
    let mut recognize_configured = Tokenizer::new(model);
    recognize_configured
        .with_normalizer(Some(NFC))
        .expect("NFC normalizer");
    recognize_configured.with_pre_tokenizer(Some(ByteLevel::new(false, true, true)));
    recognize_configured.with_decoder(Some(ByteLevel::new(false, true, true)));
    recognize_configured
        .add_special_tokens([
            AddedToken::from(END_OF_TEXT, true),
            AddedToken::from(PADDING_SPECIAL, true),
        ])
        .expect("synthetic specials");
    recognize_configured.set_encode_special_tokens(false);
    let mut encode_as_text = recognize_configured.clone();
    encode_as_text.set_encode_special_tokens(true);
    ProductionTokenizer {
        recognize_configured,
        encode_as_text,
        report: canonical_report(),
    }
}
