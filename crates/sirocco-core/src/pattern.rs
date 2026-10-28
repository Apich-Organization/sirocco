//! Shell-style wildcard matching for `--include` / `--exclude`.
//!
//! Supports `*` (any run within one path component), `?` (one character), and
//! `**` (any run, including `/`). Matching is case-insensitive on Windows and
//! case-sensitive elsewhere, following the filesystem.

use std::path::Path;

#[inline]
const fn fold(c: u8) -> u8 {
    if cfg!(windows) {
        c.to_ascii_lowercase()
    } else {
        c
    }
}

/// Match `text` against `pat`.
#[must_use]
pub fn matches(
    pat: &str,
    text: &str,
) -> bool {
    m(pat.as_bytes(), text.as_bytes())
}

fn m(
    pat: &[u8],
    text: &[u8],
) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }
    match pat[0] {
        | b'*' => {
            let (crosses, rest) = if pat.len() > 1 && pat[1] == b'*' {
                let mut r = &pat[2..];
                // `**/` also matches zero components, so drop the separator.
                if !r.is_empty() && r[0] == b'/' {
                    r = &r[1..];
                }
                (true, r)
            } else {
                (false, &pat[1..])
            };
            if rest.is_empty() {
                return crosses || !text.contains(&b'/');
            }
            for i in 0..=text.len() {
                if !crosses && i > 0 && text[i - 1] == b'/' {
                    break;
                }
                if m(rest, &text[i..]) {
                    return true;
                }
            }
            false
        },
        | b'?' => !text.is_empty() && text[0] != b'/' && m(&pat[1..], &text[1..]),
        | c => !text.is_empty() && fold(c) == fold(text[0]) && m(&pat[1..], &text[1..]),
    }
}

/// Match a relative path against a pattern (`\` is normalised to `/`).
///
/// A pattern without a `/` is also tried against the file name alone, so `*.log`
/// behaves the way people expect in an exclude list.
#[must_use]
pub fn matches_path(
    pat: &str,
    rel: &Path,
) -> bool {
    let s = rel.to_string_lossy().replace('\\', "/");
    if matches(pat, &s) {
        return true;
    }
    if !pat.contains('/')
        && let Some(name) = s.rsplit('/').next()
    {
        return matches(pat, name);
    }
    false
}

/// True when any pattern matches.
#[must_use]
pub fn any_match(
    pats: &[String],
    rel: &Path,
) -> bool {
    pats.iter().any(|p| matches_path(p, rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert!(matches("*.txt", "a.txt"));
        assert!(!matches("*.txt", "dir/a.txt"));
        assert!(matches_path("*.txt", Path::new("dir/a.txt")));
        assert!(!matches("*.txt", "a.txt.bak"));
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(matches("**/node_modules/**", "src/node_modules/x/y.js"));
        assert!(matches("**/*.rs", "crates/a/src/lib.rs"));
        assert!(matches("src/*", "src/main.rs"));
        assert!(!matches("src/*", "src/sub/main.rs"));
        assert!(matches("**", "anything/at/all"));
        assert!(matches("*", "single"));
        assert!(!matches("*", "a/b"));
        assert!(matches("exact.txt", "exact.txt"));
        assert!(matches("a/**/z", "a/z"));
        assert!(matches("a/**/z", "a/b/c/z"));
        assert!(matches("**/target/**", "target/debug/x"));
    }

    #[test]
    fn no_false_positives() {
        assert!(!matches("*.rs", "main.py"));
        assert!(!matches("a/*/z", "a/z"));
        assert!(!matches("abc", "abcd"));
        assert!(!matches("abcd", "abc"));
    }
}
