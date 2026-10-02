//! Het opgeslagen Go-schema; dit bevat geheimen en is geen publieke snapshot.
use crate::{Bytes, Timestamp, WireMap};
use alloc::string::String;

model! {
    /// Het opgeslagen `legacyLoginState`-contract.
    LegacyLoginState {
        /// `key` in de bestaande state.
        key: String => ("key", false),
        /// `files` in de bestaande state.
        files: WireMap<Bytes> => ("files", false),
        /// `updated_at` in de bestaande state.
        updated_at: Timestamp => ("updated_at", false),
    }
}

model! {
    /// Het opgeslagen `persistedState`-contract.
    PersistedState {
        /// `artifacts` in de bestaande state.
        artifacts: WireMap<crate::Artifact> => ("artifacts", false),
        /// `recordings` in de bestaande state.
        recordings: WireMap<crate::Recording> => ("recordings", false),
        /// `compositions` in de bestaande state.
        compositions: WireMap<crate::Composition> => ("compositions", false),
        /// `jobs` in de bestaande state.
        jobs: WireMap<crate::Job> => ("jobs", false),
        /// `job_attachments` in de bestaande state.
        job_attachments: WireMap<crate::JobAttachment> => ("job_attachments", false),
        /// `workflow_templates` in de bestaande state.
        workflow_templates: WireMap<crate::WorkflowTemplate> => ("workflow_templates", false),
        /// `phase_runs` in de bestaande state.
        phase_runs: WireMap<crate::PhaseRun> => ("phase_runs", false),
        /// `deliverables` in de bestaande state.
        deliverables: WireMap<crate::Deliverable> => ("deliverables", false),
        /// `deliverable_comments` in de bestaande state.
        deliverable_comments: WireMap<crate::DeliverableComment> => ("deliverable_comments", false),
        /// `code_review_revisions` in de bestaande state.
        code_review_revisions: WireMap<crate::CodeReviewRevision> => ("code_review_revisions", false),
        /// `code_review_comments` in de bestaande state.
        code_review_comments: WireMap<crate::CodeReviewComment> => ("code_review_comments", false),
        /// `workflow_questions` in de bestaande state.
        workflow_questions: WireMap<crate::WorkflowQuestion> => ("workflow_questions", false),
        /// `job_request_keys` in de bestaande state.
        job_request_keys: WireMap<String> => ("job_request_keys", false),
        /// `sessions` in de bestaande state.
        sessions: WireMap<crate::Session> => ("sessions", false),
        /// `activations` in de bestaande state.
        activations: WireMap<crate::Activation> => ("activations", false),
        /// `turns` in de bestaande state.
        turns: WireMap<crate::Turn> => ("turns", false),
        /// `checkpoints` in de bestaande state.
        checkpoints: WireMap<crate::Checkpoint> => ("checkpoints", false),
        /// `results` in de bestaande state.
        results: WireMap<crate::Result> => ("results", false),
        /// `clients` in de bestaande state.
        clients: WireMap<crate::Client> => ("clients", false),
        /// `mcp_servers` in de bestaande state.
        mcp_servers: WireMap<crate::MCPServer> => ("mcp_servers", false),
        /// `git_repositories` in de bestaande state.
        git_repositories: WireMap<crate::GitRepository> => ("git_repositories", false),
        /// `git_accounts` in de bestaande state.
        git_accounts: WireMap<crate::GitAccount> => ("git_accounts", false),
        /// `users` in de bestaande state.
        users: WireMap<crate::User> => ("users", false),
        /// `auth_sessions` in de bestaande state.
        auth_sessions: WireMap<crate::AuthSession> => ("auth_sessions", false),
        /// `git_oauth_configurations` in de bestaande state.
        git_oauth_configurations: WireMap<crate::GitOAuthConfiguration> => ("git_oauth_configurations", false),
        /// `logins` in de bestaande state.
        logins: WireMap<crate::Login> => ("logins", true),
        /// `workflow_tokens` in de bestaande state.
        workflow_tokens: WireMap<String> => ("workflow_tokens", true),
        /// `login_states` in de bestaande state.
        login_states: WireMap<LegacyLoginState> => ("login_states", true),
        /// Durable cleanup after graph deletions; absent in older databases.
        garbage_refs: crate::List<String> => ("garbage_refs", true),
        /// `worker_token` in de bestaande state.
        worker_token: String => ("worker_token", true),
    }
}

impl PersistedState {
    /// Initialiseert collecties en migreert oude Session-defaults zoals `ensureMaps`.
    pub fn normalize_loaded(&mut self) -> crate::Fallible {
        if self.artifacts.is_empty() {
            self.artifacts = WireMap::new();
        }
        if self.recordings.is_empty() {
            self.recordings = WireMap::new();
        }
        if self.compositions.is_empty() {
            self.compositions = WireMap::new();
        }
        if self.jobs.is_empty() {
            self.jobs = WireMap::new();
        }
        if self.job_attachments.is_empty() {
            self.job_attachments = WireMap::new();
        }
        if self.workflow_templates.is_empty() {
            self.workflow_templates = WireMap::new();
        }
        if self.phase_runs.is_empty() {
            self.phase_runs = WireMap::new();
        }
        if self.deliverables.is_empty() {
            self.deliverables = WireMap::new();
        }
        if self.deliverable_comments.is_empty() {
            self.deliverable_comments = WireMap::new();
        }
        if self.code_review_revisions.is_empty() {
            self.code_review_revisions = WireMap::new();
        }
        if self.code_review_comments.is_empty() {
            self.code_review_comments = WireMap::new();
        }
        if self.workflow_questions.is_empty() {
            self.workflow_questions = WireMap::new();
        }
        if self.job_request_keys.is_empty() {
            self.job_request_keys = WireMap::new();
        }
        if self.sessions.is_empty() {
            self.sessions = WireMap::new();
        }
        if self.activations.is_empty() {
            self.activations = WireMap::new();
        }
        if self.turns.is_empty() {
            self.turns = WireMap::new();
        }
        if self.checkpoints.is_empty() {
            self.checkpoints = WireMap::new();
        }
        if self.results.is_empty() {
            self.results = WireMap::new();
        }
        if self.clients.is_empty() {
            self.clients = WireMap::new();
        }
        if self.mcp_servers.is_empty() {
            self.mcp_servers = WireMap::new();
        }
        if self.git_repositories.is_empty() {
            self.git_repositories = WireMap::new();
        }
        if self.git_accounts.is_empty() {
            self.git_accounts = WireMap::new();
        }
        if self.users.is_empty() {
            self.users = WireMap::new();
        }
        if self.auth_sessions.is_empty() {
            self.auth_sessions = WireMap::new();
        }
        if self.git_oauth_configurations.is_empty() {
            self.git_oauth_configurations = WireMap::new();
        }
        if self.logins.is_empty() {
            self.logins = WireMap::new();
        }
        if self.workflow_tokens.is_empty() {
            self.workflow_tokens = WireMap::new();
        }
        for (_, session) in self.sessions.iter_mut() {
            if session.executor.is_empty() {
                session.executor = crate::try_string(crate::WORKFLOW_EXECUTOR_AGENT)?;
            }
            if let Some(job) = self
                .jobs
                .get(&session.job_id)
                .filter(|j| !j.branch.is_empty())
            {
                session.base_ref = crate::try_string(&job.branch)?;
                session.target_branch = crate::try_string(&job.branch)?;
            }
        }
        Ok(())
    }
}
