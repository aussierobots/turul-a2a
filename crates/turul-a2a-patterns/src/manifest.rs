//! `SkillCard` (SKILL.md) parsing and projection helpers.
//!
//! Hosts the `SkillCard` struct, `ExecutionHints`, the YAML→JSON
//! frontmatter shim, and the `to_agent_skill` / `render_prompt` /
//! `validate_input` / `validate_output` methods.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use turul_a2a_proto::AgentSkill;

use crate::error::{ManifestError, RenderError, ValidationError};
use crate::schema::{compile_schema, validate_json};
use crate::template::render_template;

/// Parsed SKILL.md manifest.
///
/// camelCase frontmatter only (no snake_case alias acceptance). LLM-backed
/// skills supply execution metadata + provider_config; non-LLM skills omit
/// them. The Markdown body is the prompt template (used only by LLM-backed
/// skills); template grammar is `{{ name }}` with dotted paths.
///
/// [`SkillCard::parse`] is the only constructor: the struct is
/// `#[non_exhaustive]` and deliberately implements neither `Deserialize`
/// nor `Default`, so every card has passed the frontmatter,
/// strict-keyword and schema-compilation checks.
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

/// Serde shape of the SKILL.md frontmatter. Private so that
/// [`SkillCard::parse`] stays the only way to obtain a `SkillCard`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Frontmatter {
    id: String,
    name: String,
    description: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    examples: Vec<String>,
    #[serde(default)]
    input_modes: Vec<String>,
    #[serde(default)]
    output_modes: Vec<String>,
    #[serde(default)]
    security_requirements: Vec<String>,
    #[serde(default)]
    input_schema: Option<Value>,
    #[serde(default)]
    output_schema: Option<Value>,
    #[serde(default)]
    execution_hints: Option<ExecutionHints>,
    #[serde(default)]
    provider_config: Option<Value>,
}

/// Provider-neutral execution hints.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct ExecutionHints {
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
}

/// Translate a `serde_yaml::Value` (with arbitrary scalar keys) into a
/// `serde_json::Value`. Frontmatter that resolves to non-string mapping
/// keys (rare in well-formed YAML) is rejected.
fn yaml_to_json(value: serde_yaml::Value) -> Result<Value, ManifestError> {
    use serde_yaml::Value as Y;
    Ok(match value {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(u) = n.as_u64() {
                Value::from(u)
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        Y::String(s) => Value::String(s),
        Y::Sequence(seq) => Value::Array(
            seq.into_iter()
                .map(yaml_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Y::Mapping(map) => {
            let mut obj = Map::new();
            for (k, v) in map {
                let key = match k {
                    Y::String(s) => s,
                    other => {
                        return Err(ManifestError::Parse {
                            location: "frontmatter".to_string(),
                            reason: format!("non-string mapping key: {other:?}"),
                        });
                    }
                };
                obj.insert(key, yaml_to_json(v)?);
            }
            Value::Object(obj)
        }
        Y::Tagged(tagged) => yaml_to_json(tagged.value)?,
    })
}

impl SkillCard {
    /// Parse a SKILL.md document into a `SkillCard`.
    ///
    /// Format: `---\n<frontmatter>\n---\n<body>`. Frontmatter is YAML
    /// with camelCase keys (strict — no snake_case aliases); body is an
    /// opaque template string consumed by `render_prompt`.
    ///
    /// Declared `inputSchema` / `outputSchema` must be usable: unknown
    /// JSON Schema keywords and schemas the 2020-12 validator cannot
    /// compile are rejected here as [`ManifestError::Schema`], never
    /// deferred to the first validate call.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let (frontmatter, body) = split_frontmatter(text).ok_or(ManifestError::Parse {
            location: "document".to_string(),
            reason: "missing `---` frontmatter delimiters".to_string(),
        })?;

        let yaml: serde_yaml::Value =
            serde_yaml::from_str(frontmatter).map_err(|e| ManifestError::Parse {
                location: "frontmatter".to_string(),
                reason: e.to_string(),
            })?;
        let json = yaml_to_json(yaml)?;

        let fm: Frontmatter = serde_json::from_value(json).map_err(|e| ManifestError::Parse {
            location: "frontmatter".to_string(),
            reason: e.to_string(),
        })?;

        for (schema, location) in [
            (&fm.input_schema, "/inputSchema"),
            (&fm.output_schema, "/outputSchema"),
        ] {
            if let Some(schema) = schema {
                compile_schema(schema).map_err(|e| ManifestError::Schema {
                    location: location.to_string(),
                    reason: e.to_string(),
                })?;
            }
        }

        Ok(SkillCard {
            id: fm.id,
            name: fm.name,
            description: fm.description,
            tags: fm.tags,
            examples: fm.examples,
            input_modes: fm.input_modes,
            output_modes: fm.output_modes,
            security_requirements: fm.security_requirements,
            input_schema: fm.input_schema,
            output_schema: fm.output_schema,
            execution_hints: fm.execution_hints,
            provider_config: fm.provider_config,
            body: body.to_string(),
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

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?;
    // The opening delimiter must be followed by a newline.
    let after_open_nl = rest.find('\n')?;
    let body_start = &rest[after_open_nl + 1..];
    // Search for the closing `\n---\n` (or `\n---` at EOF).
    let close = body_start.find("\n---")?;
    let frontmatter = &body_start[..close];
    let after_close = &body_start[close + 4..]; // skip "\n---"
    // The closing delimiter may be followed by `\n` or EOF.
    let body = after_close.strip_prefix('\n').unwrap_or(after_close);
    Some((frontmatter, body))
}
