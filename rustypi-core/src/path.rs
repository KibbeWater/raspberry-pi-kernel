// path.rs
//! `/`-separated paths on the SD card, relative to a current directory.

use alloc::string::String;
use alloc::vec::Vec;

/// `path` as an absolute path with no `.`, `..` or empty parts: from the root if it starts
/// with `/`, else from `cwd` (itself absolute). `..` at the root stays there.
pub fn resolve(cwd: &str, path: &str) -> String {
    let start = if path.starts_with('/') { "" } else { cwd };
    let mut parts: Vec<&str> = Vec::new();
    for part in start.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut resolved = String::with_capacity(path.len() + cwd.len() + 1);
    for part in &parts {
        resolved.push('/');
        resolved.push_str(part);
    }
    if resolved.is_empty() {
        resolved.push('/');
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_ignore_the_current_directory() {
        assert_eq!(resolve("/docs", "/bin/ls"), "/bin/ls");
        assert_eq!(resolve("/docs", "/"), "/");
    }

    #[test]
    fn relative_paths_start_at_the_current_directory() {
        assert_eq!(resolve("/", "bin"), "/bin");
        assert_eq!(resolve("/docs", "notes.txt"), "/docs/notes.txt");
        assert_eq!(resolve("/docs/a", "b/c"), "/docs/a/b/c");
        assert_eq!(resolve("/docs", ""), "/docs");
    }

    #[test]
    fn dots_and_extra_slashes_go() {
        assert_eq!(resolve("/docs", "."), "/docs");
        assert_eq!(resolve("/docs", ".."), "/");
        assert_eq!(resolve("/docs/a", "../b/./c/"), "/docs/b/c");
        assert_eq!(resolve("/", "//bin///ls"), "/bin/ls");
    }

    #[test]
    fn dot_dot_stops_at_the_root() {
        assert_eq!(resolve("/", ".."), "/");
        assert_eq!(resolve("/docs", "../../../bin"), "/bin");
    }

    #[test]
    fn names_with_dots_and_spaces_are_kept() {
        assert_eq!(resolve("/", "...hidden"), "/...hidden");
        assert_eq!(resolve("/my docs", "a file.txt"), "/my docs/a file.txt");
    }
}
