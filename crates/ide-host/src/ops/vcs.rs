use crate::{Host, apply_git_executable};
use ide_core::IdeResult;
use ide_vcs::FileStatus;
use std::path::Path;

/// Changes of every repository of the project — Git repositories, or the
/// Subversion working copies of a project without Git ones.
pub async fn status(host: &Host, window: &str) -> IdeResult<Vec<FileStatus>> {
    let project = host.project(window)?;
    host.blocking(move || {
        let settings = project.settings.effective();
        if project.is_trusted() {
            apply_git_executable(&settings);
            let svn = settings.vcs.svn.executable.trim();
            ide_vcs_svn::set_svn_executable((!svn.is_empty()).then(|| Path::new(svn)));
        }
        let repos = project.repos();
        if !repos.is_empty() {
            for r in repos.all() {
                r.set_ignored_names(settings.ignored_names());
            }
            return repos.status();
        }
        let svns = project.svn_repos();
        if !svns.is_empty() {
            return svns.status_under(&project.root);
        }
        Err(ide_core::IdeError::unavailable("project has no version control"))
    })
    .await
}

mod commands;
pub use commands::*;
