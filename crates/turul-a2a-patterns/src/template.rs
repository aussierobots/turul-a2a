//! Prompt template rendering used by `SkillCard::render_prompt`.
//!
//! Grammar: `{{ path }}` with dotted access (`{{ user.name }}`); literal
//! `{{` is escaped as `\{{`. Missing variables surface as
//! `RenderError::MissingVariable` rather than silently substituting empty.

use serde_json::Value;

use crate::error::RenderError;

/// Render `{{ path }}` substitutions against a JSON object.
///
/// `\{{` is treated as a literal `{{` (no further interpretation).
pub(crate) fn render_template(template: &str, params: &Value) -> Result<String, RenderError> {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        // Escape: `\{{` -> literal `{{`.
        if bytes[i] == b'\\' && i + 2 < bytes.len() && bytes[i + 1] == b'{' && bytes[i + 2] == b'{'
        {
            out.push_str("{{");
            i += 3;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find closing `}}`.
            let start = i + 2;
            let close = template[start..].find("}}").ok_or(RenderError::Syntax {
                offset: i,
                reason: "unterminated `{{` — missing closing `}}`".to_string(),
            })?;
            let raw = &template[start..start + close];
            let path = raw.trim();
            if path.is_empty() {
                return Err(RenderError::Syntax {
                    offset: i,
                    reason: "empty template expression".to_string(),
                });
            }
            let value = resolve_path(params, path).ok_or(RenderError::MissingVariable {
                path: path.to_string(),
                offset: i,
            })?;
            out.push_str(&render_value(value));
            i = start + close + 2;
            continue;
        }
        // Push one UTF-8 char and advance.
        let ch = template[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    Ok(out)
}

fn resolve_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path.split('.') {
        if seg.is_empty() {
            return None;
        }
        cur = cur.as_object()?.get(seg)?;
    }
    Some(cur)
}

fn render_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}
