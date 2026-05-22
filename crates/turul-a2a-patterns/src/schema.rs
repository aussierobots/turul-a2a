//! JSON Schema 2020-12 strict-keyword check and standalone `validate_json`
//! helper. The keyword table and walker are also reused by `SkillCard`
//! manifest validation in [`crate::manifest`].

use serde_json::Value;

use crate::error::ValidationError;

/// Recognised JSON Schema 2020-12 keywords. Anything outside this set in a
/// manifest's input/output schema is a strict-reject error.
pub(crate) const DRAFT_2020_12_KEYWORDS: &[&str] = &[
    // Core
    "$schema",
    "$id",
    "$ref",
    "$defs",
    "$anchor",
    "$dynamicAnchor",
    "$dynamicRef",
    "$vocabulary",
    "$comment",
    // Applicators
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "dependentSchemas",
    "prefixItems",
    "items",
    "contains",
    "properties",
    "patternProperties",
    "additionalProperties",
    "propertyNames",
    // Validation
    "type",
    "enum",
    "const",
    "multipleOf",
    "maximum",
    "exclusiveMaximum",
    "minimum",
    "exclusiveMinimum",
    "maxLength",
    "minLength",
    "pattern",
    "maxItems",
    "minItems",
    "uniqueItems",
    "maxContains",
    "minContains",
    "maxProperties",
    "minProperties",
    "required",
    "dependentRequired",
    // Format / content
    "format",
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
    // Metadata / annotations
    "title",
    "description",
    "default",
    "deprecated",
    "readOnly",
    "writeOnly",
    "examples",
    // Unevaluated
    "unevaluatedItems",
    "unevaluatedProperties",
];

pub(crate) fn is_known_draft_2020_12_keyword(k: &str) -> bool {
    DRAFT_2020_12_KEYWORDS.contains(&k)
}

/// Walk a schema and reject any unknown keywords (strict). Returns the
/// first offending keyword, if any, along with its JSON pointer.
pub(crate) fn strict_keyword_check(schema: &Value, pointer: &str) -> Result<(), ValidationError> {
    match schema {
        Value::Object(map) => {
            for (k, v) in map {
                // Subschemas inside `properties`/`patternProperties`/`$defs` may
                // use any property name — only the keys at *schema* positions
                // need to be A2020-12 keywords. Recurse into them as schemas.
                match k.as_str() {
                    "properties" | "patternProperties" | "$defs" | "dependentSchemas" => {
                        if let Value::Object(inner) = v {
                            for (name, sub) in inner {
                                let next = format!("{pointer}/{k}/{name}");
                                strict_keyword_check(sub, &next)?;
                            }
                        }
                        continue;
                    }
                    _ => {}
                }
                if !is_known_draft_2020_12_keyword(k) {
                    return Err(ValidationError::UnsupportedKeyword { keyword: k.clone() });
                }
                let next = format!("{pointer}/{k}");
                match v {
                    Value::Object(_) => strict_keyword_check(v, &next)?,
                    Value::Array(items) => {
                        for (i, item) in items.iter().enumerate() {
                            let p = format!("{next}/{i}");
                            if item.is_object() {
                                strict_keyword_check(item, &p)?;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate `instance` against `schema` (JSON Schema 2020-12).
///
/// Returns the same `ValidationError` shape used elsewhere in this
/// crate, with `location` rooted at `#` of the instance.
///
/// Schema keywords outside the 2020-12 dialect are rejected at this
/// call site via the same `strict_keyword_check` used by SKILL.md
/// manifest validation.
pub fn validate_json(
    schema: &serde_json::Value,
    instance: &serde_json::Value,
) -> Result<(), ValidationError> {
    strict_keyword_check(schema, "#")?;
    let validator = jsonschema::draft202012::new(schema).map_err(|e| ValidationError::Invalid {
        location: "#".to_string(),
        reason: format!("invalid schema: {e}"),
    })?;
    if let Some(err) = validator.iter_errors(instance).next() {
        let path = err.instance_path.to_string();
        let location = if path.is_empty() {
            "#".to_string()
        } else {
            format!("#{path}")
        };
        return Err(ValidationError::Invalid {
            location,
            reason: err.to_string(),
        });
    }
    Ok(())
}
