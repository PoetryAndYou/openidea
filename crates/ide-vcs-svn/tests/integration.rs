//! End-to-end tests against a real `svn` working copy.
//!
//! The tests are skipped (with a note on stderr) when `svn` and `svnadmin` are not installed.

use ide_vcs::{ChangeKind, FileStatus, HunkKind, LineHunk, LogQuery};
use ide_vcs_svn::{SvnRepo, SvnRepos};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether the Subversion command-line tools are installed.
fn available() -> bool {
    if which::which("svn").is_ok() && which::which("svnadmin").is_ok() {
        return true;
    }
    eprintln!("skipping: svn and svnadmin are not on PATH");
    false
}

/// Runs `program` in `cwd` and returns its stdout; panics when it fails.
fn run(program: &str, args: &[&str], cwd: &Path) -> String {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|error| panic!("cannot run {program}: {error}"));
    assert!(
        out.status.success(),
        "{program} {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Writes `contents` to `path`, creating parent folders.
fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// [`LineHunk`] has no `PartialEq`; compares by value instead.
fn hunk_parts(hunks: &[LineHunk]) -> Vec<(HunkKind, u32, u32, u32, u32, String)> {
    hunks
        .iter()
        .map(|hunk| (hunk.kind, hunk.start, hunk.lines, hunk.old_start, hunk.old_lines, hunk.old_text.clone()))
        .collect()
}

/// A checkout with one committed revision and local changes of every kind.
struct Fixture {
    /// Owns the temporary folder: dropping it removes the repository and the checkout.
    _dir: tempfile::TempDir,
    wc: PathBuf,
    repo: SvnRepo,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(dir.path()).unwrap();
        let repository = base.join("repo");
        let wc = base.join("wc");
        run("svnadmin", &["create", repository.to_str().unwrap()], &base);
        let url = format!("file://{}", repository.display());
        run("svn", &["checkout", "-q", &url, wc.to_str().unwrap()], &base);

        write(&wc.join("a.c"), "int main(void) {\n  return 0;\n}\n");
        write(&wc.join("sub/keep.txt"), "keep one\nkeep two\n");
        write(&wc.join("sub/b.txt"), "b\n");
        run("svn", &["add", "-q", "a.c", "sub"], &wc);
        run("svn", &["commit", "-q", "-m", "initial"], &wc);
        // The root's revision only moves on an update; `svn log` on the working copy would
        // otherwise stop at the checkout's revision.
        run("svn", &["update", "-q"], &wc);

        // Local changes: modified, deleted, added (nested), unversioned, and IDE state.
        write(&wc.join("a.c"), "int main(void) {\n  return 1;\n}\n");
        write(&wc.join("sub/nested/c.txt"), "c\n");
        run("svn", &["add", "-q", "sub/nested"], &wc);
        std::fs::remove_file(wc.join("sub/b.txt")).unwrap();
        write(&wc.join("u.txt"), "untracked\n");
        write(&wc.join(".openidea/state.json"), "{}\n");

        let repo = SvnRepo::discover(&wc).unwrap().expect("the fixture is a working copy");
        Self { _dir: dir, wc, repo }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.wc.join(rel)
    }

    fn status(&self) -> Vec<FileStatus> {
        self.repo.status().unwrap()
    }

    fn status_of<'a>(&self, status: &'a [FileStatus], rel: &str) -> &'a FileStatus {
        status.iter().find(|entry| entry.rel_path == rel).unwrap_or_else(|| panic!("no status for {rel}"))
    }
}

#[test]
fn status_and_diff_report_the_local_changes() {
    if !available() {
        return;
    }
    let fixture = Fixture::new();
    assert!(fixture.repo.available());
    assert_eq!(fixture.repo.root(), fixture.wc.as_path());

    let status = fixture.status();
    let modified = fixture.status_of(&status, "a.c");
    assert_eq!(modified.path, ide_core::IpcPath::from(fixture.path("a.c")));
    assert_eq!(modified.staged, None);
    assert_eq!(modified.unstaged, Some(ChangeKind::Modified));
    assert_eq!(modified.renamed_from, None);
    assert_eq!(fixture.status_of(&status, "sub/b.txt").unstaged, Some(ChangeKind::Deleted));
    assert_eq!(fixture.status_of(&status, "sub/nested/c.txt").staged, Some(ChangeKind::Added));
    assert_eq!(fixture.status_of(&status, "sub/nested/c.txt").unstaged, None);
    assert_eq!(fixture.status_of(&status, "u.txt").unstaged, Some(ChangeKind::Untracked));
    assert!(!status.iter().any(|entry| entry.rel_path.starts_with(".openidea")));

    let diff = fixture.repo.diff_text(&fixture.path("a.c")).unwrap();
    assert!(diff.contains("+  return 1;"), "diff: {diff}");
    assert_eq!(
        hunk_parts(&fixture.repo.line_hunks(&fixture.path("a.c")).unwrap()),
        [(HunkKind::Modified, 0, 3, 0, 3, "  return 0;".to_string())]
    );
    // A scheduled addition diffs against nothing.
    assert_eq!(
        hunk_parts(&fixture.repo.line_hunks(&fixture.path("sub/nested/c.txt")).unwrap()),
        [(HunkKind::Added, 0, 1, 0, 0, String::new())]
    );
}

#[test]
fn blame_log_and_file_contents_read_history() {
    if !available() {
        return;
    }
    let fixture = Fixture::new();

    let blame = fixture.repo.blame(&fixture.path("sub/keep.txt")).unwrap();
    assert_eq!(blame.len(), 2);
    for line in &blame {
        assert_eq!(line.commit, "r1");
        assert!(!line.author.is_empty());
        assert!(line.time_ms > 0.0);
        assert_eq!(line.summary, "");
    }
    // The locally changed line of a.c is not committed yet, so it has no blame entry.
    assert_eq!(fixture.repo.blame(&fixture.path("a.c")).unwrap().len(), 2);

    let commits = fixture.repo.log(&LogQuery::default()).unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].id, "r1");
    assert_eq!(commits[0].short_id, "r1");
    assert_eq!(commits[0].summary, "initial");
    assert_eq!(commits[0].message, "initial");
    assert!(commits[0].parents.is_empty());
    assert!(commits[0].refs.is_empty());

    let base = fixture.repo.file_at(&fixture.path("a.c"), Some("BASE")).unwrap().unwrap();
    assert!(base.contains("return 0;"), "{base}");
    let at_r1 = fixture.repo.file_at(&fixture.path("a.c"), Some("r1")).unwrap().unwrap();
    assert_eq!(at_r1, base);
    assert_eq!(fixture.repo.file_at(&fixture.path("gone.txt"), Some("BASE")).unwrap(), None);

    let files = fixture.repo.commit_files("r1").unwrap();
    assert_eq!(files.len(), 3, "{files:?}");
    assert!(files.iter().all(|file| file.old_path.is_none()));
    for path in ["a.c", "sub/b.txt", "sub/keep.txt"] {
        assert_eq!(files.iter().find(|file| file.path == path).map(|file| file.kind), Some(ChangeKind::Added));
    }
    assert!(!files.iter().any(|file| file.path == "sub"), "directories are not files");

    let details = fixture.repo.log_details("r1").unwrap();
    assert_eq!(details.message, "initial");
    assert_eq!(details.signature, "N");
    assert!(details.containing_branches.is_empty());
}

#[test]
fn commit_writes_a_new_revision_and_update_follows_it() {
    if !available() {
        return;
    }
    let fixture = Fixture::new();
    let revision = fixture.repo.commit(&[fixture.wc.clone()], "second change").unwrap();
    assert_eq!(revision, "r2");

    // Everything but the unversioned file is committed; `.openidea` stays hidden.
    let status = fixture.status();
    let rel: Vec<&str> = status.iter().map(|entry| entry.rel_path.as_str()).collect();
    assert_eq!(rel, ["sub/b.txt", "u.txt"]);
    // A file removed behind Subversion's back stays missing until it is `svn delete`d.
    assert_eq!(fixture.status_of(&status, "sub/b.txt").unstaged, Some(ChangeKind::Deleted));

    let update = fixture.repo.update().unwrap();
    assert_eq!(update.revision, "r2");
    assert!(update.revision.trim_start_matches('r').parse::<u64>().unwrap() >= 2);
    assert!(!update.output.is_empty());

    let commits = fixture.repo.log(&LogQuery::default()).unwrap();
    let ids: Vec<&str> = commits.iter().map(|commit| commit.id.as_str()).collect();
    assert_eq!(ids, ["r2", "r1"]);
    assert_eq!(commits[0].summary, "second change");
}

#[test]
fn revert_restores_modified_and_missing_files() {
    if !available() {
        return;
    }
    let fixture = Fixture::new();
    fixture.repo.revert(&[fixture.path("a.c"), fixture.path("sub/b.txt")]).unwrap();

    assert!(std::fs::read_to_string(fixture.path("a.c")).unwrap().contains("return 0;"));
    assert!(fixture.path("sub/b.txt").is_file());
    let status = fixture.status();
    assert!(status.iter().all(|entry| entry.rel_path != "a.c"));
    assert!(status.iter().all(|entry| entry.rel_path != "sub/b.txt"));
    assert!(fixture.repo.diff_text(&fixture.path("a.c")).unwrap().is_empty());
}

#[test]
fn add_schedules_unversioned_paths() {
    if !available() {
        return;
    }
    let fixture = Fixture::new();
    let path = fixture.path("deep/a/b.txt");
    write(&path, "nested\n");
    // Subversion reports the new folder, not the files inside it.
    assert_eq!(fixture.status_of(&fixture.status(), "deep").unstaged, Some(ChangeKind::Untracked));

    fixture.repo.add(&[path.clone()]).unwrap();
    let status = fixture.status();
    let added = fixture.status_of(&status, "deep/a/b.txt");
    assert_eq!(added.staged, Some(ChangeKind::Added));
    assert_eq!(added.unstaged, None);
    assert_eq!(added.path, ide_core::IpcPath::from(path));
    // `--parents` schedules the folders that did not exist in the repository.
    assert_eq!(fixture.status_of(&status, "deep/a").staged, Some(ChangeKind::Added));
}

/// An Eclipse-style workspace: the parent folder is not a working copy, every
/// project folder below it is. Opening the parent must find each project.
#[test]
fn workspace_discovery_finds_one_working_copy_per_project_folder() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(dir.path()).unwrap();
    let repository = base.join("repo");
    run("svnadmin", &["create", repository.to_str().unwrap()], &base);

    // Seed two projects and check each out into the workspace, the way Eclipse
    // checks projects out side by side, one `.svn` per project.
    let seed = base.join("seed");
    let workspace = base.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut expected: Vec<String> = Vec::new();
    for name in ["projA", "projB", "skipped"] {
        let project = seed.join(name);
        write(&project.join(".project"), &format!("<projectDescription><name>{name}</name></projectDescription>\n"));
        write(&project.join("src/Main.java"), "class Main {}\n");
        let url = format!("file://{}/{}", repository.display(), name);
        run("svn", &["import", "-q", project.to_str().unwrap(), &url, "-m", "import"], &base);
        let checkout = workspace.join(name);
        run("svn", &["checkout", "-q", &url, checkout.to_str().unwrap()], &base);
        expected.push(checkout.display().to_string());
    }

    // The parent folder itself is not a working copy.
    assert!(SvnRepo::discover(&workspace).unwrap().is_none());

    // `discover_roots` over the workspace finds every project, including under
    // folders the skip closure rejects are left out.
    let repos = SvnRepos::discover_roots(&[workspace.clone()], &|p: &Path| p.ends_with("skipped")).unwrap();
    let mut roots: Vec<String> = repos.roots().iter().map(|p| p.display().to_string()).collect();
    roots.sort();
    expected.sort();
    assert_eq!(roots, expected[..2].to_vec(), "the rejected folder is not a root");

    // The plain `discover` walk finds them too (an opened workspace root).
    let repos = SvnRepos::discover(&workspace, &|p: &Path| p.ends_with("skipped")).unwrap();
    assert_eq!(repos.all().len(), 2);
}

/// Repository paths map to project-relative paths through the working copies,
/// and the merged status prefixes each copy's subtree.
#[test]
fn workspace_paths_are_project_relative() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(dir.path()).unwrap();
    let repository = base.join("repo");
    run("svnadmin", &["create", repository.to_str().unwrap()], &base);
    let workspace = base.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    for name in ["projA", "projB"] {
        let project = base.join("seed").join(name);
        write(&project.join("src/Main.java"), "class Main {}\n");
        let url = format!("file://{}/{}", repository.display(), name);
        run("svn", &["import", "-q", project.to_str().unwrap(), &url, "-m", "import"], &base);
        run("svn", &["checkout", "-q", &url, workspace.join(name).to_str().unwrap()], &base);
    }
    let repos = SvnRepos::discover(&workspace, &|_: &Path| false).unwrap();

    // A commit of one project can touch the other's files (one atomic commit of
    // two working copies of a repository): both map below their own folder.
    assert_eq!(repos.project_path(&workspace, "/projA/src/Main.java"), "projA/src/Main.java");
    assert_eq!(repos.project_path(&workspace, "/projB/src/Main.java"), "projB/src/Main.java");
    // The root of a working copy itself, and a path outside every copy.
    assert_eq!(repos.project_path(&workspace, "/projB"), "projB");
    assert_eq!(repos.project_path(&workspace, "/other/x.txt"), "other/x.txt");

    // The merged status reports every copy's changes below its folder.
    write(&workspace.join("projA/src/Main.java"), "class MainA2 {}\n");
    write(&workspace.join("projB/src/Main.java"), "class MainB2 {}\n");
    let status = repos.status_under(&workspace).unwrap();
    let mut rels: Vec<String> = status.iter().map(|entry| entry.rel_path.clone()).collect();
    rels.sort();
    assert_eq!(rels, vec!["projA/src/Main.java", "projB/src/Main.java"]);
}

/// A file's history lists only the revisions that touched it, whatever working
/// copy of the repository answers (Show History in a multi-project workspace).
#[test]
fn file_history_lists_only_its_revisions_from_every_copy() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(dir.path()).unwrap();
    let repository = base.join("repo");
    run("svnadmin", &["create", repository.to_str().unwrap()], &base);
    let ws = base.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    for name in ["projA", "projB"] {
        let seed = base.join("seed").join(name);
        write(&seed.join("src/Main.java"), "v1\n");
        let url = format!("file://{}/{}", repository.display(), name);
        run("svn", &["import", "-q", seed.to_str().unwrap(), &url, "-m", "import"], &base);
        run("svn", &["checkout", "-q", &url, ws.join(name).to_str().unwrap()], &base);
    }

    // A revision that touched only projA, then one that touched only projB.
    let a = ws.join("projA/src/Main.java");
    write(&a, "v2\n");
    run("svn", &["commit", "-q", "-m", "projA only"], &ws.join("projA"));
    let b = ws.join("projB/src/Main.java");
    write(&b, "v2\n");
    run("svn", &["commit", "-q", "-m", "projB only"], &ws.join("projB"));

    let repos = SvnRepos::discover(&ws, &|_: &Path| false).unwrap();
    let query = LogQuery { path: Some(ide_core::IpcPath::from(b.clone())), ..Default::default() };
    // Each working copy answers for that path with the file's own revisions
    // (its creation and the projB edit), never the projA-only commit.
    for repo in repos.all() {
        let entries = repo.log(&query).unwrap();
        assert!(
            entries.iter().any(|c| c.summary == "projB only"),
            "file history from {} lists the file's revision",
            repo.root().display()
        );
        assert!(
            !entries.iter().any(|c| c.summary == "projA only"),
            "file history from {} leaves out another project's revision",
            repo.root().display()
        );
    }
}

/// `Set Revision Properties…`: the revision that last changed a file, and its
/// message replaced through the real client code (both outcomes).
#[test]
fn revision_message_is_read_and_replaced_through_the_client() {
    if !available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(dir.path()).unwrap();
    let repository = base.join("repo");
    run("svnadmin", &["create", repository.to_str().unwrap()], &base);
    let url = format!("file://{}", repository.display());
    let wc = base.join("wc");
    run("svn", &["checkout", "-q", &url, wc.to_str().unwrap()], &base);
    write(&wc.join("a.txt"), "one\n");
    run("svn", &["add", "-q", "a.txt"], &wc);
    run("svn", &["commit", "-q", "-m", "first message"], &wc);
    write(&wc.join("a.txt"), "two\n");
    run("svn", &["commit", "-q", "-m", "second message"], &wc);

    let repo = SvnRepo::discover(&wc).unwrap().expect("a working copy");
    let path = wc.join("a.txt");
    let revision = repo.last_changed_revision(&path).unwrap().expect("the file has history");
    assert_eq!(repo.log_details(&revision).unwrap().message.trim(), "second message");

    // A repository that does not allow revision-property edits refuses.
    let without_hook = repo.set_revision_message(&revision, "changed").unwrap_err();
    assert!(format!("{without_hook}").contains("E165006"), "{without_hook}");

    // With the hook, the message is replaced and reads back changed.
    let hook = repository.join("hooks/pre-revprop-change");
    write(&hook, "#!/bin/sh\nexit 0\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook, perms).unwrap();
    }
    repo.set_revision_message(&revision, "changed message").unwrap();
    assert_eq!(repo.log_details(&revision).unwrap().message.trim(), "changed message");
}
