//! Ergonomic builders for AgentCard and AgentSkill.
//!
//! Builds `turul_a2a_proto::AgentCard` directly — no parallel wrapper type.
//! Validates REQUIRED fields per proto field_behavior at `build()` time.

use crate::error::A2aError;

/// Builder for `turul_a2a_proto::AgentSkill`.
pub struct AgentSkillBuilder {
    id: String,
    name: String,
    description: String,
    tags: Vec<String>,
    examples: Vec<String>,
    input_modes: Vec<String>,
    output_modes: Vec<String>,
    security_requirements: Vec<turul_a2a_proto::SecurityRequirement>,
}

impl AgentSkillBuilder {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            tags: vec![],
            examples: vec![],
            input_modes: vec![],
            output_modes: vec![],
            security_requirements: vec![],
        }
    }

    pub fn tags(mut self, tags: Vec<impl Into<String>>) -> Self {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    pub fn examples(mut self, examples: Vec<impl Into<String>>) -> Self {
        self.examples = examples.into_iter().map(Into::into).collect();
        self
    }

    pub fn input_modes(mut self, modes: Vec<impl Into<String>>) -> Self {
        self.input_modes = modes.into_iter().map(Into::into).collect();
        self
    }

    pub fn output_modes(mut self, modes: Vec<impl Into<String>>) -> Self {
        self.output_modes = modes.into_iter().map(Into::into).collect();
        self
    }

    /// Advertise skill-level security requirements on the built skill.
    ///
    /// turul-a2a does NOT enforce these at request time in 0.1.x — they
    /// are published as-is for discovery and out-of-band tooling. Any
    /// runtime gatekeeping for a specific skill is the adopter's
    /// responsibility inside `AgentExecutor`. See ADR-015 for the
    /// rationale and the post-merge truthfulness invariant validated at
    /// server build.
    pub fn security_requirements(
        mut self,
        requirements: Vec<turul_a2a_proto::SecurityRequirement>,
    ) -> Self {
        self.security_requirements = requirements;
        self
    }

    pub fn build(self) -> turul_a2a_proto::AgentSkill {
        turul_a2a_proto::AgentSkill {
            id: self.id,
            name: self.name,
            description: self.description,
            tags: self.tags,
            examples: self.examples,
            input_modes: self.input_modes,
            output_modes: self.output_modes,
            security_requirements: self.security_requirements,
        }
    }
}

/// Builder for `turul_a2a_proto::AgentCard`.
///
/// Validates REQUIRED fields per proto field_behavior at `build()` time.
/// `skills` is allowed to be empty — the builder does not invent a non-empty
/// invariant beyond what the proto requires.
pub struct AgentCardBuilder {
    name: String,
    description: Option<String>,
    version: String,
    interfaces: Vec<turul_a2a_proto::AgentInterface>,
    provider: Option<turul_a2a_proto::AgentProvider>,
    documentation_url: Option<String>,
    streaming: Option<bool>,
    push_notifications: Option<bool>,
    extended_agent_card: Option<bool>,
    default_input_modes: Vec<String>,
    default_output_modes: Vec<String>,
    skills: Vec<turul_a2a_proto::AgentSkill>,
    icon_url: Option<String>,
    security_schemes: std::collections::HashMap<String, turul_a2a_proto::SecurityScheme>,
    security_requirements: Vec<turul_a2a_proto::SecurityRequirement>,
}

impl AgentCardBuilder {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            version: version.into(),
            interfaces: vec![],
            provider: None,
            documentation_url: None,
            streaming: None,
            push_notifications: None,
            extended_agent_card: None,
            default_input_modes: vec![],
            default_output_modes: vec![],
            skills: vec![],
            icon_url: None,
            security_schemes: std::collections::HashMap::new(),
            security_requirements: vec![],
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add a supported interface (url, protocol_binding, protocol_version).
    pub fn url(
        mut self,
        url: impl Into<String>,
        protocol_binding: impl Into<String>,
        protocol_version: impl Into<String>,
    ) -> Self {
        self.interfaces.push(turul_a2a_proto::AgentInterface {
            url: url.into(),
            protocol_binding: protocol_binding.into(),
            tenant: String::new(),
            protocol_version: protocol_version.into(),
        });
        self
    }

    pub fn provider(mut self, organization: impl Into<String>, url: impl Into<String>) -> Self {
        self.provider = Some(turul_a2a_proto::AgentProvider {
            organization: organization.into(),
            url: url.into(),
        });
        self
    }

    pub fn documentation_url(mut self, url: impl Into<String>) -> Self {
        self.documentation_url = Some(url.into());
        self
    }

    pub fn streaming(mut self, enabled: bool) -> Self {
        self.streaming = Some(enabled);
        self
    }

    pub fn push_notifications(mut self, enabled: bool) -> Self {
        self.push_notifications = Some(enabled);
        self
    }

    pub fn extended_agent_card(mut self, enabled: bool) -> Self {
        self.extended_agent_card = Some(enabled);
        self
    }

    pub fn default_input_modes(mut self, modes: Vec<impl Into<String>>) -> Self {
        self.default_input_modes = modes.into_iter().map(Into::into).collect();
        self
    }

    pub fn default_output_modes(mut self, modes: Vec<impl Into<String>>) -> Self {
        self.default_output_modes = modes.into_iter().map(Into::into).collect();
        self
    }

    pub fn skill(mut self, skill: turul_a2a_proto::AgentSkill) -> Self {
        self.skills.push(skill);
        self
    }

    pub fn icon_url(mut self, url: impl Into<String>) -> Self {
        self.icon_url = Some(url.into());
        self
    }

    /// Declare a named security scheme on the agent card.
    ///
    /// Adopter-supplied schemes are merged with any schemes contributed
    /// by installed middleware at server build; collisions between an
    /// adopter scheme and a middleware scheme with the same name are
    /// rejected at build time. See ADR-015.
    pub fn security_scheme(
        mut self,
        name: impl Into<String>,
        scheme: turul_a2a_proto::SecurityScheme,
    ) -> Self {
        self.security_schemes.insert(name.into(), scheme);
        self
    }

    /// Advertise an agent-level security requirement not derived from
    /// middleware (e.g., mTLS enforced at a reverse proxy).
    ///
    /// turul-a2a does not install a runtime gatekeeper for
    /// adopter-supplied agent-level requirements; they are declarative
    /// only. Authorization at the framework layer is governed solely by
    /// installed middleware. See ADR-015.
    pub fn security_requirement(
        mut self,
        requirement: turul_a2a_proto::SecurityRequirement,
    ) -> Self {
        self.security_requirements.push(requirement);
        self
    }

    /// Build and validate the AgentCard.
    ///
    /// Validates REQUIRED fields per proto field_behavior:
    /// name, description, version, supported_interfaces, capabilities,
    /// default_input_modes, default_output_modes.
    ///
    /// `skills` may be empty — no non-empty invariant is imposed.
    pub fn build(self) -> Result<turul_a2a_proto::AgentCard, A2aError> {
        let description = self.description.ok_or(A2aError::InvalidRequest {
            message: "AgentCard requires description".into(),
        })?;

        if self.interfaces.is_empty() {
            return Err(A2aError::InvalidRequest {
                message: "AgentCard requires at least one supported_interface".into(),
            });
        }

        if self.default_input_modes.is_empty() {
            return Err(A2aError::InvalidRequest {
                message: "AgentCard requires default_input_modes".into(),
            });
        }

        if self.default_output_modes.is_empty() {
            return Err(A2aError::InvalidRequest {
                message: "AgentCard requires default_output_modes".into(),
            });
        }

        Ok(turul_a2a_proto::AgentCard {
            name: self.name,
            description,
            supported_interfaces: self.interfaces,
            provider: self.provider,
            version: self.version,
            documentation_url: self.documentation_url,
            capabilities: Some(turul_a2a_proto::AgentCapabilities {
                streaming: self.streaming,
                push_notifications: self.push_notifications,
                extensions: vec![],
                extended_agent_card: self.extended_agent_card,
            }),
            security_schemes: self.security_schemes,
            security_requirements: self.security_requirements,
            default_input_modes: self.default_input_modes,
            default_output_modes: self.default_output_modes,
            skills: self.skills,
            signatures: vec![],
            icon_url: self.icon_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_valid_card() {
        let card = AgentCardBuilder::new("test", "1.0.0")
            .description("A test agent")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap();

        assert_eq!(card.name, "test");
        assert_eq!(card.description, "A test agent");
        assert_eq!(card.version, "1.0.0");
        assert_eq!(card.supported_interfaces.len(), 1);
        assert!(card.skills.is_empty(), "Empty skills should be allowed");
    }

    #[test]
    fn missing_description_fails() {
        let result = AgentCardBuilder::new("test", "1.0.0")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn missing_interfaces_fails() {
        let result = AgentCardBuilder::new("test", "1.0.0")
            .description("test")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn missing_input_modes_fails() {
        let result = AgentCardBuilder::new("test", "1.0.0")
            .description("test")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_output_modes(vec!["text/plain"])
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn missing_output_modes_fails() {
        let result = AgentCardBuilder::new("test", "1.0.0")
            .description("test")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn empty_skills_allowed() {
        let card = AgentCardBuilder::new("test", "1.0.0")
            .description("test")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap();
        assert!(card.skills.is_empty());
    }

    #[test]
    fn with_skills() {
        let skill = AgentSkillBuilder::new("echo", "Echo", "Echoes input")
            .tags(vec!["echo", "test"])
            .examples(vec!["Say hello"])
            .build();

        let card = AgentCardBuilder::new("test", "1.0.0")
            .description("test")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .skill(skill)
            .build()
            .unwrap();

        assert_eq!(card.skills.len(), 1);
        assert_eq!(card.skills[0].id, "echo");
        assert_eq!(card.skills[0].tags, vec!["echo", "test"]);
    }

    #[test]
    fn full_card_with_all_options() {
        let card = AgentCardBuilder::new("full-agent", "2.0.0")
            .description("A fully configured agent")
            .url("https://agent.example.com", "JSONRPC", "1.0")
            .url("https://agent.example.com/grpc", "GRPC", "1.0")
            .provider("Aussie Robots", "https://aussierobots.com.au")
            .documentation_url("https://docs.example.com")
            .streaming(true)
            .push_notifications(false)
            .extended_agent_card(true)
            .default_input_modes(vec!["text/plain", "application/json"])
            .default_output_modes(vec!["text/plain"])
            .icon_url("https://example.com/icon.png")
            .skill(
                AgentSkillBuilder::new("search", "Search", "Searches things")
                    .tags(vec!["search"])
                    .input_modes(vec!["text/plain"])
                    .build(),
            )
            .build()
            .unwrap();

        assert_eq!(card.supported_interfaces.len(), 2);
        assert!(card.provider.is_some());
        assert_eq!(card.capabilities.as_ref().unwrap().streaming, Some(true));
        assert_eq!(
            card.icon_url.as_deref(),
            Some("https://example.com/icon.png")
        );
    }

    #[test]
    fn card_serializes_correctly() {
        let card = AgentCardBuilder::new("json-test", "1.0.0")
            .description("test")
            .url("http://localhost", "JSONRPC", "1.0")
            .default_input_modes(vec!["text/plain"])
            .default_output_modes(vec!["text/plain"])
            .build()
            .unwrap();

        let json = serde_json::to_value(&card).unwrap();
        assert_eq!(json["name"], "json-test");
        assert!(json.get("defaultInputModes").is_some());
        assert!(json.get("defaultOutputModes").is_some());
    }

    // ADR-015 §4.1 test 1: builder does NOT reject a skill-level
    // `SecurityRequirement` whose scheme name is absent from the
    // adopter-supplied `security_schemes` map on the agent card.
    //
    // Rationale: the missing scheme may be contributed by installed
    // middleware at server-build time, so validating at builder time
    // would false-reject legitimate configurations. Cross-field
    // consistency is enforced post-merge in `A2aServerBuilder::build()`
    // per ADR-015 §2.3, not here.
    #[test]
    fn builder_permits_requirement_with_scheme_to_be_merged_later() {
        let mut bearer_ref = std::collections::HashMap::new();
        bearer_ref.insert(
            "bearer".to_string(),
            turul_a2a_proto::StringList { list: vec![] },
        );
        let req = turul_a2a_proto::SecurityRequirement {
            schemes: bearer_ref,
        };

        // AgentSkillBuilder::security_requirements does not exist on
        // current main — this call is the load-bearing red-phase
        // compile-error assertion for ADR-015. After the ADR §5.5
        // implementation lands, the builder exposes this setter and
        // the body below becomes the regression assertion that the
        // requirement survives `build()` without a cross-field check.
        let skill = AgentSkillBuilder::new("echo", "Echo", "Echoes input")
            .tags(vec!["echo"])
            .security_requirements(vec![req])
            .build();

        assert_eq!(skill.security_requirements.len(), 1);
        let only = &skill.security_requirements[0];
        assert!(
            only.schemes.contains_key("bearer"),
            "requirement must preserve the scheme name the adopter set"
        );
    }
}
