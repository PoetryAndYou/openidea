//! Subversion working-copy support: status, diff, blame and log reads plus the write
//! operations of a project, by wrapping the `svn` command-line client.
//!
//! SVN 1.7 and later keep a single `.svn` directory at the working-copy root, which is what
//! [`SvnRepo::discover`] looks for; [`SvnRepos`] adds the checkouts nested inside a project.

mod unified;

pub use unified::parse_unified;

use ide_core::{IdeError, IdeResult};
use ide_vcs::{BlameLine, BranchInfo, ChangeKind, ChangedFile, Commit, FileStatus, LineHunk, LogDetails, LogQuery};
use roxmltree::Document;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

/// `svn log` without an explicit limit returns this many entries.
const DEFAULT_LOG_LIMIT: u32 = 200;

/// How much of a failing svn's stderr an error message keeps.
const STDERR_EXCERPT: usize = 400;

/// How deep below the project root nested working copies are looked for.
const MAX_DEPTH: usize = 4;

/// Folders never searched for nested working copies (IDE state, dependencies, build output).
const SKIPPED: &[&str] = &[".git", ".svn", ".openidea", "node_modules", "target", "build", "dist"];

/// The working copy's repository identity and last-changed revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SvnInfo {
    /// URL of the working copy's root.
    pub url: String,
    /// The URL relative to the repository root, e.g. `^/trunk`.
    pub relative_url: String,
    /// URL of the repository root.
    pub repo_root: String,
    /// The repository's UUID.
    pub uuid: String,
    /// Revision of the working copy's root.
    pub revision: String,
    /// Author of the last change of the root.
    pub author: String,
    /// Time of that change, epoch milliseconds.
    pub date: f64,
}

/// The result of [`SvnRepo::update`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSummary {
    /// The working copy's revision afterwards, `rN`.
    pub revision: String,
    /// `svn update`'s output; conflicts are left in the working copy.
    pub output: String,
}

/// A Subversion working copy (a root with a single `.svn` directory).
pub struct SvnRepo {
    root: PathBuf,
    svn: Option<PathBuf>,
    /// The last `svn info`: URL, UUID and revision answer for every path mapping
    /// and branch identity, so those do not spawn a process each.
    info: std::sync::Mutex<Option<SvnInfo>>,
}

impl SvnRepo {
    /// Discovers the working copy containing `path` (a file or folder); `Ok(None)` when
    /// neither it nor any ancestor has a `.svn` directory.
    pub fn discover(path: &Path) -> IdeResult<Option<Self>> {
        let start = if path.is_file() { path.parent().unwrap_or(path) } else { path };
        let start = absolute(start);
        let mut dir = Some(dunce::simplified(&start));
        while let Some(current) = dir {
            if current.join(".svn").is_dir() {
                return Ok(Some(Self { root: current.to_path_buf(), svn: detect_svn(), info: Default::default() }));
            }
            dir = current.parent();
        }
        Ok(None)
    }

    /// The working-copy root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the `svn` command-line client was found (calls fail with
    /// [`IdeError::unavailable`] otherwise).
    pub fn available(&self) -> bool {
        self.svn.is_some()
    }

    /// `svn info --xml` of the working-copy root, cached until an operation that
    /// can move the working copy's revision (`update`, `commit`) clears it.
    pub fn info(&self) -> IdeResult<SvnInfo> {
        if let Some(hit) = self.info.lock().unwrap().clone() {
            return Ok(hit);
        }
        let fresh = parse_info(&self.run(&["info", "--xml"])?)?;
        *self.info.lock().unwrap() = Some(fresh.clone());
        Ok(fresh)
    }

    /// Drops the cached [`SvnInfo`]; call after `update`/`commit` moved the revision.
    fn invalidate_info(&self) {
        *self.info.lock().unwrap() = None;
    }

    /// The branch-shaped identity of the working copy: its URL path and revision.
    ///
    /// A working copy of the repository root has no branch name (`head: None`).
    pub fn branch_info(&self) -> IdeResult<BranchInfo> {
        let info = self.info()?;
        let path = info.relative_url.trim_start_matches("^/").trim_matches('/');
        Ok(BranchInfo {
            head: (!path.is_empty()).then(|| format!("{path}@{}", info.revision)),
            head_commit: Some(info.revision),
            upstream: Some(info.repo_root),
            ahead: 0,
            behind: 0,
            operation: None,
        })
    }

    /// The working copy's changes, sorted by path.
    ///
    /// `.openidea` (the IDE's own state) is never reported.
    pub fn status(&self) -> IdeResult<Vec<FileStatus>> {
        parse_status(&self.run(&["status", "--xml"])?, &self.root)
    }

    /// The unified diff of one path against the working copy's BASE. Empty for paths that are
    /// not under version control or no longer exist.
    pub fn diff_text(&self, path: &Path) -> IdeResult<String> {
        let target = path.to_string_lossy();
        let out = self.output(&["diff", &target])?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("E200009") || stderr.contains("E155010") {
            return Ok(String::new());
        }
        Err(IdeError::internal(format!("svn: {}", stderr_excerpt(&out.stderr))))
    }

    /// The diff's line hunks, for editor gutter markers.
    pub fn line_hunks(&self, path: &Path) -> IdeResult<Vec<LineHunk>> {
        Ok(parse_unified(&self.diff_text(path)?))
    }

    /// `svn blame` of one path; lines that are not committed yet are left out.
    pub fn blame(&self, path: &Path) -> IdeResult<Vec<BlameLine>> {
        let target = path.to_string_lossy();
        parse_blame(&self.run(&["blame", "--xml", &target])?)
    }

    /// The newest `query.limit` revisions (200 when unset) of the project or of one path,
    /// filtered here by author and message.
    ///
    /// `svn log` runs against the repository URL rather than the working-copy path: the root
    /// of a working copy keeps the revision of its last update, so `svn log <wc>` would hide
    /// commits newer than that.
    pub fn log(&self, query: &LogQuery) -> IdeResult<Vec<Commit>> {
        let limit = if query.limit == 0 { DEFAULT_LOG_LIMIT } else { query.limit };
        // One target per filtered path (a single `svn log` accepts several URLs of
        // the same repository); the whole working copy when nothing is filtered.
        let mut targets: Vec<String> = Vec::new();
        if let Some(path) = query.path.as_deref() {
            targets.push(self.log_target(Some(path)));
        }
        for path in &query.paths {
            let target = self.log_target(Some(path));
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        if targets.is_empty() {
            targets.push(self.log_target(None));
        }
        let limit_arg = limit.to_string();
        let mut args = vec!["log", "--xml", "-v", "-l", &limit_arg];
        args.extend(targets.iter().map(String::as_str));
        let xml = match self.run(&args) {
            Ok(xml) => xml,
            // A filtered path outside this working copy's repository: log that one path.
            Err(_) if targets.len() > 1 => self.run(&["log", "--xml", "-v", "-l", &limit_arg, &targets[0]])?,
            Err(error) => return Err(error),
        };
        let mut commits = parse_log(&xml)?;
        commits.retain(|commit| {
            matches_author(commit, query) && matches_text(commit, query) && matches_date(commit, query)
        });
        Ok(commits.into_iter().skip(query.skip as usize).take(limit as usize).collect())
    }

    /// The files changed by one revision, as repository-absolute paths
    /// (`/projB/src/Main.java`): one atomic commit may touch files of several
    /// working copies of the same repository, so the caller maps them into the
    /// project through [`SvnRepos::project_paths`].
    pub fn commit_files(&self, revision: &str) -> IdeResult<Vec<ChangedFile>> {
        let rev = normalize_rev(revision);
        // Against the repository root: a mixed-revision working copy cannot serve
        // revisions newer than its peg from a path, and a working-copy URL would
        // report only the paths below itself — a revision that touched another
        // project of the repository would list nothing.
        let target = self.repo_root_url()?;
        let xml = self.run(&["log", "--xml", "-v", "-r", &rev, &target])?;
        parse_changed_files(&xml, "")
    }

    /// The message of one revision. Subversion has unsigned commits only.
    pub fn log_details(&self, revision: &str) -> IdeResult<LogDetails> {
        let rev = normalize_rev(revision);
        let target = self.repo_root_url()?;
        let xml = self.run(&["log", "--xml", "-r", &rev, &target])?;
        let doc = Document::parse(&xml).map_err(xml_error)?;
        let message =
            doc.descendants().find(|node| node.has_tag_name("logentry")).map(|entry| child_text(&entry, "msg"));
        Ok(LogDetails {
            message: message.unwrap_or_default(),
            containing_branches: Vec::new(),
            signature: "N".to_string(),
            signer: String::new(),
            key: String::new(),
            fingerprint: String::new(),
        })
    }

    /// The text of `path` at `rev` (`BASE` when `None`); `Ok(None)` when it does not exist there.
    pub fn file_at(&self, path: &Path, rev: Option<&str>) -> IdeResult<Option<String>> {
        let rev = normalize_rev(rev.unwrap_or(""));
        let target = path.to_string_lossy();
        let out = self.output(&["cat", "-r", &rev, &target])?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
    }

    /// The text of a repository-absolute path (`/branches/x/src/A.java`) at `rev`,
    /// read from the repository by URL — for paths of a revision that lie outside
    /// this working copy's checkout (history before a move, or a commit that
    /// touched another project).
    pub fn file_at_repo_path(&self, repo_path: &str, rev: Option<&str>) -> IdeResult<Option<String>> {
        let rev = normalize_rev(rev.unwrap_or(""));
        let root = self.info().ok().and_then(|info| {
            let root = info.repo_root.trim_end_matches('/');
            (!root.is_empty()).then(|| format!("{root}{}", repo_path))
        });
        let Some(target) = root else { return Ok(None) };
        let out = self.output(&["cat", "-r", &rev, &target])?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
    }

    /// Commits `paths` (the whole working copy when empty) and returns the new revision, `rN`.
    pub fn commit(&self, paths: &[PathBuf], message: &str) -> IdeResult<String> {
        let mut args = vec!["commit".to_string(), "-m".to_string(), message.to_string()];
        args.extend(paths.iter().map(|path| path.to_string_lossy().into_owned()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run(&args)?;
        self.invalidate_info();
        rev_from(&out).ok_or_else(|| IdeError::internal("svn: commit reported no revision"))
    }

    /// Updates the working copy to HEAD, postponing conflicts, and reports its new revision.
    pub fn update(&self) -> IdeResult<UpdateSummary> {
        let output = self.run(&["update", "--accept", "postpone"])?.trim().to_string();
        self.invalidate_info();
        Ok(UpdateSummary { revision: rev_from(&output).unwrap_or_default(), output })
    }

    /// Schedules `paths` for addition (`--parents` also schedules missing parent folders).
    pub fn add(&self, paths: &[PathBuf]) -> IdeResult<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["add", "--parents", "--force"];
        let targets: Vec<String> = paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
        args.extend(targets.iter().map(String::as_str));
        self.run(&args).map(|_| ())
    }

    /// Appends `paths`' names to their folders' `svn:ignore` property (SVN
    /// ignores are per-directory properties, not a file). Returns the folders
    /// whose property changed.
    pub fn add_to_ignore(&self, paths: &[PathBuf]) -> IdeResult<Vec<PathBuf>> {
        let mut groups: Vec<(PathBuf, Vec<String>)> = Vec::new();
        for path in paths {
            let Some(parent) = path.parent() else { continue };
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
            match groups.iter_mut().find(|(dir, _)| dir == parent) {
                Some((_, names)) => {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
                None => groups.push((parent.to_path_buf(), vec![name])),
            }
        }
        let mut changed = Vec::new();
        for (dir, names) in groups {
            let dir_arg = dir.to_string_lossy().into_owned();
            let current = self.run(&["propget", "svn:ignore", &dir_arg]).unwrap_or_default();
            let mut lines: Vec<String> =
                current.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_string).collect();
            let before = lines.len();
            for name in names {
                if !lines.contains(&name) {
                    lines.push(name);
                }
            }
            if lines.len() == before {
                continue;
            }
            self.run(&["propset", "svn:ignore", &lines.join("\n"), &dir_arg])?;
            changed.push(dir);
        }
        Ok(changed)
    }

    /// Marks conflicts as resolved, keeping the working file's content
    /// (`svn resolve --accept working`).
    pub fn resolve(&self, paths: &[PathBuf]) -> IdeResult<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["resolve", "--accept", "working"];
        let targets: Vec<String> = paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
        args.extend(targets.iter().map(String::as_str));
        self.run(&args).map(|_| ())
    }

    /// Updates the working copy to `revision` (`svn update -r`), keeping local
    /// changes and leaving conflicts in the tree.
    pub fn update_to(&self, revision: &str) -> IdeResult<UpdateSummary> {
        let rev = normalize_rev(revision);
        let out = self.run(&["update", "-r", &rev, "--accept", "postpone"])?;
        self.invalidate_info();
        Ok(UpdateSummary { revision: rev_from(&out).unwrap_or_else(|| format!("r{rev}")), output: out })
    }

    /// Removes stale locks and resumes unfinished operations (`svn cleanup`) —
    /// the way out of `E155004: run 'svn cleanup'`.
    pub fn cleanup(&self) -> IdeResult<()> {
        self.run(&["cleanup"]).map(|_| ())
    }

    /// Every file present at `revision` (`svn list -R`), relative to the
    /// working copy's branch base.
    pub fn revision_files(&self, revision: &str) -> IdeResult<Vec<String>> {
        let rev = normalize_rev(revision);
        let url = self.url_of("").ok_or_else(|| IdeError::unavailable("svn working copy has no URL"))?;
        let out = self.run(&["list", "-R", "-r", &rev, &url])?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.ends_with('/'))
            .map(str::to_string)
            .collect())
    }

    /// Direct children of `directory` at `revision` (`svn list`, one level).
    pub fn revision_children(&self, revision: &str, directory: &str) -> IdeResult<Vec<String>> {
        let rev = normalize_rev(revision);
        let url = self
            .url_of(directory.trim_matches('/'))
            .ok_or_else(|| IdeError::unavailable("svn working copy has no URL"))?;
        let out = self.run(&["list", "-r", &rev, &url])?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| line.trim_end_matches('/').to_string())
            .collect())
    }

    /// Schedules deletion (`svn delete --force`). The files may already be gone
    /// from disk (the IDE deleted them), so `--force` is always passed.
    pub fn delete(&self, paths: &[PathBuf]) -> IdeResult<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["delete", "--force"];
        let targets: Vec<String> = paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
        args.extend(targets.iter().map(String::as_str));
        self.run(&args).map(|_| ())
    }

    /// Reverts `paths` to their BASE content and schedule.

    pub fn revert(&self, paths: &[PathBuf]) -> IdeResult<()> {
        if paths.is_empty() {
            return Ok(());
        }
        // Depth infinity: a scheduled add of a folder (`.idea`) cannot be reverted
        // without its children (E155038).
        let mut args = vec!["revert", "--depth", "infinity"];
        let targets: Vec<String> = paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
        args.extend(targets.iter().map(String::as_str));
        self.run(&args).map(|_| ())
    }

    /// The repository URL of a working-copy-relative path (`rel` empty: the working copy's URL).
    pub fn url_of(&self, rel: &str) -> Option<String> {
        let info = self.info().ok()?;
        let base = info.url.trim_end_matches('/');
        Some(if rel.is_empty() { base.to_string() } else { format!("{base}/{}", rel.trim_start_matches('/')) })
    }

    /// The part of a repository path (`/projB/src/Main.java`) that lies inside
    /// this working copy: the path past its own repository prefix. `None` when
    /// the path is outside (`/projA/…` seen from a checkout of `…/projB`).
    pub fn wc_suffix(&self, repo_path: &str) -> Option<String> {
        let info = self.info().ok()?;
        let base = info.relative_url.trim_start_matches("^/").trim_matches('/');
        let prefix = if base.is_empty() { String::new() } else { format!("/{base}") };
        let suffix = repo_path.strip_prefix(&prefix)?.trim_start_matches('/');
        Some(suffix.to_string())
    }

    /// The repository root URL, for revision-wide queries (a working-copy URL
    /// sees only its own subtree's history).
    fn repo_root_url(&self) -> IdeResult<String> {
        let root = self.info()?.repo_root.trim_end_matches('/').to_string();
        (!root.is_empty())
            .then_some(root)
            .ok_or_else(|| IdeError::unavailable("svn working copy has no repository root URL"))
    }

    /// The URL `svn log` is run against for a working-copy path.
    fn log_target(&self, path: Option<&Path>) -> String {
        let Some(path) = path else {
            return self.url_of("").unwrap_or_else(|| self.root.to_string_lossy().into_owned());
        };
        if let Some(rel) = strip_prefix_lenient(&self.root, path)
            && let Some(url) = self.url_of(&rel_string(&rel))
        {
            return url;
        }
        path.to_string_lossy().into_owned()
    }

    /// Runs `svn <args>` in the working-copy root, whatever the exit status; a missing or
    /// unusable executable is [`IdeError::unavailable`].
    fn output(&self, args: &[&str]) -> IdeResult<Output> {
        use std::process::Stdio as _;
        let exe = self.svn.as_ref().ok_or_else(|| IdeError::unavailable("svn command-line client not found"))?;
        let mut command = Command::new(exe);
        command.arg("--non-interactive").args(args).current_dir(&self.root);
        // A GUI process spawning a console executable would flash a console
        // window per call otherwise.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command.output().map_err(|_| IdeError::unavailable("svn command-line client not found"))
    }

    /// Runs svn and returns stdout; a non-zero exit is an error carrying svn's stderr.
    fn run(&self, args: &[&str]) -> IdeResult<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            return Err(IdeError::internal(format!("svn: {}", stderr_excerpt(&out.stderr))));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// An explicitly configured `svn` (Settings › Version Control › Subversion),
/// used by [`detect_svn`] before `PATH` and the probe list.
static SVN_EXECUTABLE: std::sync::RwLock<Option<std::path::PathBuf>> = std::sync::RwLock::new(None);

/// Sets the `svn` executable configured in the settings; `None` clears it.
pub fn set_svn_executable(path: Option<&Path>) {
    *SVN_EXECUTABLE.write().unwrap_or_else(|e| e.into_inner()) = path.map(Path::to_path_buf);
}

/// The configured `svn` executable, when set and present.
pub fn configured_svn() -> Option<std::path::PathBuf> {
    SVN_EXECUTABLE.read().unwrap_or_else(|e| e.into_inner()).clone().filter(|p| p.is_file())
}

/// The `svn` client: `PATH` first, then the usual install locations. A GUI app
/// started from the Dock or the Start menu inherits a minimal `PATH`
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), so Homebrew's and MacPorts' prefixes are
/// probed by name; on Windows, TortoiseSVN's and SlikSVN's.
fn detect_svn() -> Option<std::path::PathBuf> {
    if let Some(configured) = configured_svn() {
        return Some(configured);
    }
    if let Ok(path) = which::which("svn") {
        return Some(path);
    }
    let known = if cfg!(windows) {
        [
            r"C:\Program Files\TortoiseSVN\bin\svn.exe",
            r"C:\Program Files (x86)\TortoiseSVN\bin\svn.exe",
            r"C:\Program Files\SlikSvn\bin\svn.exe",
            r"C:\Program Files (x86)\SlikSvn\bin\svn.exe",
        ]
    } else {
        ["/opt/homebrew/bin/svn", "/usr/local/bin/svn", "/opt/local/bin/svn", "/usr/bin/svn"]
    };
    known.iter().map(std::path::PathBuf::from).find(|p| p.is_file())
}

/// The working copies of a project: the one containing the project root plus the checkouts
/// nested below it (independent working copies side by side).
#[derive(Clone, Default)]
pub struct SvnRepos {
    repos: Vec<Arc<SvnRepo>>,
}

impl SvnRepos {
    /// Only the working copy containing `project_root`.
    pub fn enclosing(project_root: &Path) -> SvnRepos {
        let mut repos = Vec::new();
        if let Ok(Some(repo)) = SvnRepo::discover(project_root) {
            repos.push(Arc::new(repo));
        }
        Self::new(repos)
    }

    /// The working copy containing `project_root` and every working copy below it, up to
    /// [`MAX_DEPTH`] levels down. Hidden and dependency folders and folders `skip` rejects are
    /// not searched.
    pub fn discover(project_root: &Path, skip: &dyn Fn(&Path) -> bool) -> IdeResult<SvnRepos> {
        let mut repos = Self::enclosing(project_root).repos;
        let walker = ignore::WalkBuilder::new(project_root)
            .max_depth(Some(MAX_DEPTH))
            .hidden(true)
            .require_git(false)
            .git_ignore(false)
            // `filter_entry` wants a `Send + Sync + 'static` closure, so `skip` (a borrowed
            // trait object) cannot be called here; its answers are applied to the found
            // working copies instead.
            .filter_entry(|entry| {
                if entry.depth() == 0 {
                    return true;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let name: &str = &name;
                !SKIPPED.contains(&name)
            })
            .build();
        for entry in walker.flatten() {
            let path = entry.path();
            if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_dir()) || !path.join(".svn").is_dir()
            {
                continue;
            }
            if skipped_below(path, project_root, skip) {
                continue;
            }
            if let Ok(Some(repo)) = SvnRepo::discover(path)
                && repo.root() == path
            {
                repos.push(Arc::new(repo));
            }
        }
        Ok(Self::new(repos))
    }

    /// The working copies of independent project folders of a workspace.
    pub fn discover_roots(roots: &[PathBuf], skip: &dyn Fn(&Path) -> bool) -> IdeResult<SvnRepos> {
        let mut repos = Vec::new();
        for root in roots {
            repos.extend(Self::discover(root, skip)?.repos);
        }
        Ok(Self::new(repos))
    }

    /// Working-copy roots, innermost first.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.repos.iter().map(|repo| repo.root().to_path_buf()).collect()
    }

    /// The working copies, innermost first.
    pub fn all(&self) -> &[Arc<SvnRepo>] {
        &self.repos
    }

    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }

    /// The innermost working copy containing `path`.
    pub fn for_path(&self, path: &Path) -> Option<&Arc<SvnRepo>> {
        if let Some(repo) = self.repos.iter().find(|repo| path.starts_with(repo.root())) {
            return Some(repo);
        }
        // Another spelling of the same location (symlinks, /var vs /private/var).
        let path = dunce::canonicalize(path).ok()?;
        self.repos.iter().find(|repo| dunce::canonicalize(repo.root()).is_ok_and(|root| path.starts_with(root)))
    }

    /// The working copy whose root is `root`.
    pub fn at_root(&self, root: &Path) -> Option<&Arc<SvnRepo>> {
        if let Some(repo) = self.repos.iter().find(|repo| repo.root() == root) {
            return Some(repo);
        }
        let root = dunce::canonicalize(root).ok()?;
        self.repos.iter().find(|repo| dunce::canonicalize(repo.root()).is_ok_and(|own| own == root))
    }

    /// The changed paths of every working copy, relative to `project_root`: each
    /// working copy contributes the subtree below its own root.
    pub fn status_under(&self, project_root: &Path) -> IdeResult<Vec<FileStatus>> {
        let mut out = Vec::new();
        for repo in &self.repos {
            let base = repo.root().strip_prefix(project_root).unwrap_or(repo.root());
            let base = base.to_string_lossy().trim_matches('/').to_string();
            for mut entry in repo.status()? {
                if !base.is_empty() {
                    entry.rel_path = format!("{base}/{}", entry.rel_path);
                }
                out.push(entry);
            }
        }
        Ok(out)
    }

    /// Maps repository-absolute paths (`/projB/src/Main.java`) to paths relative
    /// to `project_root` through the discovered working copies: one atomic commit
    /// may touch files of several working copies of the same repository.
    pub fn project_paths(&self, project_root: &Path, repo_paths: &[String]) -> Vec<String> {
        repo_paths.iter().map(|path| self.project_path(project_root, path)).collect()
    }

    /// The single-path form of [`Self::project_paths`].
    pub fn project_path(&self, project_root: &Path, repo_path: &str) -> String {
        for repo in &self.repos {
            if let Some(suffix) = repo.wc_suffix(repo_path) {
                let base = repo.root().strip_prefix(project_root).unwrap_or(repo.root());
                let owned = base.to_string_lossy().into_owned();
                let base = owned.trim_matches('/');
                return match (base.is_empty(), suffix.is_empty()) {
                    (true, true) => String::new(),
                    (true, false) => suffix,
                    (false, true) => base.to_string(),
                    (false, false) => format!("{base}/{suffix}"),
                };
            }
        }
        repo_path.trim_start_matches('/').to_string()
    }

    /// The status of every working copy, one entry per changed path.
    pub fn status(&self) -> IdeResult<Vec<FileStatus>> {
        let mut out = Vec::new();
        for repo in &self.repos {
            out.extend(repo.status()?);
        }
        Ok(out)
    }

    /// Deduplicates by canonical root and keeps the innermost (longest root) first.
    fn new(mut repos: Vec<Arc<SvnRepo>>) -> Self {
        let mut seen: Vec<PathBuf> = Vec::new();
        repos.retain(|repo| {
            let key = dunce::canonicalize(repo.root()).unwrap_or_else(|_| repo.root().to_path_buf());
            if seen.contains(&key) {
                return false;
            }
            seen.push(key);
            true
        });
        repos.sort_by(|a, b| b.root().as_os_str().len().cmp(&a.root().as_os_str().len()));
        Self { repos }
    }
}

/// Whether `path` or one of its ancestors below `project_root` is rejected by `skip`
/// (a working copy inside a folder the caller excludes is not part of the project).
fn skipped_below(path: &Path, project_root: &Path, skip: &dyn Fn(&Path) -> bool) -> bool {
    let mut dir = Some(path);
    while let Some(current) = dir {
        if current == project_root {
            return false;
        }
        if skip(current) {
            return true;
        }
        dir = current.parent();
    }
    false
}

/// Maps `svn status`'s item and props attributes to the staged/unstaged kinds of a
/// [`FileStatus`]. `None` is an entry that is not a change (ignored, external, normal).
fn status_kind(item: &str, props: &str) -> Option<(Option<ChangeKind>, Option<ChangeKind>)> {
    Some(match item {
        "added" => (Some(ChangeKind::Added), None),
        "deleted" => (Some(ChangeKind::Deleted), None),
        "replaced" => (Some(ChangeKind::Added), None),
        "conflicted" => (None, Some(ChangeKind::Conflicted)),
        "missing" => (None, Some(ChangeKind::Deleted)),
        "obstructed" => (None, Some(ChangeKind::Conflicted)),
        "modified" => (None, Some(ChangeKind::Modified)),
        "normal" | "none" if props == "modified" => (None, Some(ChangeKind::Modified)),
        "unversioned" => (None, Some(ChangeKind::Untracked)),
        _ => return None,
    })
}

/// `svn info --xml` → [`SvnInfo`].
fn parse_info(xml: &str) -> IdeResult<SvnInfo> {
    let doc = Document::parse(xml).map_err(xml_error)?;
    let entry = doc
        .descendants()
        .find(|node| node.has_tag_name("entry"))
        .ok_or_else(|| IdeError::internal("svn: info has no entry"))?;
    let commit = entry.children().find(|node| node.has_tag_name("commit"));
    Ok(SvnInfo {
        url: child_text(&entry, "url"),
        relative_url: child_text(&entry, "relative-url"),
        repo_root: entry
            .descendants()
            .find(|node| node.has_tag_name("repository"))
            .map(|repository| child_text(&repository, "root"))
            .unwrap_or_default(),
        uuid: entry
            .descendants()
            .find(|node| node.has_tag_name("repository"))
            .map(|repository| child_text(&repository, "uuid"))
            .unwrap_or_default(),
        revision: entry.attribute("revision").unwrap_or_default().to_string(),
        author: commit.as_ref().map(|commit| child_text(commit, "author")).unwrap_or_default(),
        date: commit.as_ref().map(|commit| svn_date_ms(&child_text(commit, "date"))).unwrap_or_default(),
    })
}

/// `svn status --xml` → the working copy's [`FileStatus`] entries, sorted by path.
fn parse_status(xml: &str, root: &Path) -> IdeResult<Vec<FileStatus>> {
    let doc = Document::parse(xml).map_err(xml_error)?;
    let mut out = Vec::new();
    for entry in doc.descendants().filter(|node| node.has_tag_name("entry")) {
        let Some(path) = entry.attribute("path") else { continue };
        let rel = path.replace('\\', "/");
        let rel = rel.trim_start_matches("./");
        // "" / "." is the working copy root itself; .openidea is the IDE's own dir.
        if rel.is_empty() || rel == "." || rel == ".openidea" || rel.starts_with(".openidea/") {
            continue;
        }
        let Some(status) = entry.children().find(|node| node.has_tag_name("wc-status")) else { continue };
        let item = status.attribute("item").unwrap_or_default();
        let props = status.attribute("props").unwrap_or_default();
        let Some((staged, unstaged)) = status_kind(item, props) else { continue };
        out.push(FileStatus {
            path: ide_core::IpcPath(root.join(rel)),
            rel_path: rel.to_string(),
            staged,
            unstaged,
            renamed_from: None,
        });
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

/// `svn blame --xml` → one entry per committed line; uncommitted lines have no `<commit>` and
/// are left out.
fn parse_blame(xml: &str) -> IdeResult<Vec<BlameLine>> {
    let doc = Document::parse(xml).map_err(xml_error)?;
    Ok(doc
        .descendants()
        .filter(|node| node.has_tag_name("entry"))
        .filter_map(|entry| {
            let commit = entry.children().find(|node| node.has_tag_name("commit"))?;
            Some(BlameLine {
                commit: format!("r{}", commit.attribute("revision").unwrap_or_default()),
                author: child_text(&commit, "author"),
                time_ms: svn_date_ms(&child_text(&commit, "date")),
                summary: String::new(),
            })
        })
        .collect())
}

/// `svn log --xml` → [`Commit`]s, newest first.
fn parse_log(xml: &str) -> IdeResult<Vec<Commit>> {
    let doc = Document::parse(xml).map_err(xml_error)?;
    Ok(doc
        .descendants()
        .filter(|node| node.has_tag_name("logentry"))
        .map(|entry| {
            let number: u64 = entry.attribute("revision").and_then(|r| r.parse().ok()).unwrap_or(0);
            let revision = format!("r{number}");
            let author = child_text(&entry, "author");
            let message = child_text(&entry, "msg");
            let time_ms = svn_date_ms(&child_text(&entry, "date"));
            // The revision before this one is the baseline the Log's file diff shows
            // (`r7`'s parent is `r6`): without it the left side reads "(does not exist)"
            // and every file looks like it was added whole.
            let parents = (number > 1).then(|| vec![format!("r{}", number - 1)]).unwrap_or_default();
            Commit {
                id: revision.clone(),
                short_id: revision,
                parents,
                // No graph edges: the history of the checked-out projects skips
                // revisions that touched other projects, and an edge per gap drew
                // a lane each (IDEA shows the Subversion log as a flat list too).
                graph_parents: Some(Vec::new()),
                graph_dashed: Vec::new(),
                history_path: None,
                log_position: None,
                log_total: None,
                graph_boundary_parents: Vec::new(),
                on_current_branch: None,
                committer: author.clone(),
                committer_email: String::new(),
                author,
                email: String::new(),
                time_ms,
                summary: message.lines().next().unwrap_or_default().to_string(),
                message,
                message_truncated: false,
                refs: Vec::new(),
                commit_time_ms: time_ms,
                dashed: Vec::new(),
            }
        })
        .collect())
}

/// `svn log --xml -v` → the files of the first revision, with `base` (the working copy's
/// repository path, e.g. `trunk`) stripped.
fn parse_changed_files(xml: &str, base: &str) -> IdeResult<Vec<ChangedFile>> {
    let doc = Document::parse(xml).map_err(xml_error)?;
    let paths = doc
        .descendants()
        .find(|node| node.has_tag_name("logentry"))
        .and_then(|entry| entry.children().find(|node| node.has_tag_name("paths")));
    let mut out = Vec::new();
    let nodes = paths.iter().flat_map(|paths| paths.children().filter(|node| node.has_tag_name("path")));
    for node in nodes {
        // Directories are implied by the files below them.
        if node.attribute("kind") == Some("dir") {
            continue;
        }
        let kind = match node.attribute("action") {
            Some("A") => ChangeKind::Added,
            Some("M") => ChangeKind::Modified,
            Some("D") => ChangeKind::Deleted,
            Some("R") => ChangeKind::Renamed,
            _ => continue,
        };
        let path = strip_base(node.text().unwrap_or_default(), base);
        if path.is_empty() {
            continue;
        }
        let old_path = if kind == ChangeKind::Renamed {
            node.attribute("copyfrom-path").map(|from| strip_base(from, base))
        } else {
            None
        };
        out.push(ChangedFile { path, kind, old_path });
    }
    Ok(out)
}

/// The commit's author and message filters of [`LogQuery`].
fn matches_author(commit: &Commit, query: &LogQuery) -> bool {
    let matches = |needle: &str| {
        if query.authors_exact { commit.author == needle } else { commit.author.contains(needle) }
    };
    let author = query.author.as_deref().filter(|author| !author.is_empty()).is_none_or(matches);
    let authors = query.authors.is_empty() || query.authors.iter().any(|author| matches(author));
    author && authors
}

/// The message filters (`text`, `speed_search`) of [`LogQuery`], case-insensitively.
fn matches_text(commit: &Commit, query: &LogQuery) -> bool {
    let message = commit.message.to_lowercase();
    [query.text.as_deref(), query.speed_search.as_deref()]
        .into_iter()
        .flatten()
        .filter(|needle| !needle.is_empty())
        .all(|needle| message.contains(&needle.to_lowercase()))
}

/// The committer-time bounds of [`LogQuery`] (`since` inclusive, `until` exclusive).
fn matches_date(commit: &Commit, query: &LogQuery) -> bool {
    let after = query.since_ms.is_none_or(|since| commit.time_ms >= since);
    let before = query.until_ms.is_none_or(|until| commit.time_ms < until);
    after && before
}

/// Milliseconds since the Unix epoch of an SVN XML timestamp such as
/// `2025-01-02T03:04:05.123456Z` (UTC, fixed positions, up to six fractional digits).
fn svn_date_ms(text: &str) -> f64 {
    let bytes = text.trim().as_bytes();
    let digits =
        |range: std::ops::Range<usize>| -> Option<i64> { std::str::from_utf8(&bytes[range]).ok()?.parse().ok() };
    if bytes.len() < 19 {
        return 0.0;
    }
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) =
        (digits(0..4), digits(5..7), digits(8..10), digits(11..13), digits(14..16), digits(17..19))
    else {
        return 0.0;
    };
    let days = days_from_civil(year, month, day);
    let mut ms = (((days * 24 + hour) * 60 + minute) * 60 + second) as f64 * 1000.0;
    if bytes.get(19) == Some(&b'.') {
        let fraction: Vec<u8> = bytes[20..].iter().copied().take_while(|byte| byte.is_ascii_digit()).take(6).collect();
        if !fraction.is_empty() {
            let mut micros = 0.0;
            for digit in &fraction {
                micros = micros * 10.0 + f64::from(digit - b'0');
            }
            micros *= 10f64.powi(6 - fraction.len() as i32);
            ms += micros / 1000.0;
        }
    }
    ms
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_of_year = (month + 9) % 12;
    let day_of_year = (153 * month_of_year + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `r123` / `123` → `123`; `BASE`, `HEAD` and `COMMITTED` pass through, empty becomes `BASE`.
fn normalize_rev(rev: &str) -> String {
    let rev = rev.trim();
    let number = rev.strip_prefix('r').or_else(|| rev.strip_prefix('R')).unwrap_or(rev);
    if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) {
        return number.to_string();
    }
    if rev.is_empty() { "BASE".to_string() } else { rev.to_string() }
}

/// The revision of `Committed revision 2.`, `Updated to revision 2.` or `At revision 2.`.
fn rev_from(text: &str) -> Option<String> {
    for prefix in ["Committed revision ", "Updated to revision ", "At revision "] {
        if let Some(rest) = text.split(prefix).nth(1) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return Some(format!("r{digits}"));
            }
        }
    }
    None
}

/// `path` with `/` separators, for URLs.
fn rel_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `path` below `base`: `Path::strip_prefix`, and on Windows also across
/// separator and drive-letter-case differences (the UI and the discovery walk
/// may spell the same folder differently).
fn strip_prefix_lenient(base: &Path, path: &Path) -> Option<PathBuf> {
    if let Ok(rel) = path.strip_prefix(base) {
        return Some(rel.to_path_buf());
    }
    if !cfg!(windows) {
        return None;
    }
    let lower = |p: &Path| p.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>();
    let b = lower(base);
    let p = lower(path);
    if p.len() > b.len() && p[..b.len()] == b {
        let rel: PathBuf = path.components().skip(b.len()).collect();
        return Some(rel);
    }
    None
}

/// A repository-absolute path from `svn log -v` (e.g. `/trunk/a.c`) relative to `base`
/// (e.g. `trunk`).
fn strip_base(path: &str, base: &str) -> String {
    let path = path.trim_start_matches('/');
    if base.is_empty() {
        return path.to_string();
    }
    match path.strip_prefix(base) {
        Some(rest) if rest.is_empty() => String::new(),
        Some(rest) if rest.starts_with('/') => rest.trim_start_matches('/').to_string(),
        _ => path.to_string(),
    }
}

/// The text of `node`'s direct child `name`.
fn child_text(node: &roxmltree::Node<'_, '_>, name: &str) -> String {
    node.children()
        .find(|child| child.has_tag_name(name))
        .and_then(|child| child.text())
        .unwrap_or_default()
        .to_string()
}

/// `p` as an absolute path without the Windows `\\?\` prefix (symlinks keep their spelling).
fn absolute(p: &Path) -> PathBuf {
    let p = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    dunce::simplified(&p).to_path_buf()
}

fn xml_error(error: roxmltree::Error) -> IdeError {
    IdeError::internal(format!("svn: invalid XML: {error}"))
}

fn stderr_excerpt(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let mut excerpt: String = text.chars().take(STDERR_EXCERPT).collect();
    if text.chars().count() > STDERR_EXCERPT {
        excerpt.push_str("...");
    }
    if excerpt.is_empty() { "<no output>".to_string() } else { excerpt }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_vcs::ChangeKind::{Added, Conflicted, Deleted, Modified, Untracked};

    #[test]
    fn maps_svn_status_items() {
        assert_eq!(status_kind("added", "none"), Some((Some(Added), None)));
        assert_eq!(status_kind("deleted", "none"), Some((Some(Deleted), None)));
        assert_eq!(status_kind("replaced", "none"), Some((Some(Added), None)));
        assert_eq!(status_kind("conflicted", "none"), Some((None, Some(Conflicted))));
        assert_eq!(status_kind("missing", "none"), Some((None, Some(Deleted))));
        assert_eq!(status_kind("obstructed", "none"), Some((None, Some(Conflicted))));
        assert_eq!(status_kind("modified", "none"), Some((None, Some(Modified))));
        assert_eq!(status_kind("normal", "none"), None);
        assert_eq!(status_kind("normal", "modified"), Some((None, Some(Modified))));
        assert_eq!(status_kind("none", "modified"), Some((None, Some(Modified))));
        assert_eq!(status_kind("unversioned", "none"), Some((None, Some(Untracked))));
        assert_eq!(status_kind("ignored", "none"), None);
        assert_eq!(status_kind("external", "none"), None);
    }

    #[test]
    fn parses_svn_dates() {
        assert_eq!(svn_date_ms("1970-01-01T00:00:00.000000Z"), 0.0);
        assert!((svn_date_ms("2025-01-02T03:04:05.123456Z") - 1_735_787_045_123.456).abs() < 1.0);
        assert!((svn_date_ms("1970-01-02T00:00:00.5Z") - 86_400_500.0).abs() < 0.001);
        assert_eq!(svn_date_ms("2025-01-02T03:04:05Z"), 1_735_787_045_000.0);
        assert_eq!(svn_date_ms("not a date"), 0.0);
    }

    #[test]
    fn normalizes_revisions() {
        assert_eq!(normalize_rev("r123"), "123");
        assert_eq!(normalize_rev("123"), "123");
        assert_eq!(normalize_rev("R7"), "7");
        assert_eq!(normalize_rev("BASE"), "BASE");
        assert_eq!(normalize_rev("HEAD"), "HEAD");
        assert_eq!(normalize_rev("committed"), "committed");
        assert_eq!(normalize_rev(""), "BASE");
        assert_eq!(normalize_rev(" 42 "), "42");
    }

    #[test]
    fn reads_revisions_from_svn_output() {
        assert_eq!(rev_from("Committed revision 2.\n"), Some("r2".to_string()));
        assert_eq!(rev_from("Updated to revision 12."), Some("r12".to_string()));
        assert_eq!(rev_from("Updating '.':\nAt revision 3."), Some("r3".to_string()));
        assert_eq!(rev_from("nothing to do"), None);
        assert_eq!(rev_from("At revision unchanged"), None);
    }

    #[test]
    fn filters_ide_state_from_status() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<status>
<target path=".">
<entry path=".openidea"><wc-status item="unversioned" props="none"></wc-status></entry>
<entry path=".openidea/state.json"><wc-status item="unversioned" props="none"></wc-status></entry>
<entry path="src/a.rs"><wc-status item="modified" props="none"></wc-status></entry>
<entry path="src/b.rs"><wc-status item="normal" props="modified"></wc-status></entry>
<entry path="c.txt"><wc-status item="unversioned" props="none"></wc-status></entry>
<entry path="ignored.log"><wc-status item="ignored" props="none"></wc-status></entry>
</target>
</status>"#;
        let status = parse_status(xml, Path::new("/wc")).unwrap();
        let rel: Vec<&str> = status.iter().map(|entry| entry.rel_path.as_str()).collect();
        assert_eq!(rel, ["c.txt", "src/a.rs", "src/b.rs"]);
        assert_eq!(status[0].path, ide_core::IpcPath::from(Path::new("/wc").join("c.txt")));
        assert_eq!(status[0].unstaged, Some(Untracked));
        assert_eq!(status[0].staged, None);
        assert_eq!(status[1].unstaged, Some(Modified));
        assert_eq!(status[2].unstaged, Some(Modified));
        assert_eq!(status[2].staged, None);
    }

    #[test]
    fn parses_log_entries() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<log>
<logentry revision="4">
<author>alice</author>
<date>2025-01-02T03:04:05.123456Z</date>
<paths>
<path action="M" kind="file">/trunk/a.c</path>
</paths>
<msg>fix: the thing

second paragraph</msg>
</logentry>
<logentry revision="3">
<author>bob</author>
<date>2025-01-01T00:00:00.000000Z</date>
<msg>initial</msg>
</logentry>
</log>"#;
        let commits = parse_log(xml).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].id, "r4");
        assert_eq!(commits[0].short_id, "r4");
        assert_eq!(commits[0].author, "alice");
        assert_eq!(commits[0].email, "");
        assert_eq!(commits[0].committer, "alice");
        assert_eq!(commits[0].committer_email, "");
        assert_eq!(commits[0].summary, "fix: the thing");
        assert_eq!(commits[0].message, "fix: the thing\n\nsecond paragraph");
        assert_eq!(commits[0].commit_time_ms, commits[0].time_ms);
        assert!((commits[0].time_ms - 1_735_787_045_123.456).abs() < 1.0);
        assert_eq!(commits[0].id, "r4");
        assert_eq!(commits[0].parents, vec!["r3"]);
        assert!(commits[0].refs.is_empty());
        assert_eq!(commits[0].on_current_branch, None);
        assert_eq!(commits[1].id, "r3");
        assert_eq!(commits[1].summary, "initial");
    }

    #[test]
    fn strips_the_branch_base_from_changed_files() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<log>
<logentry revision="2">
<paths>
<path action="A" kind="dir">/trunk/sub</path>
<path action="M" kind="file">/trunk/a.c</path>
<path action="R" kind="file" copyfrom-path="/branches/x/old.c">/trunk/new.c</path>
<path action="D" kind="file">/trunk/gone.c</path>
</paths>
<msg>change</msg>
</logentry>
</log>"#;
        let files = parse_changed_files(xml, "trunk").unwrap();
        let paths: Vec<(&str, ChangeKind)> = files.iter().map(|file| (file.path.as_str(), file.kind)).collect();
        assert_eq!(paths, [("a.c", Modified), ("new.c", ide_vcs::ChangeKind::Renamed), ("gone.c", Deleted)]);
        assert_eq!(files[1].old_path.as_deref(), Some("branches/x/old.c"));
        assert_eq!(files[0].old_path, None);
    }
}
