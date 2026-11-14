//! End-to-end tests against a real `svn` working copy.
//!
//! The tests are skipped (with a note on stderr) when `svn` and `svnadmin` are not installed.

use ide_vcs::{ChangeKind, FileStatus, HunkKind, LineHunk, LogQuery};
use ide_vcs_svn::SvnRepo;
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
