//! The environment of an owned OpenCode 1 server. MonoCode passes its
//! managed permission policy as `OPENCODE_CONFIG_CONTENT`, which would
//! replace any inline config the user already set there. This module merges
//! the two instead: the user's JSONC keeps its providers and agents, and the
//! policy overrides only the keys it names.
//!
//! OpenCode reads permission objects in key order, so the merge keeps the
//! order of both documents. This crate's `serde_json::Map` may sort keys, so
//! the merge uses its own ordered value.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use monocode_platform::expand_home;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};

/// A JSON value whose objects keep their key order.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    /// `object[key] = value`: replace in place, or append.
    fn set(entries: &mut Vec<(String, Json)>, key: String, value: Json) {
        match entries.iter_mut().find(|(name, _)| *name == key) {
            Some(entry) => entry.1 = value,
            None => entries.push((key, value)),
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Json;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_none<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }
            fn visit_bool<E>(self, value: bool) -> Result<Json, E> {
                Ok(Json::Bool(value))
            }
            fn visit_i64<E>(self, value: i64) -> Result<Json, E> {
                Ok(Json::Number(value.into()))
            }
            fn visit_u64<E>(self, value: u64) -> Result<Json, E> {
                Ok(Json::Number(value.into()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Json, E> {
                serde_json::Number::from_f64(value)
                    .map(Json::Number)
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E>(self, value: &str) -> Result<Json, E> {
                Ok(Json::String(value.to_string()))
            }
            fn visit_string<E>(self, value: String) -> Result<Json, E> {
                Ok(Json::String(value))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Json::Array(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                // A repeated key keeps its first position and its last value,
                // as `JSON.parse` does.
                let mut entries = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Json>()? {
                    Json::set(&mut entries, key, value);
                }
                Ok(Json::Object(entries))
            }
        }
        deserializer.deserialize_any(JsonVisitor)
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => serializer.serialize_unit(),
            Json::Bool(value) => serializer.serialize_bool(*value),
            Json::Number(value) => value.serialize(serializer),
            Json::String(value) => serializer.serialize_str(value),
            Json::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Json::Object(entries) => {
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

/// Deep-merge `patch` into `target`. Objects merge key by key; anything
/// else replaces, so a scalar permission replaces an inherited pattern map.
fn merge_config(target: &mut Json, patch: Json) {
    match (target, patch) {
        (Json::Object(entries), Json::Object(patch)) => {
            for (key, value) in patch {
                match entries.iter_mut().find(|(name, _)| *name == key) {
                    Some(entry) => merge_config(&mut entry.1, value),
                    None => entries.push((key, value)),
                }
            }
        }
        (target, patch) => *target = patch,
    }
}

/// Configure an owned OpenCode 1 server: drop inherited HTTP basic auth,
/// which the client does not send, and merge `policy` into any inline
/// config the environment already sets. Variable lookups see `cmd`'s final
/// environment.
pub(crate) fn configure_opencode_server(
    cmd: &mut Command,
    policy: Option<&str>,
) -> Result<(), String> {
    cmd.env_remove("OPENCODE_SERVER_PASSWORD")
        .env_remove("OPENCODE_SERVER_USERNAME");
    let Some(policy) = policy else {
        return Ok(());
    };
    let inherited = command_env(cmd, "OPENCODE_CONFIG_CONTENT").unwrap_or_else(|| "{}".into());
    let cwd = cmd
        .get_current_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let config =
        merge_opencode_config_with_env(&inherited, policy, &cwd, |name| command_env(cmd, name))?;
    cmd.env("OPENCODE_CONFIG_CONTENT", config);
    Ok(())
}

/// The value `cmd` gives its child for `name`.
fn command_env(cmd: &Command, name: &str) -> Option<String> {
    match cmd.get_envs().find(|(key, _)| *key == name) {
        Some((_, value)) => value.and_then(|value| value.to_str().map(str::to_owned)),
        None => std::env::var(name).ok(),
    }
}

/// Expand `{env:}` and `{file:}` in the inherited JSONC, merge `policy`
/// over it, and serialize it with those tokens escaped. OpenCode expands the
/// content again in the child, and expanded text must stay literal.
pub(crate) fn merge_opencode_config_with_env(
    base: &str,
    policy: &str,
    cwd: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let expanded = substitute_opencode_config(base, cwd, env)?;
    let mut config: Json = serde_json::from_str(&expanded)
        .or_else(|_| serde_json::from_str(&crate::mcp::strip_jsonc(&expanded)))
        .map_err(|_| "Invalid inherited OpenCode config".to_string())?;
    let patch: Json =
        serde_json::from_str(policy).map_err(|_| "Invalid OpenCode policy config".to_string())?;
    if !matches!(config, Json::Object(_)) || !matches!(patch, Json::Object(_)) {
        return Err("Invalid OpenCode config object".into());
    }
    merge_config(&mut config, patch);
    let text = serde_json::to_string(&config).map_err(|error| error.to_string())?;
    Ok(text
        .replace("{env:", "\\u007benv:")
        .replace("{file:", "\\u007bfile:"))
}

/// OpenCode's own `{env:NAME}` and `{file:path}` substitution. A file token
/// on a `//` comment line stays as it is, and a relative file is read from
/// `cwd`.
fn substitute_opencode_config(
    raw: &str,
    cwd: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut expanded = String::with_capacity(raw.len());
    let mut cursor = 0;
    while let Some(offset) = raw[cursor..].find("{env:") {
        let start = cursor + offset;
        let Some(end) = raw[start + 5..].find('}').map(|end| start + 5 + end) else {
            break;
        };
        expanded.push_str(&raw[cursor..start]);
        if end == start + 5 {
            expanded.push_str(&raw[start..=end]);
        } else {
            expanded.push_str(&env(&raw[start + 5..end]).unwrap_or_default());
        }
        cursor = end + 1;
    }
    expanded.push_str(&raw[cursor..]);
    let mut result = String::with_capacity(expanded.len());
    cursor = 0;
    while let Some(offset) = expanded[cursor..].find("{file:") {
        let start = cursor + offset;
        let Some(end) = expanded[start + 6..].find('}').map(|end| start + 6 + end) else {
            break;
        };
        result.push_str(&expanded[cursor..start]);
        let line = expanded[..start].rfind('\n').map_or(0, |line| line + 1);
        if end == start + 6 || expanded[line..start].trim_start().starts_with("//") {
            result.push_str(&expanded[start..=end]);
        } else {
            let name = &expanded[start + 6..end];
            let file = if name.starts_with("~/") {
                expand_home(name)
            } else {
                PathBuf::from(name)
            };
            let file = if file.is_absolute() {
                file
            } else {
                cwd.join(file)
            };
            let content = std::fs::read_to_string(&file)
                .map_err(|_| format!("Invalid OpenCode file reference: {}", file.display()))?;
            let quoted =
                serde_json::to_string(content.trim()).map_err(|error| error.to_string())?;
            result.push_str(&quoted[1..quoted.len() - 1]);
        }
        cursor = end + 1;
    }
    result.push_str(&expanded[cursor..]);
    Ok(result)
}

/// Apply the OpenCode server environment to `cmd` when it starts an owned
/// OpenCode 1 server (`serve` without `--stdio`). Returns the overrides left
/// for the caller to set.
pub(crate) fn apply_opencode_server_env(
    cmd: &mut Command,
    binary_provider: Option<&str>,
    args: &[String],
    mut environment: HashMap<String, String>,
) -> Result<HashMap<String, String>, String> {
    let owned_v1_server = binary_provider == Some("opencode")
        && args.first().map(String::as_str) == Some("serve")
        && !args.iter().any(|arg| arg == "--stdio");
    if !owned_v1_server {
        return Ok(environment);
    }
    let policy = environment.remove("OPENCODE_CONFIG_CONTENT");
    cmd.envs(environment.drain());
    configure_opencode_server(cmd, policy.as_deref())?;
    Ok(environment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn merge_opencode_config(base: &str, policy: &str, cwd: &Path) -> Result<String, String> {
        merge_opencode_config_with_env(base, policy, cwd, |name| std::env::var(name).ok())
    }

    #[test]
    fn scoped_config_preserves_provider_and_replaces_permission_maps() {
        let base = r#"{"provider":{"local":{"options":{"baseURL":"http://localhost:8000"}}},"agent":{"custom":{"prompt":"Review","permission":{"edit":{"*":"allow"}}}}}"#;
        let merged = merge_opencode_config(
            base,
            r#"{"agent":{"custom":{"permission":{"edit":"deny"}}}}"#,
            Path::new("."),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["agent"]["custom"]["permission"]["edit"], "deny");
        assert_eq!(value["agent"]["custom"]["prompt"], "Review");
        assert_eq!(
            value["provider"]["local"]["options"]["baseURL"],
            "http://localhost:8000"
        );
        let mut command = Command::new("opencode");
        configure_opencode_server(&mut command, None).unwrap();
        for name in ["OPENCODE_SERVER_PASSWORD", "OPENCODE_SERVER_USERNAME"] {
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == name && value.is_none())
            );
        }
    }

    #[test]
    fn keeps_the_key_order_of_both_documents() {
        let merged = merge_opencode_config(
            r#"{"z":1,"permission":{"zeta":"allow","*":"allow"}}"#,
            r#"{"permission":{"*":"ask","alpha":"ask"},"a":2}"#,
            Path::new("."),
        )
        .unwrap();
        assert_eq!(
            merged,
            r#"{"z":1,"permission":{"zeta":"allow","*":"ask","alpha":"ask"},"a":2}"#
        );
    }

    #[test]
    fn inherited_inline_jsonc_is_merged_without_changing_quoted_content() {
        let base = r#"{
            // Keep URL and comment markers inside strings.
            "provider": {"local": {"options": {"baseURL": "https://example.com/a//b"},},},
            "agent": {"custom": {"prompt": "Quoted \"/* text */\""},},
        }"#;
        let merged = merge_opencode_config(
            base,
            r#"{"agent":{"custom":{"permission":{"edit":"deny"}}}}"#,
            Path::new("."),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(
            value["provider"]["local"]["options"]["baseURL"],
            "https://example.com/a//b"
        );
        assert_eq!(value["agent"]["custom"]["prompt"], "Quoted \"/* text */\"");
        assert_eq!(value["agent"]["custom"]["permission"]["edit"], "deny");
        assert!(merge_opencode_config("{}", "{/* policy */}", Path::new(".")).is_err());
    }

    #[test]
    fn inline_config_uses_the_final_command_environment() {
        let mut command = Command::new("opencode");
        command.env("MONOCODE_TEST_CHILD_VALUE", "child-value");
        command.env("OPENCODE_SERVER_PASSWORD", "synthetic-parent-password");
        command.env(
            "OPENCODE_CONFIG_CONTENT",
            r#"{"agent":{"custom":{"prompt":"{env:MONOCODE_TEST_CHILD_VALUE}","description":"{env:OPENCODE_SERVER_PASSWORD}"}}}"#,
        );
        configure_opencode_server(
            &mut command,
            Some(r#"{"agent":{"custom":{"permission":{"edit":"deny"}}}}"#),
        )
        .unwrap();
        let merged = command
            .get_envs()
            .find(|(key, _)| *key == "OPENCODE_CONFIG_CONTENT")
            .unwrap()
            .1
            .unwrap();
        let config: Value = serde_json::from_str(merged.to_str().unwrap()).unwrap();
        assert_eq!(config["agent"]["custom"]["prompt"], "child-value");
        assert_eq!(config["agent"]["custom"]["description"], "");
    }

    #[test]
    fn inline_config_expands_variables_once_in_the_spawn_directory() {
        let root =
            std::env::temp_dir().join(format!("monocode-inline-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("prompt.txt"),
            "  Review \"quoted\"\nKeep {env:SECOND} and {file:missing.txt} literal.  ",
        )
        .unwrap();
        let base = r#"{
            // {file:absent-comment.txt}
            "provider": {"local": {"options": {env:OPTIONS}}},
            "agent": {"custom": {"prompt": "{file:prompt.txt}", "description": "{env:DESCRIPTION}"}},
            "username": "{env:LITERAL}",
        }"#;
        let merged = merge_opencode_config_with_env(
            base,
            r#"{"agent":{"custom":{"permission":{"edit":"deny"}}}}"#,
            &root,
            |name| match name {
                "OPTIONS" => Some(r#"{"baseURL":"http://localhost:9000/"}"#.into()),
                "DESCRIPTION" => Some("fixture-description".into()),
                "LITERAL" => Some("{env:SECOND}".into()),
                _ => None,
            },
        )
        .unwrap();
        let value: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(
            value["provider"]["local"]["options"]["baseURL"],
            "http://localhost:9000/"
        );
        assert_eq!(
            value["agent"]["custom"]["description"],
            "fixture-description"
        );
        assert_eq!(
            value["agent"]["custom"]["prompt"],
            "Review \"quoted\"\nKeep {env:SECOND} and {file:missing.txt} literal."
        );
        assert_eq!(value["username"], "{env:SECOND}");
        assert!(!merged.contains("{env:"));
        assert!(!merged.contains("{file:"));
        assert_eq!(value["agent"]["custom"]["permission"]["edit"], "deny");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_owned_v1_servers_get_the_server_environment() {
        let policy = HashMap::from([(
            "OPENCODE_CONFIG_CONTENT".to_string(),
            json!({ "permission": { "*": "ask" } }).to_string(),
        )]);
        let mut v2 = Command::new("opencode");
        let left = apply_opencode_server_env(
            &mut v2,
            Some("opencode"),
            &["serve".into(), "--stdio".into()],
            policy.clone(),
        )
        .unwrap();
        assert_eq!(left, policy);
        assert_eq!(v2.get_envs().count(), 0);

        let mut v1 = Command::new("opencode");
        v1.env("OPENCODE_CONFIG_CONTENT", r#"{"model":"local/model"}"#);
        let left = apply_opencode_server_env(&mut v1, Some("opencode"), &["serve".into()], policy)
            .unwrap();
        assert!(left.is_empty());
        let merged = v1
            .get_envs()
            .find(|(key, _)| *key == "OPENCODE_CONFIG_CONTENT")
            .and_then(|(_, value)| value)
            .unwrap();
        assert_eq!(
            merged.to_str().unwrap(),
            r#"{"model":"local/model","permission":{"*":"ask"}}"#
        );
    }
}
