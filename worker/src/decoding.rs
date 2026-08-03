use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

const JSON_START: &str = "{\"answer\":\"";
const JSON_SEPARATOR: &str = "\",\"confidence\":\"";
const JSON_END: &str = "\"}";
const EOS_TOKEN_ID: u32 = 2;
const JSON_GRAMMAR_TOKENS: u32 = 6;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SamplingConfig {
    #[serde(default)]
    pub temperature: f32,
    #[serde(default)]
    pub top_k: u32,
    #[serde(default = "default_top_p")]
    pub top_p: f32,
    #[serde(default = "default_repetition_penalty")]
    pub repetition_penalty: f32,
    #[serde(default)]
    pub seed: u64,
    #[serde(default)]
    pub banned_token_ids: Vec<u32>,
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            temperature: 0.0,
            top_k: 0,
            top_p: default_top_p(),
            repetition_penalty: default_repetition_penalty(),
            seed: 0,
            banned_token_ids: Vec::new(),
        }
    }
}

impl SamplingConfig {
    pub(crate) fn validate(&self, vocabulary: usize) -> Result<(), String> {
        if !self.temperature.is_finite() || !(0.0..=100.0).contains(&self.temperature) {
            return Err("temperature must be finite and between 0 and 100".to_owned());
        }
        if self.top_k as usize > vocabulary {
            return Err("top_k cannot exceed the vocabulary size".to_owned());
        }
        if !self.top_p.is_finite() || !(0.0 < self.top_p && self.top_p <= 1.0) {
            return Err("top_p must be finite and in (0, 1]".to_owned());
        }
        if !self.repetition_penalty.is_finite() || !(1.0..=100.0).contains(&self.repetition_penalty)
        {
            return Err("repetition_penalty must be finite and between 1 and 100".to_owned());
        }
        if self
            .banned_token_ids
            .iter()
            .any(|token| *token as usize >= vocabulary)
        {
            return Err("banned token ID is outside the model vocabulary".to_owned());
        }
        let distinct = self
            .banned_token_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if distinct.len() != self.banned_token_ids.len() {
            return Err("banned_token_ids must not contain duplicates".to_owned());
        }
        if distinct.len() == vocabulary {
            return Err("banned_token_ids cannot mask the entire vocabulary".to_owned());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    #[default]
    Text,
    JsonSchema {
        json_schema: JsonSchemaEnvelope,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JsonSchemaEnvelope {
    pub name: String,
    #[serde(default)]
    pub strict: bool,
    pub schema: TinyObjectSchema,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TinyObjectSchema {
    #[serde(rename = "type")]
    pub kind: String,
    pub properties: BTreeMap<String, TinyStringSchema>,
    pub required: Vec<String>,
    #[serde(rename = "additionalProperties")]
    pub additional_properties: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TinyStringSchema {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "enum")]
    pub choices: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DecodingConfig {
    #[serde(default)]
    pub sampling: SamplingConfig,
    #[serde(default)]
    pub response_format: ResponseFormat,
}

pub fn inference_summary_response_format() -> ResponseFormat {
    let properties = [
        (
            "answer".to_owned(),
            TinyStringSchema {
                kind: "string".to_owned(),
                choices: vec![
                    "InferLab".to_owned(),
                    "systems".to_owned(),
                    "tokens".to_owned(),
                ],
            },
        ),
        (
            "confidence".to_owned(),
            TinyStringSchema {
                kind: "string".to_owned(),
                choices: vec!["high".to_owned(), "medium".to_owned(), "low".to_owned()],
            },
        ),
    ]
    .into_iter()
    .collect();
    ResponseFormat::JsonSchema {
        json_schema: JsonSchemaEnvelope {
            name: "inference_summary".to_owned(),
            strict: true,
            schema: TinyObjectSchema {
                kind: "object".to_owned(),
                properties,
                required: vec!["answer".to_owned(), "confidence".to_owned()],
                additional_properties: Some(false),
            },
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecodingKind {
    Text,
    JsonSchema,
}

#[derive(Clone, Debug)]
pub(crate) struct TokenDfa {
    states: Vec<BTreeMap<u32, usize>>,
    state: usize,
    schema_name: String,
}

impl TokenDfa {
    pub(crate) fn allowed_token_ids(&self) -> Vec<u32> {
        self.states
            .get(self.state)
            .map(|transitions| transitions.keys().copied().collect())
            .unwrap_or_default()
    }

    pub(crate) fn advance(&mut self, token_id: u32) -> Result<(), String> {
        let next = self
            .states
            .get(self.state)
            .and_then(|transitions| transitions.get(&token_id))
            .copied()
            .ok_or_else(|| {
                format!(
                    "token {token_id} is not valid in JSON grammar state {}",
                    self.state
                )
            })?;
        self.state = next;
        Ok(())
    }

    pub(crate) fn state(&self) -> u32 {
        self.state as u32
    }

    pub(crate) fn is_accepting(&self) -> bool {
        self.state + 1 == self.states.len()
    }

    pub(crate) fn schema_name(&self) -> &str {
        &self.schema_name
    }

    pub(crate) fn validate_banned_tokens(&self, banned_token_ids: &[u32]) -> Result<(), String> {
        let banned = banned_token_ids.iter().copied().collect::<BTreeSet<_>>();
        for (state, transitions) in self.states.iter().enumerate().take(self.states.len() - 1) {
            if transitions.keys().all(|token| banned.contains(token)) {
                return Err(format!(
                    "banned_token_ids remove every legal JSON token in grammar state {state}"
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn compile_constraint(
    response_format: &ResponseFormat,
    vocabulary: &[String],
    max_tokens: u32,
) -> Result<Option<TokenDfa>, String> {
    let ResponseFormat::JsonSchema { json_schema } = response_format else {
        return Ok(None);
    };
    if max_tokens < JSON_GRAMMAR_TOKENS {
        return Err(format!(
            "the v0.10 JSON grammar requires at least {JSON_GRAMMAR_TOKENS} max_tokens"
        ));
    }
    if json_schema.name.trim().is_empty() {
        return Err("json_schema.name must not be empty".to_owned());
    }
    if !json_schema.strict {
        return Err("the v0.10 JSON grammar requires json_schema.strict=true".to_owned());
    }
    let schema = &json_schema.schema;
    if schema.kind != "object" {
        return Err("the v0.10 JSON grammar supports only type=object".to_owned());
    }
    if schema.additional_properties != Some(false) {
        return Err("the v0.10 JSON grammar requires additionalProperties=false".to_owned());
    }
    let property_names = schema.properties.keys().cloned().collect::<BTreeSet<_>>();
    let expected_names = ["answer".to_owned(), "confidence".to_owned()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    if property_names != expected_names {
        return Err(
            "the v0.10 JSON grammar requires exactly answer and confidence properties".to_owned(),
        );
    }
    let required = schema.required.iter().cloned().collect::<BTreeSet<_>>();
    if required != expected_names || required.len() != schema.required.len() {
        return Err(
            "json_schema.required must contain answer and confidence exactly once".to_owned(),
        );
    }

    let answer = compile_enum("answer", &schema.properties["answer"], vocabulary)?;
    let confidence = compile_enum("confidence", &schema.properties["confidence"], vocabulary)?;
    let start = token_id(vocabulary, JSON_START)?;
    let separator = token_id(vocabulary, JSON_SEPARATOR)?;
    let end = token_id(vocabulary, JSON_END)?;

    let mut states = vec![BTreeMap::new(); 7];
    states[0].insert(start, 1);
    for token in answer {
        states[1].insert(token, 2);
    }
    states[2].insert(separator, 3);
    for token in confidence {
        states[3].insert(token, 4);
    }
    states[4].insert(end, 5);
    states[5].insert(EOS_TOKEN_ID, 6);
    Ok(Some(TokenDfa {
        states,
        state: 0,
        schema_name: json_schema.name.clone(),
    }))
}

fn compile_enum(
    name: &str,
    schema: &TinyStringSchema,
    vocabulary: &[String],
) -> Result<Vec<u32>, String> {
    if schema.kind != "string" {
        return Err(format!("json_schema property {name} must have type=string"));
    }
    if schema.choices.is_empty() {
        return Err(format!(
            "json_schema property {name} needs a non-empty enum"
        ));
    }
    let mut tokens = Vec::with_capacity(schema.choices.len());
    let mut unique = BTreeSet::new();
    for choice in &schema.choices {
        let token = token_id(vocabulary, choice).map_err(|_| {
            format!("json_schema enum value {choice:?} is not one complete model token")
        })?;
        if !unique.insert(token) {
            return Err(format!(
                "json_schema property {name} contains duplicate enum values"
            ));
        }
        tokens.push(token);
    }
    Ok(tokens)
}

fn token_id(vocabulary: &[String], token: &str) -> Result<u32, String> {
    vocabulary
        .iter()
        .position(|candidate| candidate == token)
        .and_then(|index| u32::try_from(index).ok())
        .ok_or_else(|| format!("model vocabulary does not contain required token {token:?}"))
}

fn default_top_p() -> f32 {
    1.0
}

fn default_repetition_penalty() -> f32 {
    1.0
}
