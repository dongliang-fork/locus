//! Shared questionnaire contract for Locus tools, native CLI adaptation and UI
//! replies. Validation happens before consuming the pending response channel.
use crate::commands::AskOption;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub header: String,
    pub options: Vec<AskOption>,
    #[serde(default)]
    pub multi_select: bool,
    pub allow_custom: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QuestionForm {
    pub questions: Vec<AskQuestion>,
    pub legacy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionAnswer {
    pub id: String,
    pub selected: Vec<String>,
    #[serde(default)]
    pub custom_text: String,
}

impl QuestionAnswer {
    pub fn text(&self) -> String {
        let mut parts = self.selected.clone();
        if !self.custom_text.is_empty() {
            parts.push(self.custom_text.clone());
        }
        parts.join(", ")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerEnvelope {
    version: u32,
    answers: Vec<QuestionAnswer>,
}

impl QuestionForm {
    pub fn parse(args: &Value) -> Result<Self, String> {
        let legacy = args.get("questions").is_none();
        let values = if legacy {
            vec![args]
        } else {
            if args.get("question").is_some() || args.get("options").is_some() {
                return Err("Use questions or the legacy question/options pair, not both".into());
            }
            let values = args["questions"]
                .as_array()
                .ok_or("questions must be an array")?;
            if values.is_empty() || values.len() > 4 {
                return Err("Provide 1 to 4 questions".into());
            }
            values.iter().collect()
        };
        let mut questions = Vec::new();
        let mut ids = HashSet::new();
        for (index, value) in values.into_iter().enumerate() {
            let question = value["question"].as_str().unwrap_or_default().trim();
            if question.is_empty() {
                return Err("Each question requires non-empty question text".into());
            }
            let id = value["id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("q{}", index + 1));
            if !ids.insert(id.clone()) {
                return Err(format!("Duplicate question id: {id}"));
            }
            let kind = value["kind"].as_str().unwrap_or("select");
            if !matches!(kind, "select" | "text") {
                return Err(format!("Unsupported question kind: {kind}"));
            }
            let mut options = Vec::new();
            if let Some(raw) = value.get("options").filter(|v| !v.is_null()) {
                let raw = raw.as_array().ok_or("options must be an array")?;
                for option in raw {
                    let label = option["label"].as_str().unwrap_or_default().trim();
                    if label.is_empty() {
                        return Err("Option labels must not be empty".into());
                    }
                    options.push(AskOption {
                        label: label.into(),
                        description: option["description"].as_str().unwrap_or_default().into(),
                    });
                }
            }
            if !legacy && options.len() > 4 {
                return Err(
                    "Provide at most 4 choices per question; custom input is separate".into(),
                );
            }
            let custom = if legacy {
                Some(options.pop().ok_or("At least one option is required")?)
            } else {
                None
            };
            let allow_custom = legacy || value["allowCustom"].as_bool().unwrap_or(true);
            if kind == "text" {
                options.clear();
            }
            if options.is_empty() && !allow_custom {
                return Err("A question requires choices or custom input".into());
            }
            let mut labels = HashSet::new();
            if options.iter().any(|o| !labels.insert(o.label.clone())) {
                return Err("Option labels must be unique within a question".into());
            }
            questions.push(AskQuestion {
                id,
                question: question.into(),
                header: value["header"].as_str().unwrap_or_default().into(),
                options,
                multi_select: !legacy && value["multiSelect"].as_bool().unwrap_or(false),
                allow_custom,
                custom_label: custom.as_ref().map(|o| o.label.clone()),
                custom_description: custom.map(|o| o.description),
            });
        }
        Ok(Self { questions, legacy })
    }

    /// Older clients and tool confirmations still send strings. A legacy
    /// single-question string is interpreted without attempting JSON parsing.
    pub fn validate_answer(&self, value: &Value) -> Result<String, String> {
        let mut answers = if self.legacy && value.is_string() {
            let text = value.as_str().unwrap().trim();
            let question = &self.questions[0];
            let selected = question.options.iter().any(|o| o.label == text);
            vec![QuestionAnswer {
                id: question.id.clone(),
                selected: if selected {
                    vec![text.into()]
                } else {
                    Vec::new()
                },
                custom_text: if selected { String::new() } else { text.into() },
            }]
        } else {
            let envelope: AnswerEnvelope = serde_json::from_value(value.clone())
                .map_err(|error| format!("Invalid questionnaire answer: {error}"))?;
            if envelope.version != 1 {
                return Err("Unsupported questionnaire answer version".into());
            }
            envelope.answers
        };
        if answers.len() != self.questions.len() {
            return Err("Answer every question before submitting".into());
        }
        let mut ordered = Vec::new();
        for question in &self.questions {
            let index = answers
                .iter()
                .position(|a| a.id == question.id)
                .ok_or_else(|| format!("Missing answer for {}", question.id))?;
            let mut answer = answers.remove(index);
            answer.custom_text = answer.custom_text.trim().into();
            let mut selected = HashSet::new();
            for label in &answer.selected {
                if !question.options.iter().any(|o| &o.label == label)
                    || !selected.insert(label.clone())
                {
                    return Err(format!("Invalid or duplicate choice for {}", question.id));
                }
            }
            if !question.allow_custom && !answer.custom_text.is_empty() {
                return Err(format!("Custom input is not allowed for {}", question.id));
            }
            let count = answer.selected.len() + usize::from(!answer.custom_text.is_empty());
            if count == 0 || (!question.multi_select && count != 1) {
                return Err(format!("Invalid selection count for {}", question.id));
            }
            // Keep model output stable even when checkboxes were clicked in a
            // different order. Custom text remains a separate field.
            answer.selected = question
                .options
                .iter()
                .filter(|o| selected.contains(&o.label))
                .map(|o| o.label.clone())
                .collect();
            ordered.push(answer);
        }
        serde_json::to_string(&AnswerEnvelope {
            version: 1,
            answers: ordered,
        })
        .map_err(|e| e.to_string())
    }

    pub fn answers(&self, validated: &str) -> Result<Vec<QuestionAnswer>, String> {
        serde_json::from_str::<AnswerEnvelope>(validated)
            .map(|v| v.answers)
            .map_err(|e| e.to_string())
    }

    pub fn display(&self, answers: &[QuestionAnswer]) -> String {
        if self.legacy {
            return format!("User answered: {}", answers[0].text());
        }
        self.questions
            .iter()
            .zip(answers)
            .map(|(q, a)| format!("[{}] {}\nUser answered: {}", q.id, q.question, a.text()))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn native_result(&self, original: &Value, answers: &[QuestionAnswer]) -> Value {
        let answers: serde_json::Map<String, Value> = self
            .questions
            .iter()
            .zip(answers)
            .enumerate()
            .map(|(index, (q, a))| {
                let key = original["questions"][index]["question"]
                    .as_str()
                    .unwrap_or(&q.question);
                (key.to_string(), json!(a.text()))
            })
            .collect();
        json!({"questions": original["questions"], "answers": answers})
    }
}

/// Atomically validate and take a pending input. Tool confirmations retain their
/// string contract; questionnaire validation failures do not consume the sender.
pub(crate) fn take_answer(
    pending: &mut std::collections::HashMap<String, crate::PendingQuestionResponse>,
    id: &str,
    value: &Value,
) -> Result<(crate::PendingQuestionResponse, String), String> {
    let response = pending
        .get(id)
        .ok_or_else(|| format!("Question '{id}' not found or already answered"))?;
    let answer = match response.form.as_ref() {
        Some(form) => form.validate_answer(value)?,
        None => value
            .as_str()
            .ok_or("This confirmation requires a string answer")?
            .to_string(),
    };
    Ok((
        pending.remove(id).expect("pending input held under lock"),
        answer,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn form() -> QuestionForm {
        QuestionForm::parse(&json!({"questions":[
            {"question":"Targets?","header":"Targets","multiSelect":true,"options":[{"label":"A"},{"label":"B"}]},
            {"question":"Reason?","options":[]}
        ]})).unwrap()
    }
    #[test]
    fn grouped_answers_keep_multiple_choices_custom_text_and_question_order() {
        let form = form();
        let validated = form
            .validate_answer(&json!({"version":1,"answers":[
                {"id":"q2","selected":[],"customText":" Because "},
                {"id":"q1","selected":["B","A"],"customText":"C"}
            ]}))
            .unwrap();
        let answers = form.answers(&validated).unwrap();
        assert_eq!(answers[0].selected, vec!["A", "B"]);
        assert_eq!(answers[0].custom_text, "C");
        assert_eq!(answers[1].custom_text, "Because");
        assert!(form.display(&answers).contains("A, B, C"));
        assert_eq!(
            form.native_result(&json!({"questions":[]}), &answers)["answers"]["Targets?"],
            "A, B, C"
        );
    }
    #[test]
    fn legacy_options_and_literal_string_answers_remain_compatible() {
        let form = QuestionForm::parse(&json!({"question":"Choose","options":[{"label":"A"},{"label":"Other","description":"Type"}]})).unwrap();
        assert_eq!(form.questions[0].options.len(), 1);
        for input in ["A", r#"{"version":1}"#] {
            let validated = form.validate_answer(&json!(input)).unwrap();
            assert_eq!(
                form.display(&form.answers(&validated).unwrap()),
                format!("User answered: {input}")
            );
        }
    }
    #[test]
    fn invalid_partial_duplicate_and_unknown_answers_are_rejected() {
        let form = form();
        for answers in [
            json!([]),
            json!([{"id":"q1","selected":["A"]},{"id":"q1","selected":["A"]}]),
            json!([{"id":"q1","selected":["missing"]},{"id":"q2","selected":[],"customText":"ok"}]),
            json!([{"id":"q1","selected":["A","A"]},{"id":"q2","selected":[],"customText":"ok"}]),
            json!([{"id":"q1","selected":["A"]},{"id":"q2","selected":[],"customText":" "}]),
        ] {
            assert!(form
                .validate_answer(&json!({"version":1,"answers":answers}))
                .is_err());
        }
        assert!(form.validate_answer(&json!("A")).is_err());
        let single = QuestionForm::parse(&json!({"questions":[{"question":"One","options":[{"label":"A"},{"label":"B"}],"allowCustom":false}]})).unwrap();
        for answer in [
            json!({"id":"q1","selected":["A","B"]}),
            json!({"id":"q1","selected":[],"customText":"no"}),
        ] {
            assert!(single
                .validate_answer(&json!({"version":1,"answers":[answer]}))
                .is_err());
        }
    }
    #[test]
    fn rejected_submissions_keep_the_pending_sender_and_success_is_exactly_once() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut pending = std::collections::HashMap::new();
        pending.insert(
            "group".into(),
            crate::PendingQuestionResponse {
                session_id: "s".into(),
                run_id: "r".into(),
                form: Some(form()),
                tx,
            },
        );
        assert!(take_answer(&mut pending, "group", &json!({"version":1,"answers":[]})).is_err());
        assert!(pending.contains_key("group"));
        let value = json!({"version":1,"answers":[{"id":"q1","selected":["A","B"]},{"id":"q2","selected":[],"customText":"reason"}]});
        let (response, answer) = take_answer(&mut pending, "group", &value).unwrap();
        response.tx.send(answer).unwrap();
        assert!(rx.try_recv().unwrap().contains("reason"));
        assert!(take_answer(&mut pending, "group", &value).is_err());
        let (tx, _) = tokio::sync::oneshot::channel();
        pending.insert(
            "confirm".into(),
            crate::PendingQuestionResponse {
                session_id: "s".into(),
                run_id: "r".into(),
                form: None,
                tx,
            },
        );
        assert!(take_answer(&mut pending, "confirm", &value).is_err());
        assert_eq!(
            take_answer(&mut pending, "confirm", &json!("approve"))
                .unwrap()
                .1,
            "approve"
        );
    }

    #[test]
    fn question_ids_and_options_are_validated_before_showing_ui() {
        for args in [
            json!({"questions":[]}),
            json!({"questions":[{"question":"","options":[]}]}),
            json!({"questions":[{"id":"same","question":"A"},{"id":"same","question":"B"}]}),
            json!({"questions":[{"question":"A","options":[{"label":"X"},{"label":"X"}]}]}),
            json!({"questions":[{"question":"A","allowCustom":false,"options":[]}]}),
        ] {
            assert!(QuestionForm::parse(&args).is_err(), "{args}");
        }
    }
}
