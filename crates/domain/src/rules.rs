//! Zuivere regels voor domeinobjecten.

use crate::*;
use alloc::string::String;

/// Het pad van documenten uit een eerdere Job.
pub const PREVIOUS_JOB_DELIVERABLE_DIRECTORY: &str = "/root/deliverables/vorige-job";
/// Levensduur van een ingelogde preview, in seconden.
pub const PREVIEW_TOKEN_TTL: u64 = 15 * 60;
/// Levensduur van een openbare deellink, in seconden.
pub const SHARE_TOKEN_TTL: u64 = 60 * 60;
/// De soorten die een Template mag vragen.
pub const DELIVERABLE_KINDS: &[&str] = &[
    DELIVERABLE_KIND_MARKDOWN,
    DELIVERABLE_KIND_PDF,
    DELIVERABLE_KIND_IMAGE,
    DELIVERABLE_KIND_FOLDER,
    DELIVERABLE_KIND_FILE,
];

/// Of een oplevering als bundel wordt vervoerd.
pub fn deliverable_is_bundle(kind: &str) -> bool {
    !kind.is_empty() && kind != DELIVERABLE_KIND_MARKDOWN
}

/// De ASCII-bestandsnaam die bij een oplevering hoort.
pub fn deliverable_slug(name: &str) -> Fallible<String> {
    let mut out = String::new();
    out.try_reserve(name.len())
        .map_err(|_| Error::OutOfMemory)?;
    let mut dash = false;
    for c in name.trim().chars() {
        // Go gebruikt eenvoudige Unicode-lowercase, zonder expansie.
        let c = c.to_lowercase().next().unwrap_or(c);
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        return try_string("deliverable");
    }
    Ok(out)
}

impl Deliverable {
    /// Het pad van deze revisie in de capsule.
    pub fn capsule_path(&self) -> Fallible<String> {
        self.capsule_path_in(DELIVERABLE_DIRECTORY)
    }
    /// Het pad onder een gekozen opleveringsmap.
    pub fn capsule_path_in(&self, directory: &str) -> Fallible<String> {
        let mut path = try_string(directory)?;
        try_push_str(&mut path, "/")?;
        try_push_str(&mut path, &deliverable_slug(&self.name)?)?;
        let extension = match &self.bundle {
            Some(bundle) if deliverable_is_bundle(&self.kind) => {
                if bundle.folder {
                    ""
                } else {
                    let file = bundle.entry.rsplit('/').next().unwrap_or_default();
                    file.rfind('.')
                        .and_then(|i| file.get(i..))
                        .unwrap_or_default()
                }
            }
            _ => ".md",
        };
        for c in extension.chars().flat_map(char::to_lowercase) {
            let mut buf = [0; 4];
            try_push_str(&mut path, c.encode_utf8(&mut buf))?;
        }
        Ok(path)
    }
}

/// De checkout-map bij een workspacepad.
pub fn workspace_directory(path: &str) -> Fallible<String> {
    let mut out = try_string(WORKSPACE_ROOT)?;
    if !path.is_empty() {
        try_push_str(&mut out, "/")?;
        try_push_str(&mut out, path)?;
    }
    Ok(out)
}
impl JobRepository {
    /// De checkout-map in de capsule.
    pub fn directory(&self) -> Fallible<String> {
        workspace_directory(&self.path)
    }
}
impl GitWorkspace {
    /// De checkout-map in de capsule.
    pub fn directory(&self) -> Fallible<String> {
        workspace_directory(&self.path)
    }
    /// Of deze repository gewijzigd mag worden.
    pub fn changes(&self) -> bool {
        self.mode != REPOSITORY_MODE_REFERENCE
    }
}
impl Composition {
    /// Alle repositories, met de opgeslagen enkelvoudige vorm als terugval.
    pub fn git_workspaces(&self) -> &[GitWorkspace] {
        if !self.workspaces.is_empty() {
            &self.workspaces
        } else {
            self.git.as_slice()
        }
    }
    /// Alleen de repositories die deze Session mag wijzigen.
    pub fn changed_workspaces(&self) -> impl Iterator<Item = &GitWorkspace> {
        self.git_workspaces().iter().filter(|w| w.changes())
    }
}
impl Job {
    /// De verantwoordelijke voor de volgende stap.
    pub fn worker(&self) -> &str {
        if self.assignee.is_empty() {
            &self.owner
        } else {
            &self.assignee
        }
    }
    /// Iedere ingelogde collega mag de Job bedienen.
    pub fn allows_operator(&self, operator: &str) -> bool {
        !operator.is_empty()
    }
    /// Alle repositories, inclusief de terugval voor bestaande Jobs.
    pub fn job_repositories(&self) -> Fallible<List<JobRepository>> {
        if !self.repositories.is_empty() {
            return self.repositories.try_clone();
        }
        let mut out = List::default();
        if !self.git_repository_id.is_empty() {
            out.push(JobRepository {
                repository_id: try_string(&self.git_repository_id)?,
                name: try_string(&self.git_repository_name)?,
                remote_url: try_string(&self.git_remote_url)?,
                provider: try_string(&self.git_provider)?,
                credential_scope: try_string(&self.git_credential_scope)?,
                base_ref: try_string(&self.base_ref)?,
                mode: try_string(REPOSITORY_MODE_CHANGE)?,
                ..Default::default()
            })?;
        }
        Ok(out)
    }
    /// Alleen de repositories waarvoor de Job een branch beheert.
    pub fn changed_repositories(&self) -> Fallible<List<JobRepository>> {
        let mut out = List::default();
        for repository in self
            .job_repositories()?
            .iter()
            .filter(|r| r.mode != REPOSITORY_MODE_REFERENCE)
        {
            out.push(repository.try_clone()?)?;
        }
        Ok(out)
    }
}

/// Een brainstorm maakt alleen de goal scherp en start daarna de Templateflow.
pub fn brainstorm_phase() -> Fallible<WorkflowPhase> {
    Ok(WorkflowPhase {
        id: try_string(BRAINSTORM_PHASE_ID)?,
        name: try_string("Brainstorm")?,
        executor: try_string(WORKFLOW_EXECUTOR_AGENT)?,
        instructions: try_string(
            "Dit is een brainstorm, geen uitvoering. Alles van deze Job staat al vast (repository, omgeving, Template); alleen de goal nog niet. Verken de repository om te zien wat er al is, denk mee, leg mogelijkheden met voor- en nadelen naast elkaar en stel gerichte vragen in de chat. Bouw en wijzig niets. Werk toe naar één scherpe goal: wat er klaar moet zijn en waaraan je dat ziet. Zodra de gebruiker het eens is, leg je die goal vast met start_process; daarmee begint de gewone flow van de Template.",
        )?,
        accept: WorkflowTransition {
            target: try_string(WORKFLOW_TARGET_NEXT)?,
            ..Default::default()
        },
        reject: WorkflowTransition {
            target: try_string(WORKFLOW_TARGET_SELF)?,
            ..Default::default()
        },
        ..Default::default()
    })
}

/// Een slash aan het eind duidt een map aan.
pub fn tracked_folder(path: &str) -> bool {
    path.ends_with('/')
}
/// Of een bestand onder een gekozen pad en buiten de uitsluitingen valt.
pub fn tracked_covers<S: AsRef<str>>(path: &str, tracked: &[S], excludes: &[S]) -> bool {
    let covers = |candidate: &S| {
        let c = candidate.as_ref();
        c == path || (tracked_folder(c) && path.starts_with(c))
    };
    tracked.iter().any(covers) && !excludes.iter().any(covers)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_and_extensions() {
        for (name, want) in [
            (" Hé, WERELD! ", "h-wereld"),
            ("---", "deliverable"),
            ("A__B", "a-b"),
            ("K", "k"),
        ] {
            assert_eq!(deliverable_slug(name).unwrap(), want);
        }
        let mut d = Deliverable {
            name: try_string("Het plan").unwrap(),
            ..Default::default()
        };
        assert_eq!(d.capsule_path().unwrap(), "/root/deliverables/het-plan.md");
        d.kind = try_string("pdf").unwrap();
        d.bundle = Some(DeliverableBundle {
            entry: try_string("folder.v1/PLAN.PDF").unwrap(),
            ..Default::default()
        });
        assert_eq!(
            d.capsule_path_in("/previous").unwrap(),
            "/previous/het-plan.pdf"
        );
        d.bundle.as_mut().unwrap().folder = true;
        assert_eq!(d.capsule_path().unwrap(), "/root/deliverables/het-plan");
    }
    #[test]
    fn tracked_boundaries() {
        assert!(tracked_covers(
            "/root/.codex/auth.json",
            &["/root/.codex/"],
            &["/root/.codex/cache/"]
        ));
        assert!(!tracked_covers(
            "/root/.codex/cache/x",
            &["/root/.codex/"],
            &["/root/.codex/cache/"]
        ));
        assert!(!tracked_covers(
            "/root/.codex2/auth.json",
            &["/root/.codex/"],
            &[]
        ));
        assert!(!tracked_covers("/root/file/child", &["/root/file"], &[]));
    }
    #[test]
    fn legacy_repository_and_worker() {
        let job = Job {
            owner: try_string("owner").unwrap(),
            assignee: try_string("worker").unwrap(),
            git_repository_id: try_string("repo").unwrap(),
            ..Default::default()
        };
        assert_eq!(job.worker(), "worker");
        assert!(job.allows_operator("colleague"));
        assert!(!job.allows_operator(""));
        let repos = job.job_repositories().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].mode, "change");
        assert_eq!(repos[0].directory().unwrap(), "/workspace");
    }
}
