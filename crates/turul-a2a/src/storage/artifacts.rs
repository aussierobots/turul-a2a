//! Artifact-separation helpers shared by the persistent backends.
//!
//! Backends that split artifact bodies out of the task record (SQLite,
//! PostgreSQL, DynamoDB) keep a small manifest — `(artifact_id,
//! fingerprint)` pairs — next to the task blob and store each artifact
//! body as its own record. The manifest preserves the artifact order of
//! the original task so rehydrated reads are byte-identical to the
//! pre-separation wire shape. A task record without a manifest is a
//! legacy record whose artifacts (if any) are inline in the blob.
//!
//! The fingerprint is a SHA-256 over an explicitly canonicalized proto
//! JSON form of the artifact. Determinism is a property of the
//! canonicalizer (every JSON object is rewritten with sorted keys), not
//! of serde_json's map backing — prost binary encoding is unsuitable
//! because `pbjson_types::Struct` fields are HashMap-backed and encode
//! in nondeterministic order. A fingerprint mismatch can only cause a
//! spurious artifact rewrite, never corruption.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::A2aStorageError;

/// One manifest entry: which artifact exists and what content it had
/// when last written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManifestEntry {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    pub fingerprint: String,
}

/// Reconciliation plan for a full-task write: which artifact records to
/// write, which to delete, and the manifest that describes the result.
#[derive(Debug, Default)]
pub(crate) struct ReconcilePlan {
    /// Artifacts whose id is new or whose content changed, paired with
    /// their fingerprint. Order follows the incoming task's artifact order.
    pub writes: Vec<(turul_a2a_proto::Artifact, String)>,
    /// Manifest ids absent from the incoming artifact set.
    pub deletes: Vec<String>,
    /// Manifest describing the incoming artifact set, in task order.
    pub manifest: Vec<ManifestEntry>,
}

/// Recursively rewrite every JSON object with its keys in sorted order.
fn canonicalize(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> =
                map.into_iter().map(|(k, v)| (k, canonicalize(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(entries.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize).collect())
        }
        other => other,
    }
}

/// SHA-256 hex digest over the canonical proto JSON of an artifact.
pub(crate) fn artifact_fingerprint(
    artifact: &turul_a2a_proto::Artifact,
) -> Result<String, A2aStorageError> {
    let value = serde_json::to_value(artifact)
        .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;
    let canonical = serde_json::to_string(&canonicalize(value))
        .map_err(|e| A2aStorageError::SerializationError(e.to_string()))?;
    let digest = Sha256::digest(canonical.as_bytes());
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Diff the incoming artifact set against the stored manifest.
///
/// The incoming set is authoritative (replace semantics): unknown or
/// changed artifacts are written, manifest ids missing from the incoming
/// set are deleted, and the returned manifest lists the incoming
/// artifacts in task order.
pub(crate) fn reconcile(
    incoming: &[turul_a2a_proto::Artifact],
    stored: &[ManifestEntry],
) -> Result<ReconcilePlan, A2aStorageError> {
    let mut plan = ReconcilePlan::default();
    for artifact in incoming {
        let fingerprint = artifact_fingerprint(artifact)?;
        let unchanged = stored
            .iter()
            .any(|e| e.artifact_id == artifact.artifact_id && e.fingerprint == fingerprint);
        if !unchanged {
            plan.writes.push((artifact.clone(), fingerprint.clone()));
        }
        plan.manifest.push(ManifestEntry {
            artifact_id: artifact.artifact_id.clone(),
            fingerprint,
        });
    }
    for entry in stored {
        if !incoming.iter().any(|a| a.artifact_id == entry.artifact_id) {
            plan.deletes.push(entry.artifact_id.clone());
        }
    }
    Ok(plan)
}

pub(crate) fn manifest_to_json(manifest: &[ManifestEntry]) -> Result<String, A2aStorageError> {
    serde_json::to_string(manifest).map_err(|e| A2aStorageError::SerializationError(e.to_string()))
}

pub(crate) fn manifest_from_json(json: &str) -> Result<Vec<ManifestEntry>, A2aStorageError> {
    serde_json::from_str(json).map_err(|e| A2aStorageError::SerializationError(e.to_string()))
}

pub(crate) fn artifact_to_json(
    artifact: &turul_a2a_proto::Artifact,
) -> Result<String, A2aStorageError> {
    serde_json::to_string(artifact).map_err(|e| A2aStorageError::SerializationError(e.to_string()))
}

pub(crate) fn artifact_from_json(json: &str) -> Result<turul_a2a_proto::Artifact, A2aStorageError> {
    serde_json::from_str(json).map_err(|e| A2aStorageError::SerializationError(e.to_string()))
}

/// Reassemble a task's artifacts in manifest order from fetched records.
///
/// A manifest entry without a matching record is skipped: the only
/// producer of that state is an interrupted `delete_task`, so the task
/// is mid-deletion and a degraded read is the correct behavior.
pub(crate) fn rehydrate_in_manifest_order(
    manifest: &[ManifestEntry],
    mut records: std::collections::HashMap<String, turul_a2a_proto::Artifact>,
) -> Vec<turul_a2a_proto::Artifact> {
    let mut artifacts = Vec::with_capacity(manifest.len());
    for entry in manifest {
        match records.remove(&entry.artifact_id) {
            Some(a) => artifacts.push(a),
            None => tracing::warn!(
                artifact_id = %entry.artifact_id,
                "manifest references missing artifact record (interrupted delete?); skipping"
            ),
        }
    }
    artifacts
}

#[cfg(test)]
mod tests {
    use super::*;
    use turul_a2a_proto::pbjson_types;
    use turul_a2a_types::{Artifact, Part};

    fn proto_artifact(id: &str, text: &str) -> turul_a2a_proto::Artifact {
        Artifact::new(id, vec![Part::text(text)]).into_proto()
    }

    #[test]
    fn fingerprint_stable_across_encode_decode_round_trip() {
        let artifact = proto_artifact("art-1", "hello world");
        let before = artifact_fingerprint(&artifact).unwrap();
        let json = artifact_to_json(&artifact).unwrap();
        let decoded = artifact_from_json(&json).unwrap();
        let after = artifact_fingerprint(&decoded).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn fingerprint_independent_of_struct_map_insertion_order() {
        // Build two semantically equal artifacts whose metadata Structs
        // are populated in opposite insertion orders. HashMap-backed
        // Struct fields make encoding order unobservable; the canonical
        // fingerprint must be identical regardless.
        let mut forward = std::collections::HashMap::new();
        forward.insert("alpha".to_string(), pbjson_types::Value::from("1"));
        forward.insert("beta".to_string(), pbjson_types::Value::from("2"));
        forward.insert("gamma".to_string(), pbjson_types::Value::from("3"));

        let mut reverse = std::collections::HashMap::new();
        reverse.insert("gamma".to_string(), pbjson_types::Value::from("3"));
        reverse.insert("beta".to_string(), pbjson_types::Value::from("2"));
        reverse.insert("alpha".to_string(), pbjson_types::Value::from("1"));

        let mut a = proto_artifact("art-1", "same");
        a.metadata = Some(pbjson_types::Struct::from(forward));
        let mut b = proto_artifact("art-1", "same");
        b.metadata = Some(pbjson_types::Struct::from(reverse));

        assert_eq!(
            artifact_fingerprint(&a).unwrap(),
            artifact_fingerprint(&b).unwrap()
        );
    }

    #[test]
    fn fingerprint_differs_when_content_differs() {
        let a = proto_artifact("art-1", "one");
        let b = proto_artifact("art-1", "two");
        assert_ne!(
            artifact_fingerprint(&a).unwrap(),
            artifact_fingerprint(&b).unwrap()
        );
    }

    #[test]
    fn reconcile_writes_new_and_changed_deletes_missing() {
        let a1 = proto_artifact("a1", "unchanged");
        let a2_old = proto_artifact("a2", "old");
        let a2_new = proto_artifact("a2", "new");
        let a3 = proto_artifact("a3", "added");

        let stored = vec![
            ManifestEntry {
                artifact_id: "a1".into(),
                fingerprint: artifact_fingerprint(&a1).unwrap(),
            },
            ManifestEntry {
                artifact_id: "a2".into(),
                fingerprint: artifact_fingerprint(&a2_old).unwrap(),
            },
            ManifestEntry {
                artifact_id: "gone".into(),
                fingerprint: "x".into(),
            },
        ];

        let incoming = vec![a1.clone(), a2_new.clone(), a3.clone()];
        let plan = reconcile(&incoming, &stored).unwrap();

        let written: Vec<&str> = plan
            .writes
            .iter()
            .map(|(a, _)| a.artifact_id.as_str())
            .collect();
        assert_eq!(written, vec!["a2", "a3"]);
        assert_eq!(plan.deletes, vec!["gone".to_string()]);
        let manifest_ids: Vec<&str> = plan
            .manifest
            .iter()
            .map(|e| e.artifact_id.as_str())
            .collect();
        assert_eq!(manifest_ids, vec!["a1", "a2", "a3"]);
    }

    #[test]
    fn reconcile_unchanged_set_writes_and_deletes_nothing() {
        let a1 = proto_artifact("a1", "same");
        let stored = vec![ManifestEntry {
            artifact_id: "a1".into(),
            fingerprint: artifact_fingerprint(&a1).unwrap(),
        }];
        let plan = reconcile(std::slice::from_ref(&a1), &stored).unwrap();
        assert!(plan.writes.is_empty());
        assert!(plan.deletes.is_empty());
        assert_eq!(plan.manifest, stored);
    }

    #[test]
    fn rehydrate_preserves_manifest_order_and_skips_missing() {
        let a1 = proto_artifact("a1", "one");
        let a2 = proto_artifact("a2", "two");
        let manifest = vec![
            ManifestEntry {
                artifact_id: "a2".into(),
                fingerprint: "f2".into(),
            },
            ManifestEntry {
                artifact_id: "missing".into(),
                fingerprint: "fx".into(),
            },
            ManifestEntry {
                artifact_id: "a1".into(),
                fingerprint: "f1".into(),
            },
        ];
        let mut records = std::collections::HashMap::new();
        records.insert("a1".to_string(), a1);
        records.insert("a2".to_string(), a2);

        let out = rehydrate_in_manifest_order(&manifest, records);
        let ids: Vec<&str> = out.iter().map(|a| a.artifact_id.as_str()).collect();
        assert_eq!(ids, vec!["a2", "a1"]);
    }
}
