//! Projection views between the persona [`AgentDefinition`] shape and the
//! unified [`ManagedAgentRecord`] store shape (Phase 1A fold compatibility).

use super::types::{
    default_agent_parallelism, AgentDefinition, BackendKind, ManagedAgentRecord, RespondTo,
    DEFAULT_ACP_COMMAND, DEFAULT_AGENT_TURN_TIMEOUT_SECONDS,
};

impl AgentDefinition {
    /// Project this persona onto a key-less unified [`ManagedAgentRecord`]
    /// (Phase 1A store fold). Identity fields stay empty — keys are minted on
    /// first start. `AgentDefinition.id` becomes `slug`, preserving the 30175
    /// event coordinate (`d_tag = slug`) across the fold.
    pub fn into_agent_record(self) -> ManagedAgentRecord {
        ManagedAgentRecord {
            pubkey: String::new(),
            name: self.display_name.clone(),
            persona_id: None,
            private_key_nsec: String::new(),
            auth_tag: None,
            relay_url: String::new(),
            // Personas are global definitions; only minted instances are
            // community-bound (stamped at creation).
            community_relay_url: None,
            avatar_url: self.avatar_url,
            acp_command: DEFAULT_ACP_COMMAND.to_string(),
            agent_command: String::new(),
            agent_command_override: None,
            agent_args: Vec::new(),
            mcp_command: String::new(),
            turn_timeout_seconds: DEFAULT_AGENT_TURN_TIMEOUT_SECONDS,
            idle_timeout_seconds: None,
            max_turn_duration_seconds: None,
            parallelism: default_agent_parallelism(),
            system_prompt: (!self.system_prompt.is_empty()).then_some(self.system_prompt),
            model: self.model,
            provider: self.provider,
            persona_source_version: None,
            env_vars: self.env_vars,
            start_on_app_launch: false,
            auto_restart_on_config_change: true,
            runtime_pid: None,
            backend: BackendKind::default(),
            backend_agent_id: None,
            residual_deployments: Vec::new(),
            provider_policy_pending: false,
            provider_binary_path: None,
            waker_enabled: false,
            team_id: None,
            persona_team_dir: None,
            persona_name_in_team: None,
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_started_at: None,
            last_stopped_at: None,
            last_exit_code: None,
            last_error: None,
            last_error_code: None,
            respond_to: RespondTo::default(),
            respond_to_allowlist: Vec::new(),
            display_name: Some(self.display_name),
            description: self.description,
            slug: Some(self.id),
            runtime: self.runtime,
            name_pool: self.name_pool,
            is_builtin: self.is_builtin,
            is_active: self.is_active,
            // Catalog visibility is relay+owner scoped, not definition-global.
            shared: false,
            source_team: self.source_team,
            source_team_persona_slug: self.source_team_persona_slug,
            catalog_source: self.catalog_source,
            team_catalog_source: self.team_catalog_source,
            definition_respond_to: self.respond_to,
            definition_respond_to_allowlist: self.respond_to_allowlist,
            definition_parallelism: self.parallelism,
            relay_mesh: None,
            effort_level: None,
        }
    }
}

impl ManagedAgentRecord {
    /// Present a key-less definition record back in the legacy
    /// [`AgentDefinition`] shape — the compatibility view the persona command
    /// surface serves until Phase 1B unifies the UI. Inverse of
    /// [`AgentDefinition::into_agent_record`] for the fields personas carry.
    pub fn to_definition_view(&self) -> Option<AgentDefinition> {
        let slug = self.slug.clone()?;
        Some(AgentDefinition {
            id: slug,
            display_name: self
                .display_name
                .clone()
                .unwrap_or_else(|| self.name.clone()),
            avatar_url: self.avatar_url.clone(),
            description: self.description.clone(),
            system_prompt: self.system_prompt.clone().unwrap_or_default(),
            runtime: self.runtime.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            name_pool: self.name_pool.clone(),
            is_builtin: self.is_builtin,
            is_active: self.is_active,
            // Projected by `list_personas` from the active retention scope.
            shared: false,
            source_team: self.source_team.clone(),
            source_team_persona_slug: self.source_team_persona_slug.clone(),
            catalog_source: self.catalog_source.clone(),
            team_catalog_source: self.team_catalog_source.clone(),
            env_vars: self.env_vars.clone(),
            respond_to: self.definition_respond_to.clone(),
            respond_to_allowlist: self.definition_respond_to_allowlist.clone(),
            parallelism: self.definition_parallelism,
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        })
    }
}
