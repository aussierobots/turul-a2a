//! `SkillCard` projection of an Agent Skills `SKILL.md` and its helpers.
//!
//! Hosts the `SkillCard` struct, `ExecutionHints`, the projection from a
//! loaded [`Skill`] (its `a2a-*` metadata keys and `assets/*.json` files),
//! and the `to_agent_skill` / `render_prompt` / `validate_input` /
//! `validate_output` methods.

use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::str::FromStr;

use serde_json::Value;
use turul_agent_skills::{Skill, load_str};

use turul_a2a_proto::AgentSkill;

use crate::error::{ManifestError, RenderError, ValidationError};
use crate::schema::{compile_schema, validate_json};
use crate::template::render_template;

const ID: &str = "a2a-id";
const NAME: &str = "a2a-name";
const TAGS: &str = "a2a-tags";
const EXAMPLES: &str = "a2a-examples";
const INPUT_MODES: &str = "a2a-input-modes";
const OUTPUT_MODES: &str = "a2a-output-modes";
const SECURITY_REQUIREMENTS: &str = "a2a-security-requirements";
const INPUT_SCHEMA: &str = "a2a-input-schema";
const OUTPUT_SCHEMA: &str = "a2a-output-schema";
const MAX_TOKENS: &str = "a2a-max-tokens";
const TEMPERATURE: &str = "a2a-temperature";
const TOP_P: &str = "a2a-top-p";
const PROVIDER_CONFIG: &str = "a2a-provider-config";

/// Every metadata key this crate reads. Any other key with the `a2a-`
/// prefix is rejected, so a misspelt key fails instead of being ignored.
const KEYS: [&str; 13] = [
    ID,
    NAME,
    TAGS,
    EXAMPLES,
    INPUT_MODES,
    OUTPUT_MODES,
    SECURITY_REQUIREMENTS,
    INPUT_SCHEMA,
    OUTPUT_SCHEMA,
    MAX_TOKENS,
    TEMPERATURE,
    TOP_P,
    PROVIDER_CONFIG,
];

const PREFIX: &str = "a2a-";

/// The A2A view of an Agent Skills `SKILL.md`.
///
/// The skill's spec `name` is its catalog identity; `metadata.a2a-id`, when
/// present, is the wire id (`AgentSkill.id`) and otherwise the `name` is.
/// The other A2A fields come from `a2a-*` metadata keys, and the schemas and
/// provider config from JSON files under the skill directory. LLM-backed
/// skills supply execution hints and provider config; non-LLM skills omit
/// them. The Markdown body is the prompt template (used only by LLM-backed
/// skills); template grammar is `{{ name }}` with dotted paths.
///
/// [`SkillCard::from_embedded`], [`SkillCard::from_skill`] and
/// `TryFrom<&Skill>` are the only constructors: the struct is `#[non_exhaustive]` and deliberately
/// implements neither `Deserialize` nor `Default`, so every card has passed
/// the metadata, strict-keyword and schema-compilation checks.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SkillCard {
    // AgentSkill discovery fields (all eight).
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub examples: Vec<String>,
    pub input_modes: Vec<String>,
    pub output_modes: Vec<String>,
    pub security_requirements: Vec<String>,

    // Provider-neutral execution metadata (optional).
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub execution_hints: Option<ExecutionHints>,

    // Opaque to this crate — the patterns layer does not interpret
    // provider-specific configuration; that is the adopter's concern.
    pub provider_config: Option<Value>,

    /// Markdown body (prompt template for LLM-backed skills).
    pub body: String,
}

/// Provider-neutral execution hints.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ExecutionHints {
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
}

impl SkillCard {
    /// Project a loaded skill onto a `SkillCard`, reading asset files
    /// through `assets`.
    ///
    /// `assets` is called with the path a metadata key names, relative to
    /// the skill directory (`assets/input.schema.json`), and returns the
    /// file's text or `None` when it is absent. Use it for a skill embedded
    /// in the binary; `SkillCard::try_from(&skill)` reads the files from the
    /// directory the skill was loaded from.
    ///
    /// List keys hold a JSON array of strings, the execution hints a
    /// decimal string, and the schema and provider-config keys a relative
    /// path to a JSON file. An unknown `a2a-*` key, a path that leaves the
    /// skill directory, a missing asset, or a value that does not parse is
    /// a [`ManifestError::Parse`] at that key. Declared schemas must be
    /// usable: unknown JSON Schema keywords and schemas the 2020-12
    /// validator cannot compile are a [`ManifestError::Schema`], never
    /// deferred to the first validate call.
    pub fn from_skill(
        skill: &Skill,
        assets: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ManifestError> {
        let metadata = &skill.properties.metadata;
        if let Some(key) = metadata
            .keys()
            .find(|key| key.starts_with(PREFIX) && !KEYS.contains(&key.as_str()))
        {
            return Err(parse_error(key, "unknown metadata key"));
        }

        let input_schema = asset(metadata, INPUT_SCHEMA, &assets)?;
        let output_schema = asset(metadata, OUTPUT_SCHEMA, &assets)?;
        for (schema, key) in [
            (&input_schema, INPUT_SCHEMA),
            (&output_schema, OUTPUT_SCHEMA),
        ] {
            if let Some(schema) = schema {
                compile_schema(schema).map_err(|e| ManifestError::Schema {
                    location: location(key),
                    reason: e.to_string(),
                })?;
            }
        }

        let hints = ExecutionHints {
            max_tokens: number(metadata, MAX_TOKENS)?,
            temperature: number(metadata, TEMPERATURE)?,
            top_p: number(metadata, TOP_P)?,
        };
        let execution_hints =
            (hints.max_tokens.is_some() || hints.temperature.is_some() || hints.top_p.is_some())
                .then_some(hints);

        let name = skill.properties.name.as_str();
        Ok(SkillCard {
            id: metadata.get(ID).map_or(name, String::as_str).to_string(),
            name: metadata.get(NAME).map_or(name, String::as_str).to_string(),
            description: skill.properties.description.clone(),
            tags: list(metadata, TAGS)?,
            examples: list(metadata, EXAMPLES)?,
            input_modes: list(metadata, INPUT_MODES)?,
            output_modes: list(metadata, OUTPUT_MODES)?,
            security_requirements: list(metadata, SECURITY_REQUIREMENTS)?,
            input_schema,
            output_schema,
            execution_hints,
            provider_config: asset(metadata, PROVIDER_CONFIG, &assets)?,
            body: skill.body.clone(),
        })
    }

    /// Load a skill embedded in the binary and project it.
    ///
    /// `dir_name` is the skill's directory name, `skill_md` the text of its
    /// `SKILL.md` and `assets` its asset files as `(relative path, text)`
    /// pairs, typically all from `include_str!`. Embedded content is fixed at
    /// build time, so any problem the loader reports — an unknown field, a
    /// name that differs from `dir_name` — is an error here rather than a
    /// diagnostic. Errors from loading are [`ManifestError::Parse`] at
    /// `SKILL.md`; the rest are as for [`SkillCard::from_skill`].
    pub fn from_embedded(
        dir_name: &str,
        skill_md: &str,
        assets: &[(&str, &str)],
    ) -> Result<Self, ManifestError> {
        let loaded = load_str(dir_name, skill_md).map_err(|skip| ManifestError::Parse {
            location: "SKILL.md".to_string(),
            reason: skip.to_string(),
        })?;
        if let Some(problem) = loaded.diagnostics.first() {
            return Err(ManifestError::Parse {
                location: "SKILL.md".to_string(),
                reason: problem.message().to_string(),
            });
        }
        SkillCard::from_skill(&loaded.skill, |path| {
            assets
                .iter()
                .find(|(asset, _)| *asset == path)
                .map(|(_, text)| text.to_string())
        })
    }

    /// Project the eight A2A discovery fields onto an `AgentSkill`.
    ///
    /// Schemas, execution hints, and provider config are intentionally NOT
    /// projected — they are Turul-local planning metadata, not wire fields.
    /// For manifest-backed skills, `params_schema` is derived from the
    /// manifest's input schema, ensuring there is exactly one schema per
    /// skill.
    pub fn to_agent_skill(&self) -> AgentSkill {
        AgentSkill {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            tags: self.tags.clone(),
            examples: self.examples.clone(),
            input_modes: self.input_modes.clone(),
            output_modes: self.output_modes.clone(),
            // The manifest representation of `securityRequirements` is a
            // list of scheme names. Mapping to the proto's richer
            // `SecurityRequirement` (scheme -> scopes) is
            // empty-scope-by-default; empty list -> empty Vec.
            security_requirements: self
                .security_requirements
                .iter()
                .map(|scheme| {
                    let mut schemes = std::collections::HashMap::new();
                    schemes.insert(scheme.clone(), turul_a2a_proto::StringList { list: vec![] });
                    turul_a2a_proto::SecurityRequirement { schemes }
                })
                .collect(),
        }
    }

    /// Render the Markdown body against the given input parameters.
    ///
    /// Grammar: `{{ path }}` with dotted access (`{{ user.name }}`).
    /// Literal `{{` escaped as `\{{`. Missing variable -> structured
    /// `RenderError::MissingVariable` (no silent empty substitution).
    pub fn render_prompt(&self, params: &Value) -> Result<String, RenderError> {
        render_template(&self.body, params)
    }

    /// Validate input against the manifest's input schema. A card
    /// without an input schema accepts any input. Error locations are
    /// JSON pointers into the instance rooted at `#`, as in
    /// [`validate_json`](crate::validate_json).
    pub fn validate_input(&self, input: &Value) -> Result<(), ValidationError> {
        match &self.input_schema {
            Some(schema) => validate_json(schema, input),
            None => Ok(()),
        }
    }

    /// Validate output against the manifest's output schema. A card
    /// without an output schema accepts any output. Error locations are
    /// JSON pointers into the instance rooted at `#`, as in
    /// [`validate_json`](crate::validate_json).
    pub fn validate_output(&self, output: &Value) -> Result<(), ValidationError> {
        match &self.output_schema {
            Some(schema) => validate_json(schema, output),
            None => Ok(()),
        }
    }
}

/// Reads asset files from the directory the skill was loaded from. The
/// resolved file must lie inside that directory, so a symlink pointing out
/// of it is refused. A file that is absent, refused or unreadable fails as
/// not found; so does every asset of a skill with no location (one that was
/// built, or loaded from text).
impl TryFrom<&Skill> for SkillCard {
    type Error = ManifestError;

    fn try_from(skill: &Skill) -> Result<Self, ManifestError> {
        let dir = skill.location().and_then(Path::parent);
        SkillCard::from_skill(skill, |path| {
            let dir = dir?;
            let file = dir.join(path).canonicalize().ok()?;
            if !file.starts_with(dir) {
                return None;
            }
            std::fs::read_to_string(file).ok()
        })
    }
}

fn location(key: &str) -> String {
    format!("metadata.{key}")
}

fn parse_error(key: &str, reason: impl Into<String>) -> ManifestError {
    ManifestError::Parse {
        location: location(key),
        reason: reason.into(),
    }
}

fn list(metadata: &BTreeMap<String, String>, key: &str) -> Result<Vec<String>, ManifestError> {
    match metadata.get(key) {
        Some(value) => serde_json::from_str(value)
            .map_err(|e| parse_error(key, format!("expected a JSON array of strings: {e}"))),
        None => Ok(Vec::new()),
    }
}

fn number<T: FromStr>(
    metadata: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<T>, ManifestError>
where
    T::Err: std::fmt::Display,
{
    metadata
        .get(key)
        .map(|value| {
            value
                .parse()
                .map_err(|e| parse_error(key, format!("expected a number: {e}")))
        })
        .transpose()
}

fn asset(
    metadata: &BTreeMap<String, String>,
    key: &str,
    assets: &impl Fn(&str) -> Option<String>,
) -> Result<Option<Value>, ManifestError> {
    let Some(path) = metadata.get(key) else {
        return Ok(None);
    };
    let inside = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if path.is_empty() || !inside {
        return Err(parse_error(
            key,
            format!("`{path}` is not a relative path inside the skill directory"),
        ));
    }
    let text = assets(path).ok_or_else(|| parse_error(key, format!("asset `{path}` not found")))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| parse_error(key, format!("asset `{path}` is not JSON: {e}")))
}
