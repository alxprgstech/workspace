//! Strict unified-diff application.
//!
//! Only the subset produced by `git diff` / `diff -u` is accepted: context
//! lines, removals and additions. Every hunk must match the current file
//! exactly, so a stale patch fails with a conflict instead of corrupting the
//! file. No fuzz, no offsets.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PatchError {
    #[error("patch is empty")]
    Empty,

    #[error("patch has no hunks")]
    NoHunks,

    #[error("malformed hunk header on line {0}")]
    Header(usize),

    #[error("hunk does not match the file (line {0})")]
    Conflict(usize),

    #[error("patch line {0} has an unknown prefix")]
    UnknownLine(usize),
}

/// Apply a unified diff to `original`.
pub fn apply(original: &str, patch: &str) -> Result<String, PatchError> {
    if patch.trim().is_empty() {
        return Err(PatchError::Empty);
    }

    // Patches are always written with LF, but files edited on Windows may use
    // CRLF. Normalise for matching and restore the file's convention at the end.
    let crlf = original.contains("\r\n");
    let normalised;

    let original = if crlf {
        normalised = original.replace("\r\n", "\n");
        normalised.as_str()
    } else {
        original
    };

    let original_lines: Vec<&str> = split_lines(original);
    let mut output: Vec<String> = Vec::with_capacity(original_lines.len());
    let mut cursor = 0usize;
    let mut saw_hunk = false;

    let mut lines = patch.lines().enumerate().peekable();

    while let Some((index, line)) = lines.next() {
        if !line.starts_with("@@") {
            // File headers, index lines and diff preambles are ignored.
            continue;
        }

        saw_hunk = true;

        let (old_start, _old_count) = parse_header(line, index + 1)?;

        let target = old_start.saturating_sub(1);

        if target < cursor || target > original_lines.len() {
            return Err(PatchError::Conflict(index + 1));
        }

        while cursor < target {
            output.push(original_lines[cursor].to_owned());
            cursor += 1;
        }

        // Apply the hunk body.
        while let Some((body_index, body)) = lines.peek().copied() {
            if body.starts_with("@@") || body.starts_with("diff ") {
                break;
            }

            let _ = lines.next();

            if body.is_empty() {
                // Blank separator between hunks.
                continue;
            }

            let Some((prefix, content)) = split_prefix(body) else {
                return Err(PatchError::UnknownLine(body_index + 1));
            };

            match prefix {
                ' ' => {
                    if original_lines.get(cursor) != Some(&content) {
                        return Err(PatchError::Conflict(body_index + 1));
                    }

                    output.push(content.to_owned());
                    cursor += 1;
                }
                '-' => {
                    if original_lines.get(cursor) != Some(&content) {
                        return Err(PatchError::Conflict(body_index + 1));
                    }

                    cursor += 1;
                }
                '+' => output.push(content.to_owned()),
                '\\' => {
                    // "\ No newline at end of file" is handled after the loop.
                }
                _ => return Err(PatchError::UnknownLine(body_index + 1)),
            }
        }
    }

    if !saw_hunk {
        return Err(PatchError::NoHunks);
    }

    while cursor < original_lines.len() {
        output.push(original_lines[cursor].to_owned());
        cursor += 1;
    }

    let mut result = output.join("\n");

    // Preserve the original's final-newline convention.
    if original.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }

    if patch.contains("\\ No newline at end of file") {
        while result.ends_with('\n') {
            result.pop();
        }
    }

    if crlf {
        result = result.replace('\n', "\r\n");
    }

    Ok(result)
}

fn parse_header(line: &str, number: usize) -> Result<(usize, usize), PatchError> {
    // @@ -1,3 +1,4 @@
    let mut parts = line.split_whitespace();

    if parts.next() != Some("@@") {
        return Err(PatchError::Header(number));
    }

    let old = parts.next().ok_or(PatchError::Header(number))?;

    let old = old.strip_prefix('-').ok_or(PatchError::Header(number))?;

    let (start, count) = match old.split_once(',') {
        Some((start, count)) => (start, count),
        None => (old, "1"),
    };

    let start: usize = start.parse().map_err(|_| PatchError::Header(number))?;
    let count: usize = count.parse().map_err(|_| PatchError::Header(number))?;

    Ok((start, count))
}

fn split_prefix(line: &str) -> Option<(char, &str)> {
    let mut characters = line.chars();
    let prefix = characters.next()?;

    Some((prefix, characters.as_str()))
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<&str> = text.split('\n').collect();

    // A trailing newline produces an empty last element that is not a line.
    if lines.last() == Some(&"") {
        lines.pop();
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = "one\ntwo\nthree\nfour\n";

    #[test]
    fn applies_a_simple_patch() {
        let patch = "--- a/file\n+++ b/file\n@@ -2,2 +2,2 @@\n two\n-three\n+THREE\n";

        assert_eq!(apply(ORIGINAL, patch).unwrap(), "one\ntwo\nTHREE\nfour\n");
    }

    #[test]
    fn applies_multiple_hunks() {
        let patch = "@@ -1,1 +1,1 @@\n-one\n+ONE\n@@ -4,1 +4,1 @@\n-four\n+FOUR\n";

        assert_eq!(apply(ORIGINAL, patch).unwrap(), "ONE\ntwo\nthree\nFOUR\n");
    }

    #[test]
    fn adds_lines_without_touching_context_below() {
        let patch = "@@ -1,0 +1,1 @@\n+zero\n";

        // A zero-length old range at the start inserts before the first line.
        assert_eq!(
            apply(ORIGINAL, patch).unwrap(),
            "zero\none\ntwo\nthree\nfour\n"
        );
    }

    #[test]
    fn refuses_a_stale_patch() {
        let patch = "@@ -2,1 +2,1 @@\n-WRONG\n+RIGHT\n";

        assert!(matches!(
            apply(ORIGINAL, patch),
            Err(PatchError::Conflict(_))
        ));
    }

    #[test]
    fn refuses_malformed_input() {
        assert_eq!(apply(ORIGINAL, "").unwrap_err(), PatchError::Empty);
        assert_eq!(
            apply(ORIGINAL, "--- a\n+++ b\n").unwrap_err(),
            PatchError::NoHunks
        );
        assert!(matches!(
            apply(ORIGINAL, "@@ nope @@\n"),
            Err(PatchError::Header(_))
        ));
    }

    #[test]
    fn applies_to_crlf_files_and_keeps_crlf() {
        let patch = "@@ -1,3 +1,3 @@\n [package]\n name = \"demo\"\n-version = \"0.3.0\"\n+version = \"0.4.0\"\n";

        let result = apply(
            "[package]\r\nname = \"demo\"\r\nversion = \"0.3.0\"\r\n",
            patch,
        )
        .unwrap();

        assert_eq!(
            result,
            "[package]\r\nname = \"demo\"\r\nversion = \"0.4.0\"\r\n"
        );
    }

    #[test]
    fn keeps_a_missing_final_newline() {
        let patch = "@@ -1,1 +1,1 @@\n-hello\n+world\n\\ No newline at end of file\n";

        assert_eq!(apply("hello", patch).unwrap(), "world");
    }
}
