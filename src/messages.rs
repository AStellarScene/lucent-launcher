//! Typed events exchanged between worker threads and the GTK application.

use crate::{DiscoveryCardData, DiscoveryKind};

/// Event envelope consumed by the GTK main-loop dispatcher.
#[derive(Debug)]
pub(crate) enum AppEvent {
    /// Shared presentation output from a background operation.
    Log(String),
    /// Shared presentation status from a background operation.
    StatusUpdate(String),
    /// Ask the UI to open a URL with the platform default handler.
    OpenUrl(String),
    Versions(VersionsEvent),
    Auth(AuthEvent),
    Launch(LaunchEvent),
    Discovery(DiscoveryEvent),
}

#[derive(Debug)]
pub(crate) enum VersionsEvent {
    Loaded(Vec<String>),
    Failed(String),
}

#[derive(Debug)]
pub(crate) enum AuthEvent {
    MicrosoftSuccess {
        username: String,
        uuid: String,
        access_token: String,
        refresh_token: String,
    },
    MicrosoftFailed(String),
}

#[derive(Debug)]
pub(crate) struct LaunchProgress {
    pub(crate) profile_id: String,
    pub(crate) stage: String,
    pub(crate) completed_tasks: u64,
    pub(crate) total_tasks: Option<u64>,
    pub(crate) current_task: Option<String>,
    pub(crate) bytes_received: u64,
    pub(crate) total_bytes: Option<u64>,
}

#[derive(Debug)]
pub(crate) enum LaunchEvent {
    Progress(LaunchProgress),
    /// Minecraft emitted a client-initialization log marker after the process started.
    Ready {
        profile_id: String,
    },
    Finished {
        profile_id: String,
    },
    Failed {
        profile_id: String,
        error: String,
    },
}

#[derive(Debug)]
pub(crate) enum DiscoveryEvent {
    ActionFailed {
        kind: DiscoveryKind,
        error: String,
    },
    SearchResults {
        kind: DiscoveryKind,
        query: String,
        results: Vec<DiscoveryCardData>,
    },
    SearchFailed {
        kind: DiscoveryKind,
        error: String,
    },
    InstallFinished {
        kind: DiscoveryKind,
        title: String,
        target_path: String,
    },
    InstalledChanged(DiscoveryKind),
}

impl AppEvent {
    pub(crate) fn versions_loaded(versions: Vec<String>) -> Self {
        Self::Versions(VersionsEvent::Loaded(versions))
    }

    pub(crate) fn versions_failed(error: impl Into<String>) -> Self {
        Self::Versions(VersionsEvent::Failed(error.into()))
    }

    pub(crate) fn auth_failed(error: impl Into<String>) -> Self {
        Self::Auth(AuthEvent::MicrosoftFailed(error.into()))
    }

    pub(crate) fn auth_success(
        username: String,
        uuid: String,
        access_token: String,
        refresh_token: String,
    ) -> Self {
        Self::Auth(AuthEvent::MicrosoftSuccess {
            username,
            uuid,
            access_token,
            refresh_token,
        })
    }

    pub(crate) fn launch_finished(profile_id: impl Into<String>) -> Self {
        Self::Launch(LaunchEvent::Finished {
            profile_id: profile_id.into(),
        })
    }

    pub(crate) fn launch_ready(profile_id: impl Into<String>) -> Self {
        Self::Launch(LaunchEvent::Ready {
            profile_id: profile_id.into(),
        })
    }

    pub(crate) fn launch_failed(profile_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self::Launch(LaunchEvent::Failed {
            profile_id: profile_id.into(),
            error: error.into(),
        })
    }

    pub(crate) fn discovery_failed(kind: DiscoveryKind, error: impl Into<String>) -> Self {
        Self::Discovery(DiscoveryEvent::ActionFailed {
            kind,
            error: error.into(),
        })
    }

    pub(crate) fn discovery_search_results(
        kind: DiscoveryKind,
        query: String,
        results: Vec<DiscoveryCardData>,
    ) -> Self {
        Self::Discovery(DiscoveryEvent::SearchResults {
            kind,
            query,
            results,
        })
    }

    pub(crate) fn discovery_search_failed(kind: DiscoveryKind, error: impl Into<String>) -> Self {
        Self::Discovery(DiscoveryEvent::SearchFailed {
            kind,
            error: error.into(),
        })
    }

    pub(crate) fn discovery_install_finished(
        kind: DiscoveryKind,
        title: String,
        target_path: String,
    ) -> Self {
        Self::Discovery(DiscoveryEvent::InstallFinished {
            kind,
            title,
            target_path,
        })
    }

    pub(crate) fn discovery_installed_changed(kind: DiscoveryKind) -> Self {
        Self::Discovery(DiscoveryEvent::InstalledChanged(kind))
    }
}
