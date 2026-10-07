//! Port of the project path helpers in src/features/projects/model/recents.ts
//! that persistence and history need: `normalizeProjectPath`,
//! `sameProjectPath`, `isRemoteProjectPath`, and `isLocalProject`. The
//! `projects` package owns the rest of recents.ts.

use monocode_core::paths::{path_key, slash};

/// `REMOTE_PROJECT_PREFIX`: paths on a connected machine use this scheme.
pub const REMOTE_PROJECT_PREFIX: &str = "remote://";

/// `normalizeProjectPath`: forward slashes, no trailing slash.
pub fn normalize_project_path(path: &str) -> String {
    let slashed = slash(path);
    let trimmed = slashed.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".into()
    } else {
        trimmed.to_string()
    }
}

/// `sameProjectPath`.
pub fn same_project_path(a: &str, b: &str) -> bool {
    path_key(a) == path_key(b)
}

/// `isRemoteProjectPath`.
pub fn is_remote_project_path(path: &str) -> bool {
    slash(path).starts_with(REMOTE_PROJECT_PREFIX)
}

/// `isLocalProject`: a project folder on this computer. Runtime cannot use
/// the layout crate's `pretty_cwd`, so the home check is inlined here.
pub fn is_local_project(path: &str) -> bool {
    if path.is_empty() || path == "/" || path == "~" || is_remote_project_path(path) {
        return false;
    }
    let normalized = normalize_project_path(path);
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let drive = |part: &str| {
        let bytes = part.as_bytes();
        bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
    };
    if normalized == "/" || drive(&normalized) {
        return false;
    }
    // Home itself arrives expanded (`/Users/me`), so the `~` check above
    // misses it. This matches `prettyCwd(path) === "~"`.
    let home = (parts.len() == 2 && (parts[0] == "Users" || parts[0] == "home"))
        || (parts.len() == 3 && drive(parts[0]) && parts[1] == "Users");
    !home && !path.contains(".app/") && !path.contains(".app\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_compares_project_paths() {
        assert_eq!(normalize_project_path("/repo/"), "/repo");
        assert_eq!(normalize_project_path("///"), "/");
        assert_eq!(normalize_project_path("C:\\repo\\"), "C:/repo");
        assert!(same_project_path("C:\\Repo", "c:/repo/"));
        assert!(!same_project_path("/Repo", "/repo"));
        assert!(is_remote_project_path("remote://env/home/me"));
        assert!(!is_remote_project_path("/home/me"));
    }
}
