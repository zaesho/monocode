//! Port of src/integrations/harness/providers/claude/claudeElicitation.ts:
//! MCP form elicitations shown in the question UI.
//!
//! Each schema field becomes a question, and one more question asks whether
//! to accept, decline, or cancel. The answers go back typed by the schema.
//! Only flat forms of string, enum, boolean, and number fields are supported.

use monocode_core::user_question::{
    UserQuestion, UserQuestionOption, UserQuestionReply, selected_answer_labels,
};
use regex::Regex;
use serde_json::{Map, Number, Value, json};

use super::protocol::{Record, as_record, record_field, string_field};

/// The id of the accept, decline, or cancel question. It gains underscores
/// until no schema field uses it.
fn action_question_id(request: &Record) -> String {
    let properties = record_field(
        record_field(Some(request), "requested_schema"),
        "properties",
    );
    let mut id = "__mcp_action".to_string();
    while properties.is_some_and(|properties| properties.contains_key(&id)) {
        id.push('_');
    }
    id
}

fn value_label(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `elicitationQuestions`: the form as questions, or why it cannot be shown.
pub fn elicitation_questions(request: &Record) -> Result<Vec<UserQuestion>, String> {
    let schema = record_field(Some(request), "requested_schema");
    let properties = record_field(schema, "properties");
    let (Some(properties), false) = (
        properties,
        string_field(Some(request), "mode") == Some("url"),
    ) else {
        return Err("This MCP elicitation form is not supported.".into());
    };
    let mut questions = Vec::new();
    for (id, raw) in properties {
        let field = as_record(raw);
        let kind = string_field(field, "type").unwrap_or("string");
        if !matches!(kind, "string" | "boolean" | "number" | "integer") {
            return Err(format!("Unsupported MCP field type for {id}."));
        }
        let choices: Vec<Value> = match field.and_then(|field| field.get("enum")) {
            Some(Value::Array(values)) => values.clone(),
            _ if kind == "boolean" => vec![json!(true), json!(false)],
            _ => Vec::new(),
        };
        questions.push(UserQuestion {
            id: id.clone(),
            header: string_field(field, "description").map(str::to_string),
            prompt: string_field(field, "title").unwrap_or(id).to_string(),
            multi_select: false,
            allow_custom: choices.is_empty(),
            options: choices
                .iter()
                .map(|value| UserQuestionOption {
                    id: value_label(value),
                    label: value_label(value),
                    description: None,
                })
                .collect(),
        });
    }
    questions.push(UserQuestion {
        id: action_question_id(request),
        header: None,
        prompt: "Submit this form?".into(),
        multi_select: false,
        allow_custom: false,
        options: [
            ("accept", "Accept"),
            ("decline", "Decline"),
            ("cancel", "Cancel"),
        ]
        .map(|(id, label)| UserQuestionOption {
            id: id.into(),
            label: label.into(),
            description: None,
        })
        .to_vec(),
    });
    Ok(questions)
}

/// JavaScript's `Number(text)` for a form answer, as JSON. `None` when the
/// text is not a finite number.
fn parse_number(text: &str) -> Option<Number> {
    let value: f64 = text
        .trim()
        .parse()
        .ok()
        .filter(|value: &f64| value.is_finite())?;
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        return Some(Number::from(value as i64));
    }
    Number::from_f64(value)
}

/// `elicitationResponse`: the reply to send back. An error names the field
/// to fix, so the caller can show the form again.
pub fn elicitation_response(
    request: &Record,
    questions: &[UserQuestion],
    reply: &UserQuestionReply,
) -> Result<Value, String> {
    let UserQuestionReply::Answered { answers, custom } = reply else {
        return Ok(json!({ "action": "cancel" }));
    };
    let action_id = action_question_id(request);
    let action = answers
        .get(&action_id)
        .and_then(|selected| selected.first())
        .map(String::as_str);
    if action != Some("accept") {
        let action = if action == Some("decline") {
            "decline"
        } else {
            "cancel"
        };
        return Ok(json!({ "action": action }));
    }
    let schema = record_field(Some(request), "requested_schema");
    let properties = record_field(schema, "properties");
    let required: Vec<&str> = schema
        .and_then(|schema| schema.get("required"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut content = Map::new();
    for question in questions {
        if question.id == action_id {
            continue;
        }
        let text = selected_answer_labels(question, answers, custom.as_ref())
            .into_iter()
            .next()
            .unwrap_or_default();
        let prompt = &question.prompt;
        if text.is_empty() {
            if required.contains(&question.id.as_str()) {
                return Err(format!("{prompt} is required."));
            }
            continue;
        }
        let field = record_field(properties, &question.id);
        let kind = string_field(field, "type");
        let value = match kind {
            Some("boolean") => {
                if text != "true" && text != "false" {
                    return Err(format!("{prompt} must be true or false."));
                }
                Value::Bool(text == "true")
            }
            Some(kind @ ("number" | "integer")) => {
                let number = parse_number(&text)
                    .filter(|number| kind == "number" || number.is_i64())
                    .ok_or_else(|| format!("{prompt} must be a valid {kind}."))?;
                let value = number.as_f64().unwrap_or_default();
                let bound = |key: &str| {
                    field
                        .and_then(|field| field.get(key))
                        .and_then(Value::as_f64)
                };
                if bound("minimum").is_some_and(|minimum| value < minimum)
                    || bound("maximum").is_some_and(|maximum| value > maximum)
                {
                    return Err(format!("{prompt} is outside the allowed range."));
                }
                Value::Number(number)
            }
            _ => {
                let length = text.chars().count() as f64;
                let limit = |key: &str| {
                    field
                        .and_then(|field| field.get(key))
                        .and_then(Value::as_f64)
                };
                let pattern =
                    string_field(field, "pattern").and_then(|pattern| Regex::new(pattern).ok());
                if limit("minLength").is_some_and(|min| length < min)
                    || limit("maxLength").is_some_and(|max| length > max)
                    || pattern.is_some_and(|pattern| !pattern.is_match(&text))
                {
                    return Err(format!("{prompt} does not match the requested format."));
                }
                Value::String(text)
            }
        };
        if let Some(Value::Array(allowed)) = field.and_then(|field| field.get("enum"))
            && !allowed.iter().any(|item| same_value(item, &value))
        {
            return Err(format!("{prompt} must use a listed value."));
        }
        content.insert(question.id.clone(), value);
    }
    Ok(json!({ "action": "accept", "content": content }))
}

/// JavaScript `includes` equality: numbers compare by value.
fn same_value(left: &Value, right: &Value) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_core::user_question::{QuestionAnswers, QuestionCustom};

    fn rec(value: Value) -> Record {
        value.as_object().cloned().unwrap()
    }

    fn request() -> Record {
        rec(json!({
            "mode": "form",
            "requested_schema": {
                "type": "object",
                "required": ["label", "count"],
                "properties": {
                    "label": { "type": "string", "minLength": 2 },
                    "count": { "type": "integer", "minimum": 1, "maximum": 10 },
                    "enabled": { "type": "boolean" },
                },
            },
        }))
    }

    fn answered(action: &str, custom: &[(&str, &str)]) -> UserQuestionReply {
        let mut answers = QuestionAnswers::new();
        answers.insert("__mcp_action".into(), vec![action.into()]);
        let custom: QuestionCustom = custom
            .iter()
            .map(|(id, text)| (id.to_string(), text.to_string()))
            .collect();
        UserQuestionReply::Answered {
            answers,
            custom: Some(custom),
        }
    }

    fn respond(reply: &UserQuestionReply) -> Result<Value, String> {
        let request = request();
        elicitation_response(&request, &elicitation_questions(&request).unwrap(), reply)
    }

    #[test]
    fn preserves_a_schema_field_that_uses_the_action_questions_default_id() {
        let form = rec(json!({
            "requested_schema": { "properties": { "__mcp_action": { "type": "string" } } },
        }));
        let mut answers = QuestionAnswers::new();
        answers.insert("__mcp_action_".into(), vec!["accept".into()]);
        let mut custom = QuestionCustom::new();
        custom.insert("__mcp_action".into(), "value".into());
        let reply = UserQuestionReply::Answered {
            answers,
            custom: Some(custom),
        };
        assert_eq!(
            elicitation_response(&form, &elicitation_questions(&form).unwrap(), &reply),
            Ok(json!({ "action": "accept", "content": { "__mcp_action": "value" } }))
        );
    }

    #[test]
    fn returns_typed_answers_and_keeps_optional_omissions_absent() {
        assert_eq!(
            respond(&answered("accept", &[("label", "Test"), ("count", "3")])),
            Ok(json!({ "action": "accept", "content": { "label": "Test", "count": 3 } }))
        );
    }

    #[test]
    fn preserves_the_users_decline_or_cancel_action() {
        for action in ["decline", "cancel"] {
            assert_eq!(
                respond(&answered(action, &[])),
                Ok(json!({ "action": action }))
            );
        }
    }

    #[test]
    fn cancels_a_skipped_form() {
        assert_eq!(
            respond(&UserQuestionReply::Skipped),
            Ok(json!({ "action": "cancel" }))
        );
    }

    #[test]
    fn rejects_an_invalid_integer() {
        for count in ["0", "11", "1.5", "invalid"] {
            assert!(respond(&answered("accept", &[("label", "Test"), ("count", count)])).is_err());
        }
    }

    #[test]
    fn rejects_missing_required_answers() {
        assert!(
            respond(&answered("accept", &[]))
                .unwrap_err()
                .contains("required")
        );
    }

    #[test]
    fn offers_boolean_choices_and_refuses_url_forms() {
        let questions = elicitation_questions(&request()).unwrap();
        let enabled = questions.iter().find(|q| q.id == "enabled").unwrap();
        assert!(!enabled.allow_custom);
        assert_eq!(enabled.options[0].id, "true");
        assert_eq!(questions.last().unwrap().id, "__mcp_action");
        assert!(elicitation_questions(&rec(json!({ "mode": "url" }))).is_err());
    }
}
