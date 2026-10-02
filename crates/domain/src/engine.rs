//! Gegevenscontracten van de capsule-engine; transport en uitvoering leven elders.
use crate::{Bytes, List, Timestamp};
use alloc::string::String;

model! {
    /// Het veldcontract van `Execution`.
    Execution {
        /// `Output` in het bestaande JSON-contract.
        output: String => ("Output", false),
        /// `ExitCode` in het bestaande JSON-contract.
        exit_code: i64 => ("ExitCode", false),
    }
}

model! {
    /// Het veldcontract van `RecordingStack`.
    RecordingStack {
        /// `layers` in het bestaande JSON-contract.
        layers: List<String> => ("layers", false),
        /// `artifacts` in het bestaande JSON-contract.
        artifacts: List<crate::Artifact> => ("artifacts", false),
    }
}

model! {
    /// Het veldcontract van `LiveCapsules`.
    LiveCapsules {
        /// `compositions` in het bestaande JSON-contract.
        compositions: List<String> => ("compositions", false),
        /// `recordings` in het bestaande JSON-contract.
        recordings: List<String> => ("recordings", false),
    }
}

model! {
    /// Het veldcontract van `GitAuthentication`.
    GitAuthentication {
        /// `Username` in het bestaande JSON-contract.
        username: String => ("Username", false),
        /// `Password` in het bestaande JSON-contract.
        password: String => ("Password", false),
        /// `AuthorName` in het bestaande JSON-contract.
        author_name: String => ("AuthorName", false),
        /// `AuthorEmail` in het bestaande JSON-contract.
        author_email: String => ("AuthorEmail", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceFileChange`.
    WorkspaceFileChange {
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", false),
        /// `status` in het bestaande JSON-contract.
        status: String => ("status", false),
        /// `repository` in het bestaande JSON-contract.
        repository: String => ("repository", true),
        /// `folder` in het bestaande JSON-contract.
        folder: String => ("folder", true),
        /// `head` in het bestaande JSON-contract.
        head: String => ("head", true),
        /// `added` in het bestaande JSON-contract.
        added: i64 => ("added", false),
        /// `deleted` in het bestaande JSON-contract.
        deleted: i64 => ("deleted", false),
        /// `patch` in het bestaande JSON-contract.
        patch: String => ("patch", true),
        /// `binary` in het bestaande JSON-contract.
        binary: bool => ("binary", true),
        /// `truncated` in het bestaande JSON-contract.
        truncated: bool => ("truncated", true),
    }
}

model! {
    /// Het veldcontract van `WorkspaceChanges`.
    WorkspaceChanges {
        /// `branch` in het bestaande JSON-contract.
        branch: String => ("branch", true),
        /// `head` in het bestaande JSON-contract.
        head: String => ("head", true),
        /// `added` in het bestaande JSON-contract.
        added: i64 => ("added", false),
        /// `deleted` in het bestaande JSON-contract.
        deleted: i64 => ("deleted", false),
        /// `files` in het bestaande JSON-contract.
        files: List<WorkspaceFileChange> => ("files", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceAttachment`.
    WorkspaceAttachment {
        /// `SourcePath` in het bestaande JSON-contract.
        source_path: String => ("SourcePath", false),
        /// `Data` in het bestaande JSON-contract.
        data: Bytes => ("Data", false),
        /// `TargetPath` in het bestaande JSON-contract.
        target_path: String => ("TargetPath", false),
    }
}

model! {
    /// Het veldcontract van `TrackedSelection`.
    TrackedSelection {
        /// `paths` in het bestaande JSON-contract.
        paths: List<String> => ("paths", true),
        /// `excludes` in het bestaande JSON-contract.
        excludes: List<String> => ("excludes", true),
    }
}

model! {
    /// Het veldcontract van `WorkspaceComparison`.
    WorkspaceComparison {
        /// `Path` in het bestaande JSON-contract.
        path: String => ("Path", false),
        /// `BaseRef` in het bestaande JSON-contract.
        base_ref: String => ("BaseRef", false),
        /// `HeadRef` in het bestaande JSON-contract.
        head_ref: String => ("HeadRef", false),
        /// `CommitMessageMatch` in het bestaande JSON-contract.
        commit_message_match: String => ("CommitMessageMatch", false),
        /// `MergeCommit` in het bestaande JSON-contract.
        merge_commit: String => ("MergeCommit", false),
        /// `Authentication` in het bestaande JSON-contract.
        authentication: Option<GitAuthentication> => ("Authentication", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceAcceptance`.
    WorkspaceAcceptance {
        /// `Path` in het bestaande JSON-contract.
        path: String => ("Path", false),
        /// `BaseBranch` in het bestaande JSON-contract.
        base_branch: String => ("BaseBranch", false),
        /// `AllowChanges` in het bestaande JSON-contract.
        allow_changes: bool => ("AllowChanges", false),
        /// `CommitSubject` in het bestaande JSON-contract.
        commit_subject: String => ("CommitSubject", false),
        /// `CommitBody` in het bestaande JSON-contract.
        commit_body: String => ("CommitBody", false),
        /// `RemoteRef` in het bestaande JSON-contract.
        remote_ref: String => ("RemoteRef", false),
        /// `Authentication` in het bestaande JSON-contract.
        authentication: Option<GitAuthentication> => ("Authentication", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceMerge`.
    WorkspaceMerge {
        /// `Path` in het bestaande JSON-contract.
        path: String => ("Path", false),
        /// `SourceRef` in het bestaande JSON-contract.
        source_ref: String => ("SourceRef", false),
        /// `TargetRef` in het bestaande JSON-contract.
        target_ref: String => ("TargetRef", false),
        /// `CommitSubject` in het bestaande JSON-contract.
        commit_subject: String => ("CommitSubject", false),
        /// `CommitBody` in het bestaande JSON-contract.
        commit_body: String => ("CommitBody", false),
        /// `Authentication` in het bestaande JSON-contract.
        authentication: Option<GitAuthentication> => ("Authentication", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceMergeResult`.
    WorkspaceMergeResult {
        /// `Head` in het bestaande JSON-contract.
        head: String => ("Head", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceSync`.
    WorkspaceSync {
        /// `Path` in het bestaande JSON-contract.
        path: String => ("Path", false),
        /// `SessionRef` in het bestaande JSON-contract.
        session_ref: String => ("SessionRef", false),
        /// `Authentication` in het bestaande JSON-contract.
        authentication: Option<GitAuthentication> => ("Authentication", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceSyncResult`.
    WorkspaceSyncResult {
        /// `Head` in het bestaande JSON-contract.
        head: String => ("Head", false),
        /// `Committed` in het bestaande JSON-contract.
        committed: bool => ("Committed", false),
        /// `Pushed` in het bestaande JSON-contract.
        pushed: bool => ("Pushed", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceEntry`.
    WorkspaceEntry {
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", false),
        /// `size` in het bestaande JSON-contract.
        size: i64 => ("size", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceTree`.
    WorkspaceTree {
        /// `ref` in het bestaande JSON-contract.
        r#ref: String => ("ref", false),
        /// `entries` in het bestaande JSON-contract.
        entries: List<WorkspaceEntry> => ("entries", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceFile`.
    WorkspaceFile {
        /// `ref` in het bestaande JSON-contract.
        r#ref: String => ("ref", false),
        /// `path` in het bestaande JSON-contract.
        path: String => ("path", false),
        /// `size` in het bestaande JSON-contract.
        size: i64 => ("size", false),
        /// `content` in het bestaande JSON-contract.
        content: String => ("content", false),
        /// `binary` in het bestaande JSON-contract.
        binary: bool => ("binary", false),
        /// `truncated` in het bestaande JSON-contract.
        truncated: bool => ("truncated", false),
    }
}

model! {
    /// Het veldcontract van `RepositoryBrowse`.
    RepositoryBrowse {
        /// `RemoteURL` in het bestaande JSON-contract.
        remote_url: String => ("RemoteURL", false),
        /// `CacheKey` in het bestaande JSON-contract.
        cache_key: String => ("CacheKey", false),
        /// `Mode` in het bestaande JSON-contract.
        mode: String => ("Mode", false),
        /// `Ref` in het bestaande JSON-contract.
        r#ref: String => ("Ref", false),
        /// `Path` in het bestaande JSON-contract.
        path: String => ("Path", false),
        /// `Authentication` in het bestaande JSON-contract.
        authentication: Option<GitAuthentication> => ("Authentication", false),
    }
}

model! {
    /// Het veldcontract van `RepositoryRef`.
    RepositoryRef {
        /// `name` in het bestaande JSON-contract.
        name: String => ("name", false),
        /// `committed_at` in het bestaande JSON-contract.
        committed_at: Timestamp => ("committed_at", false),
    }
}

model! {
    /// Het veldcontract van `RepositoryBrowseResult`.
    RepositoryBrowseResult {
        /// `refs` in het bestaande JSON-contract.
        refs: List<RepositoryRef> => ("refs", true),
        /// `tree` in het bestaande JSON-contract.
        tree: Option<WorkspaceTree> => ("tree", true),
        /// `file` in het bestaande JSON-contract.
        file: Option<WorkspaceFile> => ("file", true),
    }
}

model! {
    /// Het veldcontract van `RepositoryComparison`.
    RepositoryComparison {
        /// `RemoteURL` in het bestaande JSON-contract.
        remote_url: String => ("RemoteURL", false),
        /// `CacheKey` in het bestaande JSON-contract.
        cache_key: String => ("CacheKey", false),
        /// `Comparison` in het bestaande JSON-contract.
        comparison: WorkspaceComparison => ("Comparison", false),
    }
}

model! {
    /// Het veldcontract van `RepositoryAcceptance`.
    RepositoryAcceptance {
        /// `RemoteURL` in het bestaande JSON-contract.
        remote_url: String => ("RemoteURL", false),
        /// `CacheKey` in het bestaande JSON-contract.
        cache_key: String => ("CacheKey", false),
        /// `SessionRef` in het bestaande JSON-contract.
        session_ref: String => ("SessionRef", false),
        /// `JobRef` in het bestaande JSON-contract.
        job_ref: String => ("JobRef", false),
        /// `BootstrapRef` in het bestaande JSON-contract.
        bootstrap_ref: String => ("BootstrapRef", false),
        /// `AllowChanges` in het bestaande JSON-contract.
        allow_changes: bool => ("AllowChanges", false),
        /// `CommitSubject` in het bestaande JSON-contract.
        commit_subject: String => ("CommitSubject", false),
        /// `CommitBody` in het bestaande JSON-contract.
        commit_body: String => ("CommitBody", false),
        /// `Authentication` in het bestaande JSON-contract.
        authentication: Option<GitAuthentication> => ("Authentication", false),
    }
}

model! {
    /// Het veldcontract van `RepositoryMerge`.
    RepositoryMerge {
        /// `RemoteURL` in het bestaande JSON-contract.
        remote_url: String => ("RemoteURL", false),
        /// `CacheKey` in het bestaande JSON-contract.
        cache_key: String => ("CacheKey", false),
        /// `SourceRef` in het bestaande JSON-contract.
        source_ref: String => ("SourceRef", false),
        /// `TargetRef` in het bestaande JSON-contract.
        target_ref: String => ("TargetRef", false),
        /// `CommitSubject` in het bestaande JSON-contract.
        commit_subject: String => ("CommitSubject", false),
        /// `CommitBody` in het bestaande JSON-contract.
        commit_body: String => ("CommitBody", false),
        /// `Authentication` in het bestaande JSON-contract.
        authentication: Option<GitAuthentication> => ("Authentication", false),
    }
}

model! {
    /// Het veldcontract van `WorkspaceAcceptanceResult`.
    WorkspaceAcceptanceResult {
        /// `Head` in het bestaande JSON-contract.
        head: String => ("Head", false),
        /// `Committed` in het bestaande JSON-contract.
        committed: bool => ("Committed", false),
    }
}
