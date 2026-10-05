//! Port of the managed permission helpers in opencodeProtocol.ts: the
//! session rules for an access mode, the server configuration that overrides
//! every agent's own rules, and the check that the server applied them.
//!
//! OpenCode reads permission objects in key order and the last matching rule
//! wins, so the configuration is written as JSON text with its keys in the
//! order the TypeScript inserted them. This workspace's `serde_json::Map` may
//! sort keys, so it is not used for the output.

use std::sync::LazyLock;

use anyhow::{Result, anyhow, bail};
use monocode_core::harness::RuntimeMode;
use regex::Regex;
use serde_json::Value;

use super::protocol::{OpenCodePermissionRule, PermissionAction};

/// The tools a Plan turn may use without asking.
const PLAN_READ_ONLY: [&str; 6] = ["read", "grep", "glob", "list", "websearch", "codesearch"];

const POLICY_CONFLICT: &str = "OpenCode configuration grants tools beyond the selected access mode. Remove conflicting agent permissions or experimental.primary_tools settings.";

static AGENT_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+)\s+\((primary|subagent|all)\)\s*$").unwrap());
static DATA_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^data[ \t]+(.+)$").unwrap());
static WINDOWS_DRIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z]:[\\/]").unwrap());
static WINDOWS_SHARE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\\\\[^\\/]+[\\/][^\\/]+").unwrap());
static UNSAFE_PATH_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\x00-\x1f*?\[\]{}]").unwrap());

/// `parseOpenCodeToolOutputGlob`: the `tool-output` glob under the data
/// directory `opencode debug paths` reports. OpenCode saves truncated tool
/// output there, and a restricted session must still read it back.
pub fn parse_open_code_tool_output_glob(output: &str) -> Result<String> {
    let paths: Vec<&str> = output
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter_map(|line| DATA_LINE.captures(line))
        .filter_map(|captures| captures.get(1).map(|path| path.as_str()))
        .collect();
    let unsafe_dir =
        || anyhow!("OpenCode did not expose a safe data directory for its tool output.");
    let [path] = paths.as_slice() else {
        return Err(unsafe_dir());
    };
    let windows = WINDOWS_DRIVE.is_match(path) || WINDOWS_SHARE.is_match(path);
    let parts: Vec<&str> = if windows {
        path.split(['\\', '/']).collect()
    } else {
        path.split('/').collect()
    };
    if path.is_empty()
        || path.trim() != *path
        || UNSAFE_PATH_CHARS.is_match(path)
        || !(path.starts_with('/') || windows)
        || parts.iter().any(|part| *part == "." || *part == "..")
    {
        return Err(unsafe_dir());
    }
    let separator = if windows && path.contains('\\') {
        '\\'
    } else {
        '/'
    };
    let directory = path.trim_end_matches(separator);
    Ok(format!("{directory}{separator}tool-output{separator}*"))
}

/// `buildOpenCodePermissionRules`: the session permission rules for an
/// access mode. Questions are always allowed so the agent can ask. A Plan
/// turn denies everything except reading, searching, and Explore tasks.
pub fn build_open_code_permission_rules(
    runtime_mode: RuntimeMode,
    planning: bool,
    tool_output_glob: Option<&str>,
) -> Vec<OpenCodePermissionRule> {
    use PermissionAction::*;
    let rule = |permission: &str, pattern: &str, action| OpenCodePermissionRule {
        permission: permission.into(),
        pattern: pattern.into(),
        action,
    };
    let output_rules: Vec<OpenCodePermissionRule> = tool_output_glob
        .map(|glob| rule("external_directory", glob, Allow))
        .into_iter()
        .collect();
    if planning {
        let mut rules = vec![rule("*", "*", Deny)];
        rules.extend(
            PLAN_READ_ONLY
                .iter()
                .chain(&["question"])
                .map(|permission| rule(permission, "*", Allow)),
        );
        rules.extend(output_rules);
        rules.push(rule("task", "explore", Allow));
        return rules;
    }
    if runtime_mode == RuntimeMode::FullAccess {
        return vec![rule("*", "*", Allow)];
    }
    let mut rules = vec![rule("*", "*", Ask), rule("question", "*", Allow)];
    rules.extend(output_rules);
    if matches!(
        runtime_mode,
        RuntimeMode::AutoAcceptEdits | RuntimeMode::Auto
    ) {
        rules.push(rule("edit", "*", Allow));
    }
    if runtime_mode == RuntimeMode::Auto {
        rules.push(rule("read", "*", Allow));
    }
    rules
}

/// A JSON value whose objects keep their insertion order.
#[derive(Debug, Clone)]
enum Ordered {
    Text(&'static str),
    Object(Vec<(String, Ordered)>),
    EmptyArray,
}

impl Ordered {
    /// `object[key] = value`: replace in place, or append.
    fn set(&mut self, key: &str, value: Ordered) {
        let Ordered::Object(entries) = self else {
            return;
        };
        match entries.iter_mut().find(|(name, _)| name == key) {
            Some(entry) => entry.1 = value,
            None => entries.push((key.to_string(), value)),
        }
    }

    fn write(&self, out: &mut String) {
        match self {
            Ordered::Text(text) => out.push_str(&Value::from(*text).to_string()),
            Ordered::EmptyArray => out.push_str("[]"),
            Ordered::Object(entries) => {
                out.push('{');
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::from(key.as_str()).to_string());
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn action_text(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::Allow => "allow",
        PermissionAction::Ask => "ask",
        PermissionAction::Deny => "deny",
    }
}

/// One agent from `opencode agent list`, with its effective rule names.
struct ListedAgent {
    name: String,
    primary: bool,
    permissions: Vec<String>,
    external_patterns: Vec<String>,
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

/// Read the agents and their rules from `opencode agent list`.
fn listed_agents(output: &str) -> Result<Vec<ListedAgent>> {
    let mut agents: Vec<ListedAgent> = Vec::new();
    let mut current: Option<(String, String, Vec<&str>)> = None;
    let mut flush = |current: Option<(String, String, Vec<&str>)>| -> Result<()> {
        let Some((name, mode, lines)) = current else {
            return Ok(());
        };
        let rules: Value = serde_json::from_str(&lines.join("\n"))
            .map_err(|_| anyhow!("Could not read OpenCode permissions for agent {name}"))?;
        let Some(rules) = rules.as_array() else {
            bail!("OpenCode did not expose permissions for agent {name}");
        };
        let mut permissions = Vec::new();
        let mut external_patterns = Vec::new();
        for rule in rules {
            let text = |key: &str| {
                rule.get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
            };
            if let Some(permission) = text("permission") {
                push_unique(&mut permissions, permission);
            }
            if rule.get("permission").and_then(Value::as_str) == Some("external_directory")
                && let Some(pattern) = text("pattern").filter(|pattern| *pattern != "*")
            {
                push_unique(&mut external_patterns, pattern);
            }
        }
        let agent = ListedAgent {
            name: name.clone(),
            primary: mode == "primary",
            permissions,
            external_patterns,
        };
        // A repeated name keeps its first position, as a `Map` does.
        match agents.iter_mut().find(|existing| existing.name == name) {
            Some(existing) => {
                existing.permissions = agent.permissions;
                existing.external_patterns = agent.external_patterns;
                existing.primary |= agent.primary;
            }
            None => agents.push(agent),
        }
        Ok(())
    };
    for line in output.split('\n') {
        if let Some(header) = AGENT_HEADER.captures(line) {
            flush(current.take())?;
            current = Some((header[1].to_string(), header[2].to_string(), Vec::new()));
        } else if let Some((_, _, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    flush(current.take())?;
    if agents.is_empty() {
        bail!("OpenCode did not expose its agent permission rules");
    }
    Ok(agents)
}

/// `managedOpenCodeConfig`: the `OPENCODE_CONFIG_CONTENT` that overrides
/// every permission key the effective agent rules mention, as JSON text.
/// Custom keys need their own override, or an agent's allow survives the
/// managed baseline.
pub fn managed_open_code_config(
    agent_list_output: &str,
    runtime_mode: RuntimeMode,
    planning: bool,
    tool_output_glob: Option<&str>,
) -> Result<String> {
    let agents = listed_agents(agent_list_output)?;
    let action_for = |permission: &str| -> &'static str {
        if permission == "question" {
            return "allow";
        }
        if planning {
            return if PLAN_READ_ONLY.contains(&permission) {
                "allow"
            } else {
                "deny"
            };
        }
        if runtime_mode == RuntimeMode::FullAccess {
            return "allow";
        }
        if permission == "edit"
            && matches!(
                runtime_mode,
                RuntimeMode::Auto | RuntimeMode::AutoAcceptEdits
            )
        {
            return "allow";
        }
        if permission == "read" && runtime_mode == RuntimeMode::Auto {
            return "allow";
        }
        "ask"
    };
    let permissions = |keys: &[String], patterns: &[String], allow_explore_task: bool| {
        let mut policy = Ordered::Object(
            keys.iter()
                .map(|key| (key.clone(), Ordered::Text(action_for(key))))
                .collect(),
        );
        for rule in build_open_code_permission_rules(runtime_mode, planning, None) {
            if rule.pattern == "*" {
                policy.set(&rule.permission, Ordered::Text(action_text(rule.action)));
            }
        }
        // TaskTool uses the target agent's own rules for child sessions. A
        // scalar deny replaces inherited scoped grants instead of leaving
        // Explore enabled.
        if planning {
            policy.set(
                "task",
                if allow_explore_task {
                    Ordered::Object(vec![
                        ("*".into(), Ordered::Text("deny")),
                        ("explore".into(), Ordered::Text("allow")),
                    ])
                } else {
                    Ordered::Text("deny")
                },
            );
        }
        if planning || runtime_mode != RuntimeMode::FullAccess {
            let external = action_for("external_directory");
            let mut directories = Ordered::Object(vec![("*".into(), Ordered::Text(external))]);
            for pattern in patterns {
                directories.set(pattern, Ordered::Text(external));
            }
            // Only the host's data directory can bypass the external-path
            // baseline.
            if let Some(glob) = tool_output_glob {
                directories.set(glob, Ordered::Text("allow"));
            }
            policy.set("external_directory", directories);
        }
        policy
    };
    let mut all_keys = Vec::new();
    let mut all_patterns = Vec::new();
    for agent in &agents {
        for key in &agent.permissions {
            push_unique(&mut all_keys, key);
        }
        for pattern in &agent.external_patterns {
            push_unique(&mut all_patterns, pattern);
        }
    }
    let wrap = |permission: Ordered| Ordered::Object(vec![("permission".into(), permission)]);
    let mut config = vec![
        (
            "permission".to_string(),
            permissions(&all_keys, &all_patterns, false),
        ),
        (
            "agent".to_string(),
            Ordered::Object(
                agents
                    .iter()
                    .map(|agent| {
                        let rules = permissions(
                            &agent.permissions,
                            &agent.external_patterns,
                            agent.primary && agent.name != "explore",
                        );
                        (agent.name.clone(), wrap(rules))
                    })
                    .collect(),
            ),
        ),
        (
            "mode".to_string(),
            Ordered::Object(
                agents
                    .iter()
                    .filter(|agent| agent.primary)
                    .map(|agent| {
                        let rules = permissions(
                            &agent.permissions,
                            &agent.external_patterns,
                            agent.name != "explore",
                        );
                        (agent.name.clone(), wrap(rules))
                    })
                    .collect(),
            ),
        ),
    ];
    if planning || runtime_mode != RuntimeMode::FullAccess {
        config.push((
            "experimental".into(),
            Ordered::Object(vec![("primary_tools".into(), Ordered::EmptyArray)]),
        ));
    }
    let mut out = String::new();
    Ordered::Object(config).write(&mut out);
    Ok(out)
}

/// `verifyManagedOpenCodePolicy`: fail when the server's effective agents
/// (`GET /agent`) or config (`GET /config`) still grant more than the
/// selected access mode, for example through a higher-priority project
/// config that the managed policy could not override.
pub fn verify_managed_open_code_policy(
    agents: &Value,
    config: &Value,
    runtime_mode: RuntimeMode,
    planning: bool,
    tool_output_glob: Option<&str>,
) -> Result<()> {
    if !planning && runtime_mode == RuntimeMode::FullAccess {
        return Ok(());
    }
    let conflict = || anyhow!(POLICY_CONFLICT);
    let config = config.as_object().ok_or_else(conflict)?;
    if let Some(primary_tools) = config
        .get("experimental")
        .and_then(Value::as_object)
        .and_then(|experimental| experimental.get("primary_tools"))
        && !primary_tools.as_array().is_some_and(Vec::is_empty)
    {
        return Err(conflict());
    }
    let agents = agents
        .as_array()
        .filter(|agents| !agents.is_empty())
        .ok_or_else(conflict)?;
    for agent in agents {
        let text = |value: &Value, key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        };
        // TaskTool can select Explore by name even if config changes its mode.
        let allow_explore_task = agent.get("mode").and_then(Value::as_str) == Some("primary")
            && agent.get("name").and_then(Value::as_str) != Some("explore");
        let permissions = agent
            .get("permission")
            .and_then(Value::as_array)
            .ok_or_else(conflict)?;
        let mut rules: Vec<(String, String, String)> = Vec::with_capacity(permissions.len());
        for rule in permissions {
            let (Some(permission), Some(pattern), Some(action)) = (
                text(rule, "permission"),
                text(rule, "pattern"),
                text(rule, "action"),
            ) else {
                return Err(conflict());
            };
            if !matches!(action.as_str(), "allow" | "ask" | "deny") {
                return Err(conflict());
            }
            rules.push((permission, pattern, action));
        }
        let baseline = rules
            .iter()
            .rposition(|(permission, pattern, _)| permission == "*" && pattern == "*")
            .ok_or_else(conflict)?;
        let baseline_action = &rules[baseline].2;
        if baseline_action == "allow" || (planning && baseline_action != "deny") {
            return Err(conflict());
        }
        let mut later: Vec<&(String, String, String)> = Vec::new();
        for rule in rules[baseline + 1..].iter().rev() {
            let (permission, pattern, action) = rule;
            let covered = later.iter().any(|(over_permission, over_pattern, _)| {
                (over_permission == "*" || over_permission == permission)
                    && (over_pattern == "*" || over_pattern == pattern)
            });
            if !covered && action != "deny" {
                let allowed = permission == "question"
                    || (permission == "external_directory"
                        && tool_output_glob == Some(pattern.as_str()))
                    || (planning
                        && (PLAN_READ_ONLY.contains(&permission.as_str())
                            || (allow_explore_task
                                && permission == "task"
                                && pattern == "explore")))
                    || (!planning
                        && ((permission == "edit"
                            && matches!(
                                runtime_mode,
                                RuntimeMode::Auto | RuntimeMode::AutoAcceptEdits
                            ))
                            || (permission == "read" && runtime_mode == RuntimeMode::Auto)));
                if !allowed && (planning || action == "allow") {
                    return Err(conflict());
                }
            }
            later.push(rule);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(permission: &str, pattern: &str, action: &str) -> Value {
        json!({ "permission": permission, "pattern": pattern, "action": action })
    }

    fn verify(permission: Value, planning: bool, config: Value) -> Result<()> {
        verify_managed_open_code_policy(
            &json!([{ "name": "custom", "mode": "primary", "permission": permission }]),
            &config,
            RuntimeMode::Supervised,
            planning,
            None,
        )
    }

    fn conflicts(result: Result<()>) -> bool {
        result.is_err_and(|error| error.to_string().contains("grants tools beyond"))
    }

    fn config(output: &str, mode: RuntimeMode, planning: bool, glob: Option<&str>) -> Value {
        serde_json::from_str(&managed_open_code_config(output, mode, planning, glob).unwrap())
            .unwrap()
    }

    // describe("effective managed permissions")

    #[test]
    fn rejects_a_higher_priority_custom_allow_after_the_managed_ask_rule() {
        assert!(conflicts(verify(
            json!([rule("*", "*", "ask"), rule("mcp_write", "*", "allow")]),
            false,
            json!({}),
        )));
    }

    #[test]
    fn accepts_stricter_rules_that_cover_the_whole_earlier_allow() {
        verify(
            json!([
                rule("*", "*", "ask"),
                rule("mcp_write", "*", "allow"),
                rule("mcp_write", "*", "deny"),
            ]),
            false,
            json!({}),
        )
        .unwrap();
    }

    #[test]
    fn rejects_narrow_wildcard_denies_that_do_not_cover_an_earlier_broad_allow() {
        assert!(conflicts(verify(
            json!([
                rule("*", "*", "ask"),
                rule("mcp_write", "foo*", "allow"),
                rule("mcp_write", "foo?", "deny"),
            ]),
            false,
            json!({}),
        )));
    }

    #[test]
    fn allows_only_exact_readonly_plan_exceptions_and_the_explore_task() {
        verify(
            json!([
                rule("*", "*", "deny"),
                rule("read", "*", "allow"),
                rule("task", "explore", "allow"),
            ]),
            true,
            json!({}),
        )
        .unwrap();
        assert!(conflicts(verify(
            json!([rule("*", "*", "deny"), rule("task", "explore*", "allow")]),
            true,
            json!({}),
        )));
        assert!(conflicts(verify(
            json!([
                rule("*", "*", "deny"),
                rule("search_and_delete", "*", "allow")
            ]),
            true,
            json!({}),
        )));
    }

    #[test]
    fn requires_a_safe_broad_baseline_and_the_ordered_rule_array() {
        assert!(conflicts(verify(
            json!([rule("read", "*", "deny")]),
            false,
            json!({})
        )));
        assert!(conflicts(verify(json!({ "*": "ask" }), false, json!({}))));
        assert!(conflicts(verify(
            json!([rule("*", "*", "ask")]),
            true,
            json!({})
        )));
    }

    #[test]
    fn allows_a_plan_explore_grant_only_for_primary_agents_other_than_explore() {
        for (name, mode, allowed) in [
            ("build", Some("primary"), true),
            ("plan", Some("primary"), true),
            ("custom", Some("primary"), true),
            ("general", Some("subagent"), false),
            ("explore", Some("subagent"), false),
            ("custom", Some("all"), false),
            ("custom", None, false),
            ("explore", Some("primary"), false),
        ] {
            let mut agent = json!({
                "name": name,
                "permission": [rule("*", "*", "deny"), rule("task", "explore", "allow")],
            });
            if let Some(mode) = mode {
                agent["mode"] = json!(mode);
            }
            let result = verify_managed_open_code_policy(
                &json!([agent]),
                &json!({}),
                RuntimeMode::FullAccess,
                true,
                None,
            );
            assert_eq!(result.is_ok(), allowed, "{name} {mode:?}");
        }
    }

    #[test]
    fn accepts_a_later_deny_that_removes_a_subagent_explore_grant() {
        verify_managed_open_code_policy(
            &json!([{
                "name": "explore",
                "mode": "subagent",
                "permission": [
                    rule("*", "*", "deny"),
                    rule("task", "explore", "allow"),
                    rule("task", "*", "deny"),
                ],
            }]),
            &json!({}),
            RuntimeMode::FullAccess,
            true,
            None,
        )
        .unwrap();
    }

    #[test]
    fn allows_only_the_exact_host_probed_tool_output_directory() {
        let output = "/isolated/data/opencode/tool-output/*";
        let check = |pattern: &str| {
            verify_managed_open_code_policy(
                &json!([{
                    "name": "build",
                    "permission": [rule("*", "*", "deny"), rule("external_directory", pattern, "allow")],
                }]),
                &json!({}),
                RuntimeMode::Supervised,
                true,
                Some(output),
            )
        };
        check(output).unwrap();
        assert!(conflicts(check("/isolated/data/opencode/*")));
        assert!(conflicts(check("/other/opencode/tool-output/*")));
        assert!(
            build_open_code_permission_rules(RuntimeMode::Supervised, true, Some(output)).contains(
                &OpenCodePermissionRule {
                    permission: "external_directory".into(),
                    pattern: output.into(),
                    action: PermissionAction::Allow,
                }
            )
        );
    }

    #[test]
    fn rejects_a_restored_child_tool_grant_and_skips_restrictions_in_full_access() {
        assert!(conflicts(verify(
            json!([rule("*", "*", "ask")]),
            false,
            json!({ "experimental": { "primary_tools": ["bash"] } }),
        )));
        verify_managed_open_code_policy(
            &json!([]),
            &json!({}),
            RuntimeMode::FullAccess,
            false,
            None,
        )
        .unwrap();
    }

    #[test]
    fn patches_legacy_mode_only_for_agents_observed_as_primary() {
        let output = managed_open_code_config(
            "build (primary)\n[]\ngeneral (subagent)\n[]\nexplore (subagent)\n[]",
            RuntimeMode::Supervised,
            false,
            None,
        )
        .unwrap();
        assert!(output.contains(r#""mode":{"build":"#));
        assert!(output.contains(r#""agent":{"build":{"permission":{"*":"ask","question":"allow","external_directory":{"*":"ask"}}},"general":"#));
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["mode"].as_object().unwrap().len(), 1);
        assert_eq!(value["agent"].as_object().unwrap().len(), 3);
    }

    // describe("owned OpenCode tool-output directory")

    #[test]
    fn derives_the_exact_output_glob_from_the_data_line() {
        for (output, expected) in [
            (
                "data       /isolated/data/opencode",
                "/isolated/data/opencode/tool-output/*",
            ),
            (
                "data       C:\\Users\\fixture\\opencode",
                "C:\\Users\\fixture\\opencode\\tool-output\\*",
            ),
            (
                "data       \\\\server\\share\\opencode",
                "\\\\server\\share\\opencode\\tool-output\\*",
            ),
            (
                "home       /home/user\r\ndata       /data/opencode/\r\n",
                "/data/opencode/tool-output/*",
            ),
        ] {
            assert_eq!(parse_open_code_tool_output_glob(output).unwrap(), expected);
        }
    }

    #[test]
    fn rejects_an_ambiguous_data_directory() {
        for output in [
            "cache      /cache",
            "data       relative/opencode",
            "data       /safe/../other",
            "data       /safe/*/opencode",
            "data       /safe/opencode?",
            "data       /one\ndata       /two",
        ] {
            let error = parse_open_code_tool_output_glob(output).unwrap_err();
            assert!(
                error.to_string().contains("safe data directory"),
                "{output}"
            );
        }
    }

    // describe("managed OpenCode permissions")

    const AGENTS: &str = r#"build (primary)
[{"permission":"bash","pattern":"*","action":"allow"}]
custom (subagent)
[{"permission":"mcp_write","pattern":"*","action":"allow"},{"permission":"read","pattern":"*.env","action":"allow"}]"#;

    #[test]
    fn overrides_custom_subagent_permissions_before_the_server_starts() {
        let config = config(AGENTS, RuntimeMode::Supervised, false, None);
        for key in ["*", "bash", "mcp_write", "read"] {
            assert_eq!(config["permission"][key], "ask", "{key}");
        }
        for key in ["*", "mcp_write", "read"] {
            assert_eq!(config["agent"]["custom"]["permission"][key], "ask", "{key}");
        }
        assert_eq!(config["experimental"]["primary_tools"], json!([]));
    }

    #[test]
    fn preserves_only_the_trusted_output_directory_grant() {
        for (mode, planning, baseline) in [
            (RuntimeMode::FullAccess, true, "deny"),
            (RuntimeMode::Supervised, false, "ask"),
        ] {
            let pattern = "/isolated/data/opencode/tool-output/*";
            let listed = json!([{ "permission": "external_directory", "pattern": pattern, "action": "allow" }]);
            let output = format!("build (primary)\n{listed}\ngeneral (subagent)\n{listed}");
            let trusted = "/host/data/opencode/tool-output/*";
            let config = config(&output, mode, planning, Some(trusted));
            let expected = json!({ "*": baseline, pattern: baseline, trusted: "allow" });
            assert_eq!(config["permission"]["external_directory"], expected);
            assert_eq!(
                config["agent"]["build"]["permission"]["external_directory"],
                expected
            );
            assert_eq!(
                config["agent"]["general"]["permission"]["external_directory"],
                expected
            );
            assert_eq!(
                config["mode"]["build"]["permission"]["external_directory"],
                expected
            );
        }
    }

    #[test]
    fn preserves_plan_restrictions_under_every_access_mode() {
        for mode in [
            RuntimeMode::FullAccess,
            RuntimeMode::Auto,
            RuntimeMode::AutoAcceptEdits,
        ] {
            let rules = build_open_code_permission_rules(mode, true, None);
            assert_eq!(
                rules[0],
                OpenCodePermissionRule {
                    permission: "*".into(),
                    pattern: "*".into(),
                    action: PermissionAction::Deny,
                }
            );
            assert!(
                !rules
                    .iter()
                    .any(|rule| rule.permission == "edit" && rule.action == PermissionAction::Allow)
            );
            let config = config(AGENTS, mode, true, None);
            assert_eq!(config["permission"]["*"], "deny");
            assert_eq!(config["permission"]["bash"], "deny");
            assert_eq!(config["permission"]["task"], "deny");
            assert_eq!(
                config["agent"]["build"]["permission"]["task"],
                json!({ "*": "deny", "explore": "allow" })
            );
            let custom = &config["agent"]["custom"]["permission"];
            assert_eq!(custom["task"], "deny");
            assert_eq!(custom["mcp_write"], "deny");
            assert_eq!(custom["read"], "allow");
        }
    }

    #[test]
    fn grants_plan_explore_tasks_only_to_primary_agents_that_cannot_be_the_explore_child() {
        let config = config(
            "build (primary)\n[]\nplan (primary)\n[]\ngeneral (subagent)\n[]\nexplore (primary)\n[]\ncustom (all)\n[]",
            RuntimeMode::FullAccess,
            true,
            None,
        );
        let allowed = json!({ "*": "deny", "explore": "allow" });
        assert_eq!(config["permission"]["task"], "deny");
        for (agent, task) in [
            ("build", &allowed),
            ("plan", &allowed),
            ("general", &json!("deny")),
            ("explore", &json!("deny")),
            ("custom", &json!("deny")),
        ] {
            assert_eq!(
                &config["agent"][agent]["permission"]["task"], task,
                "{agent}"
            );
        }
        assert_eq!(config["mode"]["build"]["permission"]["task"], allowed);
        assert_eq!(config["mode"]["plan"]["permission"]["task"], allowed);
        assert_eq!(config["mode"]["explore"]["permission"]["task"], "deny");
        assert!(config["mode"].get("general").is_none());
        assert!(config["mode"].get("custom").is_none());
        assert!(
            build_open_code_permission_rules(RuntimeMode::FullAccess, true, None).contains(
                &OpenCodePermissionRule {
                    permission: "task".into(),
                    pattern: "explore".into(),
                    action: PermissionAction::Allow,
                }
            )
        );
    }

    #[test]
    fn fails_closed_when_the_cli_omits_effective_agent_permissions() {
        let error = managed_open_code_config(
            "custom (subagent)\nnot JSON",
            RuntimeMode::Supervised,
            false,
            None,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Could not read OpenCode permissions")
        );
        let error = managed_open_code_config("", RuntimeMode::Supervised, false, None).unwrap_err();
        assert!(error.to_string().contains("did not expose"));
    }

    #[test]
    fn denies_custom_mutating_permissions_whose_names_contain_read_or_search() {
        let config = config(
            r#"custom (primary)
[{"permission":"spreadsheet_delete","pattern":"*","action":"allow"},{"permission":"search_and_delete","pattern":"*","action":"allow"}]"#,
            RuntimeMode::FullAccess,
            true,
            None,
        );
        let custom = &config["agent"]["custom"]["permission"];
        assert_eq!(custom["spreadsheet_delete"], "deny");
        assert_eq!(custom["search_and_delete"], "deny");
    }

    #[test]
    fn full_access_without_plan_keeps_every_tool_and_skips_the_child_tool_override() {
        let config = config(AGENTS, RuntimeMode::FullAccess, false, None);
        assert_eq!(config["permission"]["*"], "allow");
        assert_eq!(config["permission"]["bash"], "allow");
        assert!(config.get("experimental").is_none());
        assert!(config["permission"].get("external_directory").is_none());
    }
}
