//! Validation shared by owner-native and delegated question answers.
//! `cutexQuestionOptions` is additive; generated Codex question types stay unchanged.

use std::collections::HashSet;

use serde_json::Value;

pub const FEATURE: &str = "user-input-options-v1";
pub const MAX_ANSWER_BYTES: usize = 64 * 1024;
pub const MAX_TOTAL_ANSWER_BYTES: usize = 256 * 1024;
const MAX_PREVIEW_BYTES: usize = 64 * 1024;

struct Selection<'a> {
    labels: Vec<&'a str>,
    multiple: bool,
    other: bool,
}

// Python's str.strip also recognizes these four ASCII separators. Keep the
// daemon's pre-claim checks identical to the adapter's normalization.
fn question_space(c: char) -> bool {
    c.is_whitespace() || ('\u{001c}'..='\u{001f}').contains(&c)
}

fn trim_answer(value: &str) -> &str {
    value.trim_matches(question_space)
}

fn selection(question: &Value) -> Result<Selection<'_>, String> {
    let rich = question
        .get("cutexQuestionOptions")
        .filter(|value| value.get("version").and_then(Value::as_u64) == Some(1));
    let multiple = match rich {
        Some(details) => details
            .get("multiSelect")
            .and_then(Value::as_bool)
            .ok_or("question selection mode must be a boolean")?,
        None => false,
    };
    let options = rich
        .and_then(|details| details.get("options"))
        .or_else(|| question.get("options"));
    let options = match options {
        Some(Value::Array(options)) => options.as_slice(),
        None | Some(Value::Null) if rich.is_none() => &[],
        _ => return Err("question options must be an array".into()),
    };
    if rich.is_some() && !(2..=4).contains(&options.len()) {
        return Err("structured question options must contain two to four choices".into());
    }
    let mut labels = Vec::with_capacity(options.len());
    let mut distinct = HashSet::new();
    for option in options {
        let label = option
            .get("label")
            .and_then(Value::as_str)
            .filter(|value| !trim_answer(value).is_empty())
            .ok_or("question option label must be a nonblank string")?;
        if !distinct.insert(trim_answer(label)) {
            return Err("question option labels must be distinct".into());
        }
        if rich.is_some() {
            if option.get("description").and_then(Value::as_str).is_none() {
                return Err("structured question option description must be a string".into());
            }
            if option.get("preview").is_some_and(|preview| {
                preview
                    .as_str()
                    .is_none_or(|text| text.len() > MAX_PREVIEW_BYTES)
            }) {
                return Err("question preview must be text of at most 64 KiB".into());
            }
        }
        labels.push(label);
    }
    Ok(Selection {
        other: labels.is_empty() || question.get("isOther").and_then(Value::as_bool) == Some(true),
        labels,
        multiple,
    })
}

/// Reject an invalid answer before any durable response claim or question resolution.
pub fn validate_answers(questions: &Value, answers: &Value) -> Result<(), String> {
    let questions = questions
        .as_array()
        .filter(|questions| !questions.is_empty())
        .ok_or("pending questions must be a nonempty array")?;
    let answers = answers
        .as_object()
        .ok_or("answers must be an object keyed by question id")?;
    // Native clients explicitly submit an empty map on nonblocking auto-resolution.
    // This is a no-answer result, not an invented choice or a malformed packet.
    if answers.is_empty() {
        return Ok(());
    }
    if answers.len() != questions.len() {
        return Err("answer every question exactly once without additional question ids".into());
    }
    let mut question_ids = HashSet::new();
    let mut total_bytes = 0usize;
    for question in questions {
        let id = question
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or("pending question id must be a nonempty string")?;
        if !question_ids.insert(id) {
            return Err("pending question ids must be distinct".into());
        }
        let selection = selection(question)?;
        let answer = answers
            .get(id)
            .and_then(Value::as_object)
            .filter(|answer| answer.len() == 1)
            .ok_or("each question needs an answers array and no additional fields")?;
        let values = answer
            .get("answers")
            .and_then(Value::as_array)
            .ok_or("each question needs an answers array")?;
        let limit = if selection.multiple {
            selection.labels.len() + usize::from(selection.other)
        } else {
            1
        };
        let mut distinct = HashSet::new();
        let mut custom = 0;
        let mut notes = 0;
        let mut selections = 0;
        for value in values {
            let text = value
                .as_str()
                .filter(|text| !trim_answer(text).is_empty() && text.len() <= MAX_ANSWER_BYTES)
                .ok_or("answers must be nonblank strings of at most 64 KiB")?;
            total_bytes = total_bytes.saturating_add(text.len());
            if total_bytes > MAX_TOTAL_ANSWER_BYTES {
                return Err("all answer text must fit within 256 KiB".into());
            }
            let raw = text;
            let text = trim_answer(raw);
            if !distinct.insert(text) {
                return Err("answers to one question must be distinct".into());
            }
            if selection
                .labels
                .iter()
                .any(|label| trim_answer(label) == text)
            {
                selections += 1;
            } else if let Some(note) = raw
                .trim_start_matches(question_space)
                .strip_prefix("user_note: ")
            {
                // The native TUI appends a note as an additional string. It is
                // not another selected option and is legal even without Other.
                notes += 1;
                if notes > 1 || trim_answer(note).is_empty() {
                    return Err("only one nonblank legacy note is permitted".into());
                }
            } else {
                selections += 1;
                custom += 1;
                if !selection.other || custom > 1 {
                    return Err("choose a listed option or one permitted custom answer".into());
                }
            }
        }
        if selections > limit {
            return Err("answer count does not match the question selection mode".into());
        }
    }
    Ok(())
}

pub(crate) fn validate_native_answer(request: &Value, response: &Value) -> Result<(), String> {
    if request.get("method").and_then(Value::as_str) != Some("item/tool/requestUserInput")
        || response.get("error").is_some()
    {
        return Ok(());
    }
    let result = response
        .get("result")
        .and_then(Value::as_object)
        .filter(|result| result.len() == 1)
        .ok_or("a question response must contain only its answers object")?;
    let answers = result
        .get("answers")
        .ok_or("a question response must contain answers")?;
    validate_answers(&request["params"]["questions"], answers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn questions(multiple: bool, other: bool) -> Value {
        json!([{
            "id": "q0", "question": "legacy fallback", "isOther": other,
            "options": null,
            "cutexQuestionOptions": {
                "version": 1, "question": "Platforms?", "multiSelect": multiple,
                "options": [
                    {"label": "Windows", "description": "PC", "preview": "<script>untrusted</script>"},
                    {"label": "Linux", "description": "Server", "preview": "**Text**"}
                ]
            }
        }])
    }

    #[test]
    fn structured_multiple_answers_and_one_custom_answer_are_valid() {
        let questions = questions(true, true);
        let answers = json!({"q0": {"answers": ["Windows", "Linux", "Other OS"]}});
        assert!(validate_answers(&questions, &answers).is_ok());
        // Preview is retained as text; it does not participate in choosing or answering.
        assert_eq!(
            questions[0]["cutexQuestionOptions"]["options"][0]["preview"],
            "<script>untrusted</script>"
        );
    }

    #[test]
    fn legacy_codex_choices_and_text_only_fallback_still_work() {
        let choices =
            json!([{"id":"q0", "isOther":false, "options":[{"label":"A"},{"label":"B"}]}]);
        assert!(validate_answers(&choices, &json!({"q0":{"answers":["A"]}})).is_ok());
        assert!(validate_answers(&choices, &json!({"q0":{"answers":["C"]}})).is_err());
        assert!(validate_answers(&choices, &json!({"q0":{"answers":["A","B"]}})).is_err());
        let text = json!([{"id":"q0", "options":null}]);
        assert!(validate_answers(&text, &json!({"q0":{"answers":["A, B"]}})).is_ok());
    }

    #[test]
    fn malformed_incomplete_duplicate_and_unadvertised_answers_are_rejected() {
        let questions = questions(true, true);
        for answers in [
            json!({"other":{"answers":["Windows"]}}),
            json!({"q0":{"answers":[" "]}}),
            json!({"q0":{"answers":[12]}}),
            json!({"q0":{"answers":["Linux"," Linux "]}}),
            json!({"q0":{"answers":["Other 1","Other 2"]}}),
            json!({"q0":{"answers":["Linux"],"extra":true}}),
            json!({"q0":{"answers":["Linux"]},"extra":{"answers":["ignored"]}}),
        ] {
            assert!(validate_answers(&questions, &answers).is_err(), "{answers}");
        }
        assert!(validate_answers(
            &self::questions(false, true),
            &json!({"q0":{"answers":["Linux","Windows"]}})
        )
        .is_err());
        assert!(validate_answers(
            &self::questions(true, false),
            &json!({"q0":{"answers":["Other OS"]}})
        )
        .is_err());
    }

    #[test]
    fn text_limits_count_utf8_bytes_and_do_not_log_the_answer() {
        let text = "界".repeat(MAX_ANSWER_BYTES / 3 + 1);
        let error = validate_answers(&questions(true, true), &json!({"q0":{"answers":[text]}}))
            .unwrap_err();
        assert!(!error.contains('界'));
        let many = json!((0..5)
            .map(|i| json!({"id":format!("q{i}")}))
            .collect::<Vec<_>>());
        let answers: serde_json::Map<String, Value> = (0..5)
            .map(|i| {
                (
                    format!("q{i}"),
                    json!({"answers":["x".repeat(MAX_ANSWER_BYTES)]}),
                )
            })
            .collect();
        assert!(validate_answers(&many, &Value::Object(answers)).is_err());
    }

    #[test]
    fn malformed_known_extension_is_rejected_and_unknown_versions_use_legacy_fields() {
        let mut questions = questions(true, true);
        questions[0]["cutexQuestionOptions"]["multiSelect"] = json!("true");
        assert!(validate_answers(&questions, &json!({"q0":{"answers":["Linux"]}})).is_err());
        questions[0]["cutexQuestionOptions"]["version"] = json!(2);
        assert!(validate_answers(&questions, &json!({"q0":{"answers":["legacy text"]}})).is_ok());
        questions[0]["cutexQuestionOptions"]["version"] = json!(1);
        questions[0]["cutexQuestionOptions"]["multiSelect"] = json!(true);
        questions[0]["cutexQuestionOptions"]["options"][1]["label"] = json!("Windows");
        assert!(validate_answers(&questions, &json!({"q0":{"answers":["Windows"]}})).is_err());
    }

    #[test]
    fn question_response_validation_does_not_turn_cancellation_into_an_answer() {
        let request = json!({"method":"item/tool/requestUserInput", "params":{"questions":questions(false, true)}});
        assert!(validate_native_answer(
            &request,
            &json!({"error":{"code":-32800,"message":"cancelled"}})
        )
        .is_ok());
        assert!(validate_native_answer(
            &request,
            &json!({"result":{"answers":{"q0":{"answers":["Windows"]}}}})
        )
        .is_ok());
        assert!(validate_native_answer(&request, &json!({"result":{"answers":{}}})).is_ok());
        assert!(validate_native_answer(&request, &json!({"result":null})).is_err());
        assert!(validate_native_answer(
            &json!({"method":"item/commandExecution/requestApproval"}),
            &json!({"result":{"decision":"accept"}})
        )
        .is_ok());
    }

    #[test]
    fn legacy_option_plus_note_and_intentional_no_answer_results_remain_valid() {
        let questions = questions(false, false);
        for answers in [
            json!({}),
            json!({"q0":{"answers":[]}}),
            json!({"q0":{"answers":["Windows","user_note: explain why"]}}),
            json!({"q0":{"answers":["user_note: an unlisted idea"]}}),
        ] {
            assert!(validate_answers(&questions, &answers).is_ok(), "{answers}");
        }
        for answers in [
            json!({"q0":{"answers":["Windows","Linux","user_note: too many choices"]}}),
            json!({"q0":{"answers":["Windows","user_note: "]}}),
            json!({"q0":{"answers":["user_note: one","user_note: two"]}}),
        ] {
            assert!(validate_answers(&questions, &answers).is_err(), "{answers}");
        }
    }

    #[test]
    fn python_strip_separators_are_blank_or_duplicate_before_claiming() {
        let questions = questions(true, true);
        for c in ['\u{001c}', '\u{001d}', '\u{001e}', '\u{001f}'] {
            assert!(
                validate_answers(&questions, &json!({"q0":{"answers":[c.to_string()]}})).is_err()
            );
            assert!(validate_answers(
                &questions,
                &json!({"q0":{"answers":["Linux",format!("{c}Linux")]}})
            )
            .is_err());
            assert!(validate_answers(
                &questions,
                &json!({"q0":{"answers":[format!("{c}Linux"),"Other OS"]}})
            )
            .is_ok());
        }
        // BOM is not stripped by Python or Rust; do not silently broaden the rule.
        assert_eq!(trim_answer("\u{feff}"), "\u{feff}");
    }

    #[test]
    fn adapter_and_daemon_accept_the_same_shared_answer_contract() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../components/claude-runtime/tests/fixtures/question_answer_contract.json"
        ))
        .expect("shared answer contract fixture");
        for case in cases {
            let questions = questions(case["multiSelect"].as_bool().unwrap(), true);
            assert_eq!(
                validate_answers(&questions, &case["answers"]).is_ok(),
                case["accepted"].as_bool().unwrap(),
                "shared contract case {}",
                case["name"]
            );
        }
    }
}
