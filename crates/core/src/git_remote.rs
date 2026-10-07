//! Repository identity for projects with a folder on more than one machine
//! (docs/repo-machines.md). `normalizeGitRemoteUrl` and the remote choice
//! must give the same answer as the TypeScript app.

/// `normalizeGitRemoteUrl`: the canonical repository key of a remote URL.
/// Two clones of one repository give the same key whatever the protocol,
/// user, port, or `.git` suffix.
pub fn normalize_git_remote_url(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let trimmed = trimmed.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let trimmed = trimmed.trim_end_matches('/');
    let located = if let Some(rest) = strip_scheme(trimmed) {
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = host.split(':').next().unwrap_or(host);
        format!("{host}{path}")
    } else if let Some((host, path)) = scp_parts(trimmed) {
        format!("{host}/{}", path.trim_start_matches('/'))
    } else {
        format!("file:{}", trimmed.replace('\\', "/"))
    };
    azure_devops(&located).unwrap_or(located).to_lowercase()
}

/// What follows `scheme://`, when the URL has a scheme.
fn strip_scheme(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    let mut chars = scheme.chars();
    let valid = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|char| char.is_ascii_alphanumeric() || matches!(char, '+' | '.' | '-'));
    valid.then_some(rest)
}

/// The host and path of the scp form `[user@]host:path`. A single letter
/// before the colon is a Windows drive.
fn scp_parts(url: &str) -> Option<(&str, &str)> {
    let (before, path) = url.split_once(':')?;
    if before.contains('/') || before.contains('\\') {
        return None;
    }
    let host = before.rsplit('@').next().unwrap_or(before);
    if host.is_empty() || (host.len() == 1 && host.as_bytes()[0].is_ascii_alphabetic()) {
        return None;
    }
    Some((host, path))
}

/// Azure DevOps SSH and legacy `visualstudio.com` URLs in their HTTPS form.
fn azure_devops(located: &str) -> Option<String> {
    let parts: Vec<&str> = located.split('/').collect();
    let host = parts.first()?.to_ascii_lowercase();
    if host == "ssh.dev.azure.com" {
        if let [_, version, org, project, repo] = parts.as_slice()
            && version.eq_ignore_ascii_case("v3")
        {
            return Some(format!("dev.azure.com/{org}/{project}/_git/{repo}"));
        }
        return None;
    }
    let org = host.strip_suffix(".visualstudio.com")?;
    if let [_, project, git, repo] = parts.as_slice()
        && git.eq_ignore_ascii_case("_git")
        && !org.is_empty()
    {
        return Some(format!("dev.azure.com/{org}/{project}/_git/{repo}"));
    }
    None
}

/// The remote whose URL names the repository: `origin`, else `upstream`,
/// else the first remote in name order.
pub fn pick_git_remote<S: AsRef<str>>(names: &[S]) -> Option<String> {
    let names: Vec<&str> = names
        .iter()
        .map(|name| name.as_ref().trim())
        .filter(|name| !name.is_empty())
        .collect();
    for preferred in ["origin", "upstream"] {
        if names.contains(&preferred) {
            return Some(preferred.into());
        }
    }
    names.into_iter().min().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_shared_test_vectors() {
        for (input, output) in [
            (
                "git@github.com:T3Tools/T3Code.git",
                "github.com/t3tools/t3code",
            ),
            (
                "https://github.com/T3Tools/T3Code.git",
                "github.com/t3tools/t3code",
            ),
            ("https://user@github.com/a/b/", "github.com/a/b"),
            ("ssh://git@github.com:22/a/b.git", "github.com/a/b"),
            ("git://example.com/a/b", "example.com/a/b"),
            (
                "ssh://git@gitlab.example.com:2222/group/sub/repo.git",
                "gitlab.example.com/group/sub/repo",
            ),
            (
                "git@ssh.dev.azure.com:v3/Org/Proj/Repo",
                "dev.azure.com/org/proj/_git/repo",
            ),
            (
                "https://Org@dev.azure.com/Org/Proj/_git/Repo",
                "dev.azure.com/org/proj/_git/repo",
            ),
            (
                "https://org.visualstudio.com/Proj/_git/Repo",
                "dev.azure.com/org/proj/_git/repo",
            ),
            ("/srv/git/app.git", "file:/srv/git/app"),
            ("C:\\repos\\app", "file:c:/repos/app"),
            ("", ""),
        ] {
            assert_eq!(normalize_git_remote_url(input), output, "{input}");
        }
    }

    #[test]
    fn blank_input_has_no_identity() {
        assert_eq!(normalize_git_remote_url("   "), "");
    }

    #[test]
    fn prefers_origin_then_upstream_then_the_first_name() {
        assert_eq!(
            pick_git_remote(&["fork", "origin", "upstream"]).as_deref(),
            Some("origin")
        );
        assert_eq!(
            pick_git_remote(&["fork", "upstream"]).as_deref(),
            Some("upstream")
        );
        assert_eq!(
            pick_git_remote(&["zeta", "alpha"]).as_deref(),
            Some("alpha")
        );
        assert_eq!(pick_git_remote::<&str>(&[]), None);
    }
}
