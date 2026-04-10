use serde::{Deserialize, Serialize};
use turul_a2a_proto as pb;

use crate::message::Part;

/// Ergonomic wrapper over proto `Artifact`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Artifact {
    pub(crate) inner: pb::Artifact,
}

impl Artifact {
    pub fn new(artifact_id: impl Into<String>, parts: Vec<Part>) -> Self {
        Self {
            inner: pb::Artifact {
                artifact_id: artifact_id.into(),
                name: String::new(),
                description: String::new(),
                parts: parts.into_iter().map(|p| p.into_proto()).collect(),
                metadata: None,
                extensions: vec![],
            },
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.inner.name = name.into();
        self
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.inner.description = description.into();
        self
    }

    pub fn artifact_id(&self) -> &str {
        &self.inner.artifact_id
    }

    pub fn as_proto(&self) -> &pb::Artifact {
        &self.inner
    }

    pub fn into_proto(self) -> pb::Artifact {
        self.inner
    }
}

impl From<pb::Artifact> for Artifact {
    fn from(inner: pb::Artifact) -> Self {
        Self { inner }
    }
}

impl From<Artifact> for pb::Artifact {
    fn from(artifact: Artifact) -> Self {
        artifact.inner
    }
}

impl Serialize for Artifact {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.inner.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Artifact {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        pb::Artifact::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_constructor() {
        let art = Artifact::new("art-1", vec![Part::text("result")])
            .with_name("Output")
            .with_description("The output");
        assert_eq!(art.artifact_id(), "art-1");
        assert_eq!(art.as_proto().name, "Output");
        assert_eq!(art.as_proto().description, "The output");
        assert_eq!(art.as_proto().parts.len(), 1);
    }

    #[test]
    fn artifact_serde_round_trip() {
        let art = Artifact::new("art-rt", vec![Part::text("data")]);
        let json = serde_json::to_string(&art).unwrap();
        let back: Artifact = serde_json::from_str(&json).unwrap();
        assert_eq!(back.artifact_id(), "art-rt");
    }
}
