//! Parsing of unified diffs (`svn diff`, `diff -u`, `git diff`) into [`LineHunk`]s.
//!
//! Only the hunks are of interest here: file headers (`Index:`, `--- a/x`, `+++ b/x`,
//! `===================================================================`) and other lines outside a
//! hunk are skipped. Hunk headers are 1-based; the hunks are 0-based like the IPC types.

use ide_vcs::{HunkKind, LineHunk};

/// `-1,3` / `+1` / `-0,0`: the (start, count) of one side of a hunk header. A missing count
/// means one line.
fn range(spec: &str) -> Option<(u32, u32)> {
    let digits = spec.strip_prefix('-').or_else(|| spec.strip_prefix('+'))?;
    let (start, count) = match digits.split_once(',') {
        Some((start, count)) => (start, count),
        None => (digits, "1"),
    };
    Some((start.parse().ok()?, count.parse().ok()?))
}

/// A hunk while its body is being read.
struct Hunk {
    old_start: u32,
    new_start: u32,
    /// Body lines still expected (`old_count + new_count`).
    remaining: u32,
    context: u32,
    added: u32,
    removed: u32,
    old_text: Vec<String>,
    /// The hunk ended on a line that is not part of a body.
    closed: bool,
}

impl Hunk {
    fn header(line: &str) -> Option<Self> {
        let rest = line.strip_prefix("@@")?;
        let end = rest.find("@@")?;
        let mut parts = rest[..end].split_whitespace();
        let (old_start, old_count) = range(parts.next()?)?;
        let (new_start, new_count) = range(parts.next()?)?;
        Some(Self {
            // A zero-count range names the line before the hunk, not line 1 of it.
            old_start: if old_count == 0 { old_start } else { old_start.saturating_sub(1) },
            new_start: if new_count == 0 { new_start } else { new_start.saturating_sub(1) },
            remaining: old_count.saturating_add(new_count),
            context: 0,
            added: 0,
            removed: 0,
            old_text: Vec::new(),
            closed: false,
        })
    }

    fn context(&mut self) {
        self.context = self.context.saturating_add(1);
        // A context line belongs to both sides, so it uses up one old and one new line.
        self.remaining = self.remaining.saturating_sub(2);
    }

    fn add(&mut self) {
        self.added = self.added.saturating_add(1);
        self.remaining = self.remaining.saturating_sub(1);
    }

    fn remove(&mut self, text: &str) {
        self.removed = self.removed.saturating_add(1);
        self.old_text.push(text.to_string());
        self.remaining = self.remaining.saturating_sub(1);
    }

    fn finish(self) -> LineHunk {
        let kind = match (self.added, self.removed) {
            (0, 0) => HunkKind::Modified,
            (_, 0) => HunkKind::Added,
            (0, _) => HunkKind::Deleted,
            _ => HunkKind::Modified,
        };
        LineHunk {
            kind,
            start: self.new_start,
            lines: self.context.saturating_add(self.added),
            old_start: self.old_start,
            old_lines: self.context.saturating_add(self.removed),
            old_text: self.old_text.join("\n"),
        }
    }
}

/// The hunks of a unified diff, in file order.
///
/// A hunk closes as soon as its old and new line counts are used up, so a following
/// `--- a/x` file header is never taken for a removed line. `\ No newline at end of file`
/// marks the preceding line instead of being a line of its own.
pub fn parse_unified(text: &str) -> Vec<LineHunk> {
    let mut hunks = Vec::new();
    let mut open: Option<Hunk> = None;
    for line in text.lines() {
        if line.starts_with("@@ -")
            && let Some(hunk) = Hunk::header(line)
        {
            hunks.extend(open.take().map(Hunk::finish));
            open = Some(hunk);
            continue;
        }
        let Some(hunk) = open.as_mut() else { continue };
        match line.as_bytes().first() {
            Some(b' ') | None => hunk.context(),
            Some(b'+') => hunk.add(),
            Some(b'-') => hunk.remove(&line[1..]),
            // `\ No newline at end of file`: a note about the previous line.
            Some(b'\\') => {}
            // Not a body line: the diff was cut short (or is malformed); keep what was read.
            Some(_) => hunk.closed = true,
        }
        let done = hunk.remaining == 0 || hunk.closed;
        if done {
            hunks.extend(open.take().map(Hunk::finish));
        }
    }
    hunks.extend(open.take().map(Hunk::finish));
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`LineHunk`] has no `PartialEq`; compares by value instead.
    fn parts(hunk: &LineHunk) -> (HunkKind, u32, u32, u32, u32, String) {
        (hunk.kind, hunk.start, hunk.lines, hunk.old_start, hunk.old_lines, hunk.old_text.clone())
    }

    #[track_caller]
    fn assert_hunks(text: &str, expected: &[(HunkKind, u32, u32, u32, u32, &str)]) {
        let actual: Vec<_> = parse_unified(text).iter().map(parts).collect();
        let expected: Vec<_> = expected
            .iter()
            .map(|&(kind, start, lines, old_start, old_lines, old_text)| {
                (kind, start, lines, old_start, old_lines, old_text.to_string())
            })
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn reads_hunks_of_a_multi_file_diff() {
        let diff = "\
Index: a.c
===================================================================
--- a.c\t(revision 3)
+++ a.c\t(working copy)
@@ -1,3 +1,3 @@
 int main(void) {
-  return 0;
+  return 1;
 }
Index: sub/b.txt
===================================================================
--- sub/b.txt\t(revision 3)
+++ sub/b.txt\t(working copy)
@@ -10,1 +10,3 @@
 keep
+more
+again
";
        assert_hunks(diff, &[(HunkKind::Modified, 0, 3, 0, 3, "  return 0;"), (HunkKind::Added, 9, 3, 9, 1, "")]);
    }

    #[test]
    fn reads_empty_ranges() {
        assert_hunks(
            "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+one\n+two\n",
            &[(HunkKind::Added, 0, 2, 0, 0, "")],
        );
        assert_hunks(
            "--- a/old.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-one\n-two\n",
            &[(HunkKind::Deleted, 0, 0, 0, 2, "one\ntwo")],
        );
    }

    #[test]
    fn missing_counts_mean_one_line() {
        assert_hunks("--- a/x\n+++ b/x\n@@ -5 +7 @@\n-old\n+new\n", &[(HunkKind::Modified, 6, 1, 4, 1, "old")]);
    }

    #[test]
    fn ignores_no_newline_markers() {
        let diff = "\
--- a/x
+++ b/x
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
--- a/y
+++ b/y
@@ -1,2 +1,2 @@
 keep
-gone
+here
";
        assert_hunks(diff, &[(HunkKind::Modified, 0, 1, 0, 1, "old"), (HunkKind::Modified, 0, 2, 0, 2, "gone")]);
    }

    #[test]
    fn added_file_has_no_old_text() {
        let diff = "Index: sub/c.txt\n===================================================================\n\
--- sub/c.txt\t(nonexistent)\n+++ sub/c.txt\t(working copy)\n@@ -0,0 +1 @@\n+y\n";
        assert_hunks(diff, &[(HunkKind::Added, 0, 1, 0, 0, "")]);
    }

    #[test]
    fn ignores_text_outside_hunks() {
        assert!(parse_unified("").is_empty());
        assert!(parse_unified("no diff here\n").is_empty());
        assert!(parse_unified("--- a/x\n+++ b/x\n").is_empty());
    }
}
