use super::ProductConfig;

impl PartialEq for ProductConfig {
    fn eq(&self, other: &Self) -> bool {
        self.templates == other.templates
            && self.root_agent == other.root_agent
            && self.root_composition == other.root_composition
            && self.working_set == other.working_set
            && self.subagents == other.subagents
            && self.occupancy == other.occupancy
            && self.max_depth == other.max_depth
            && self.max_children == other.max_children
            && self.workspace_idle_seconds == other.workspace_idle_seconds
            && self.workspace_budget_mib == other.workspace_budget_mib
            && self.child_retention_days == other.child_retention_days
            && self.child_retention_count == other.child_retention_count
            && self.max_nodes == other.max_nodes
            && self.max_agents_per_cell == other.max_agents_per_cell
            && self.max_attempts == other.max_attempts
            && self.transient_resumes == other.transient_resumes
            && self.requests_per_account == other.requests_per_account
            && self.require_review_evidence == other.require_review_evidence
            && self.auto_continue == other.auto_continue
            && self.jobs_foreground_seconds == other.jobs_foreground_seconds
            && self.jobs_max_runtime_hours == other.jobs_max_runtime_hours
            && self.api_full_window == other.api_full_window
            && self.max_model_rounds == other.max_model_rounds
            && self.tui_mouse == other.tui_mouse
            && self.scroll_lines == other.scroll_lines
            && self.show_thinking == other.show_thinking
            && self.group_tool_calls == other.group_tool_calls
            && self.reduced_motion == other.reduced_motion
            && self.reload_requests == other.reload_requests
            && self.theme == other.theme
            && self.compaction_enabled == other.compaction_enabled
            && self.skills_enabled == other.skills_enabled
            && self.telemetry_max_bytes == other.telemetry_max_bytes
            && self.telemetry_max_age_days == other.telemetry_max_age_days
            && self.mcp_client_id_metadata_url == other.mcp_client_id_metadata_url
            && self.tool_sections == other.tool_sections
            && self.resume_after_loss == other.resume_after_loss
            && self.runners == other.runners
            && self.web_allow == other.web_allow
            && self.web_origin == other.web_origin
            && self.web_accounts_read == other.web_accounts_read
            && self.web_credentials_write == other.web_credentials_write
            && self.web_config_read == other.web_config_read
            && self.web_config_write == other.web_config_write
    }
}

impl Eq for ProductConfig {}
