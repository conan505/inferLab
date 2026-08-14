use std::{ffi::OsString, fmt, io::Read, path::PathBuf, process::ExitCode};

use model_artifacts::{
    DecodeSpecialMode, EncodeOptions, LiteralSpecialMode, ProductionTokenizer, load_pinned_pythia,
};
use serde::{Deserialize, Serialize};

const USAGE: &str = "usage: inferlab-model-inspect inspect --lock <lock.json> --assets <directory>\n       inferlab-model-inspect tokenize --lock <lock.json> --assets <directory>";
const REQUEST_SCHEMA: &str = "inferlab.tokenizer.request.v1";
const RESPONSE_SCHEMA: &str = "inferlab.tokenizer.response.v1";
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

enum Command {
    Inspect,
    Tokenize,
}

struct Arguments {
    command: Command,
    lock: PathBuf,
    assets: PathBuf,
}

fn parse_arguments(arguments: impl IntoIterator<Item = OsString>) -> Result<Arguments, ()> {
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    let command = match arguments.next().as_deref() {
        Some(command) if command == std::ffi::OsStr::new("inspect") => Command::Inspect,
        Some(command) if command == std::ffi::OsStr::new("tokenize") => Command::Tokenize,
        _ => return Err(()),
    };
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--lock")) {
        return Err(());
    }
    let lock = arguments.next().map(PathBuf::from).ok_or(())?;
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--assets")) {
        return Err(());
    }
    let assets = arguments.next().map(PathBuf::from).ok_or(())?;
    if arguments.next().is_some() {
        return Err(());
    }
    Ok(Arguments {
        command,
        lock,
        assets,
    })
}

fn main() -> ExitCode {
    let arguments = match parse_arguments(std::env::args_os()) {
        Ok(arguments) => arguments,
        Err(()) => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let bundle = match load_pinned_pythia(&arguments.lock, &arguments.assets) {
        Ok(bundle) => bundle,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    match arguments.command {
        Command::Inspect => emit_json(
            bundle.report(),
            "model artifact verification failed: report_encoding_failed",
        ),
        Command::Tokenize => {
            let tokenizer = match bundle.production_tokenizer() {
                Ok(tokenizer) => tokenizer,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::FAILURE;
                }
            };
            let request = match read_request(std::io::stdin().lock()) {
                Ok(request) => request,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::from(2);
                }
            };
            let response = match execute_request(&tokenizer, request) {
                Ok(response) => response,
                Err(error) => {
                    eprintln!("{error}");
                    return ExitCode::FAILURE;
                }
            };
            emit_json(
                &response,
                "tokenizer request failed: response_encoding_failed",
            )
        }
    }
}

fn emit_json(value: &impl Serialize, encoding_failure: &str) -> ExitCode {
    match serde_json::to_string_pretty(value) {
        Ok(encoded) => {
            println!("{encoded}");
            ExitCode::SUCCESS
        }
        Err(_) => {
            eprintln!("{encoding_failure}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum TokenizeRequest {
    Encode {
        schema: String,
        text: String,
        literal_specials: LiteralSpecialMode,
        add_special_tokens: bool,
    },
    Decode {
        schema: String,
        ids: Vec<u32>,
        configured_specials: DecodeSpecialMode,
    },
}

impl TokenizeRequest {
    fn schema(&self) -> &str {
        match self {
            Self::Encode { schema, .. } | Self::Decode { schema, .. } => schema,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
enum TokenizeResponse {
    Encode {
        schema: &'static str,
        operation: &'static str,
        token_count: usize,
        ids: Vec<u32>,
    },
    Decode {
        schema: &'static str,
        operation: &'static str,
        token_count: usize,
        text: String,
    },
}

fn read_request(mut input: impl Read) -> Result<TokenizeRequest, RequestError> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| RequestError::new(RequestErrorKind::StdinReadFailed))?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err(RequestError::new(RequestErrorKind::RequestOversize));
    }
    let input = std::str::from_utf8(&bytes)
        .map_err(|_| RequestError::new(RequestErrorKind::RequestInvalidUtf8))?;
    let request: TokenizeRequest = serde_json::from_str(input)
        .map_err(|_| RequestError::new(RequestErrorKind::RequestInvalid))?;
    if request.schema() != REQUEST_SCHEMA {
        return Err(RequestError::new(RequestErrorKind::RequestSchemaMismatch));
    }
    Ok(request)
}

fn execute_request(
    tokenizer: &ProductionTokenizer,
    request: TokenizeRequest,
) -> Result<TokenizeResponse, model_artifacts::TokenizerError> {
    match request {
        TokenizeRequest::Encode {
            text,
            literal_specials,
            add_special_tokens,
            ..
        } => {
            let ids = tokenizer.encode(
                &text,
                EncodeOptions {
                    literal_specials,
                    add_special_tokens,
                },
            )?;
            Ok(TokenizeResponse::Encode {
                schema: RESPONSE_SCHEMA,
                operation: "encode",
                token_count: ids.len(),
                ids,
            })
        }
        TokenizeRequest::Decode {
            ids,
            configured_specials,
            ..
        } => {
            let token_count = ids.len();
            let text = tokenizer.decode(&ids, configured_specials)?;
            Ok(TokenizeResponse::Decode {
                schema: RESPONSE_SCHEMA,
                operation: "decode",
                token_count,
                text,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestErrorKind {
    StdinReadFailed,
    RequestOversize,
    RequestInvalidUtf8,
    RequestInvalid,
    RequestSchemaMismatch,
}

impl RequestErrorKind {
    const fn code(self) -> &'static str {
        match self {
            Self::StdinReadFailed => "stdin_read_failed",
            Self::RequestOversize => "request_oversize",
            Self::RequestInvalidUtf8 => "request_invalid_utf8",
            Self::RequestInvalid => "request_invalid",
            Self::RequestSchemaMismatch => "request_schema_mismatch",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequestError {
    kind: RequestErrorKind,
}

impl RequestError {
    const fn new(kind: RequestErrorKind) -> Self {
        Self { kind }
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "tokenizer request failed: {}", self.kind.code())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor, Read};

    use super::{
        Command, MAX_REQUEST_BYTES, REQUEST_SCHEMA, RESPONSE_SCHEMA, RequestErrorKind,
        TokenizeRequest, TokenizeResponse, USAGE, parse_arguments, read_request,
    };
    use model_artifacts::{DecodeSpecialMode, LiteralSpecialMode};

    #[test]
    fn accepts_only_the_two_documented_offline_command_shapes() {
        let inspect = parse_arguments(
            [
                "inferlab-model-inspect",
                "inspect",
                "--lock",
                "lock.json",
                "--assets",
                "assets",
            ]
            .map(Into::into),
        )
        .expect("documented inspect arguments");
        assert!(matches!(inspect.command, Command::Inspect));
        assert_eq!(inspect.lock.to_string_lossy(), "lock.json");
        assert_eq!(inspect.assets.to_string_lossy(), "assets");

        let tokenize = parse_arguments(
            [
                "inferlab-model-inspect",
                "tokenize",
                "--lock",
                "lock.json",
                "--assets",
                "assets",
            ]
            .map(Into::into),
        )
        .expect("documented tokenize arguments");
        assert!(matches!(tokenize.command, Command::Tokenize));
    }

    #[test]
    fn rejects_network_or_ambiguous_cli_shapes() {
        for arguments in [
            vec!["inferlab-model-inspect", "fetch"],
            vec!["inferlab-model-inspect", "tokenize", "--lock", "lock.json"],
            vec![
                "inferlab-model-inspect",
                "inspect",
                "--lock",
                "lock.json",
                "--assets",
                "assets",
                "extra",
            ],
        ] {
            assert!(parse_arguments(arguments.into_iter().map(Into::into)).is_err());
        }
        assert!(USAGE.contains("inferlab-model-inspect inspect"));
        assert!(USAGE.contains("inferlab-model-inspect tokenize"));
    }

    #[test]
    fn strict_request_parser_accepts_both_semantic_operations() {
        let encode = format!(
            r#"{{"schema":"{REQUEST_SCHEMA}","operation":"encode","text":"hello","literal_specials":"encode_as_text","add_special_tokens":false}}"#
        );
        assert!(matches!(
            read_request(Cursor::new(encode)).expect("encode request"),
            TokenizeRequest::Encode {
                literal_specials: LiteralSpecialMode::EncodeAsText,
                add_special_tokens: false,
                ..
            }
        ));

        let decode = format!(
            r#"{{"schema":"{REQUEST_SCHEMA}","operation":"decode","ids":[127,104],"configured_specials":"preserve_configured"}}"#
        );
        assert!(matches!(
            read_request(Cursor::new(decode)).expect("decode request"),
            TokenizeRequest::Decode {
                configured_specials: DecodeSpecialMode::PreserveConfigured,
                ..
            }
        ));
    }

    #[test]
    fn stdin_is_bounded_utf8_and_strict_json_without_disclosures() {
        struct FailingReader;

        impl Read for FailingReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("sensitive reader detail"))
            }
        }

        let error = read_request(FailingReader).expect_err("reader failure");
        assert_eq!(error.kind, RequestErrorKind::StdinReadFailed);
        assert!(!error.to_string().contains("sensitive reader detail"));

        let oversize = vec![b' '; MAX_REQUEST_BYTES as usize + 1];
        let error = read_request(Cursor::new(oversize)).expect_err("oversize request");
        assert_eq!(error.kind, RequestErrorKind::RequestOversize);

        let error = read_request(Cursor::new([0xff])).expect_err("non-UTF-8 request");
        assert_eq!(error.kind, RequestErrorKind::RequestInvalidUtf8);

        for invalid in [
            format!(
                r#"{{"schema":"{REQUEST_SCHEMA}","operation":"encode","text":"secret","text":"duplicate","literal_specials":"encode_as_text","add_special_tokens":false}}"#
            ),
            format!(
                r#"{{"schema":"{REQUEST_SCHEMA}","operation":"decode","ids":[],"configured_specials":"skip_configured","unknown":true}}"#
            ),
            format!(
                r#"{{"schema":"{REQUEST_SCHEMA}","operation":"encode","text":"missing explicit options"}}"#
            ),
            format!(
                r#"{{"schema":"{REQUEST_SCHEMA}","operation":"decode","ids":[],"configured_specials":"skip_configured"}} trailing"#
            ),
            r#"{"schema":"wrong","operation":"decode","ids":[],"configured_specials":"skip_configured"}"#.to_owned(),
        ] {
            let error = read_request(Cursor::new(invalid)).expect_err("invalid request");
            assert!(matches!(
                error.kind,
                RequestErrorKind::RequestInvalid | RequestErrorKind::RequestSchemaMismatch
            ));
            assert!(!error.to_string().contains("secret"));
            assert!(!error.to_string().contains("duplicate"));
        }
    }

    #[test]
    fn deterministic_response_schema_has_no_implicit_cleanup_or_payload_metadata() {
        let response = TokenizeResponse::Encode {
            schema: RESPONSE_SCHEMA,
            operation: "encode",
            token_count: 2,
            ids: vec![127, 104],
        };
        assert_eq!(
            serde_json::to_string(&response).expect("response JSON"),
            format!(
                r#"{{"schema":"{RESPONSE_SCHEMA}","operation":"encode","token_count":2,"ids":[127,104]}}"#
            )
        );
    }
}
