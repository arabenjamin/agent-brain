//! A fixed choice with a probability per option, from a decision model (`nimble`)
//! on the local llama-server router.
//!
//! The prompt building and the scoring come from Ollama's `/v1/systemone`
//! (`decision/` at v0.40.2, the version every stored answer came from; MIT,
//! Copyright (c) Ollama, LICENSE-ollama in this crate). It only builds a prompt and
//! normalizes first-token probabilities, so it ports over `logprobs`. Later Ollama
//! versions build a different prompt, and the answer moves with it: the same topic
//! question scored 0.75 to 0.86 across these forms. LOCAL_ENGINE_PLAN.md Phase 2.
//!
//! **Order is part of the prompt.** Nimble's answer depends on the order of the
//! options and of the state's fields, so state must serialize in a fixed order
//! (a struct, never a `Value` map, which sorts its keys).

use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

const SYSTEM: &str = include_str!("nimble_system.txt");

/// How many first tokens to ask for. A candidate below this rank is an error,
/// never a probability of 0. The worst seen in 256 real questions was rank 16.
const TOP_LOGPROBS: u32 = 100;

/// Ollama's limit: the codes are the letters A to Z.
const MAX_CHOICES: usize = 26;

const DEFAULT_URL: &str = "http://localhost:8080";

/// The value of a yes/no question's "yes" in [`Answer::probabilities`].
pub const YES: &str = "true";

#[derive(Debug, Clone, Copy)]
pub enum Question<'a> {
    /// One of these `(name, description)` options, in this order.
    Choice {
        instructions: &'a str,
        criteria: &'a [(String, String)],
    },
    YesNo {
        instructions: &'a str,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// The most probable option; [`YES`] or `"false"` for a yes/no question.
    pub choice: String,
    /// Every option in question order, summing to 1.
    pub probabilities: Vec<(String, f64)>,
}

impl Answer {
    pub fn p(&self, option: &str) -> f64 {
        self.probabilities
            .iter()
            .find(|(n, _)| n == option)
            .map_or(0.0, |(_, p)| *p)
    }

    pub fn p_yes(&self) -> f64 {
        self.p(YES)
    }
}

#[derive(Serialize)]
struct Schema<'a> {
    context: &'a str,
    schema: [Field<'a>; 1],
}

#[derive(Serialize)]
struct Field<'a> {
    name: &'a str,
    description: &'a str,
    choices: Vec<Choice<'a>>,
}

#[derive(Serialize)]
struct Choice<'a> {
    code: String,
    value: Value,
    description: &'a str,
}

fn code(i: usize) -> String {
    char::from(b'A' + i as u8).to_string()
}

/// The option names in question order.
fn options(q: &Question) -> Vec<String> {
    match q {
        Question::YesNo { .. } => vec!["false".to_string(), YES.to_string()],
        Question::Choice { criteria, .. } => criteria.iter().map(|(n, _)| n.clone()).collect(),
    }
}

/// Compact JSON with a space after each `:` and `,` outside strings, the form
/// Ollama sends. serde_json's strings are escaped as Ollama's are.
fn spaced(compact: &str) -> String {
    let mut out = String::with_capacity(compact.len() + compact.len() / 8);
    let (mut in_string, mut escaped) = (false, false);
    for c in compact.chars() {
        out.push(c);
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else {
            match c {
                '"' => in_string = true,
                ':' | ',' => out.push(' '),
                _ => {}
            }
        }
    }
    out
}

/// The user message, as Ollama's Nimble encoding builds it: the state stays compact
/// inside a spaced schema.
pub fn prompt<S: Serialize>(state: &S, name: &str, q: &Question) -> Result<String, String> {
    let context = serde_json::to_string(state).map_err(|e| format!("state: {e}"))?;
    let (description, choices) = match q {
        Question::YesNo { instructions } => (
            *instructions,
            vec![
                Choice {
                    code: code(0),
                    value: json!(false),
                    description: "No",
                },
                Choice {
                    code: code(1),
                    value: json!(true),
                    description: "Yes",
                },
            ],
        ),
        Question::Choice {
            instructions,
            criteria,
        } => {
            if criteria.len() < 2 || criteria.len() > MAX_CHOICES {
                return Err(format!(
                    "a question takes 2 to {MAX_CHOICES} options, not {}",
                    criteria.len()
                ));
            }
            (
                *instructions,
                criteria
                    .iter()
                    .enumerate()
                    .map(|(i, (n, d))| Choice {
                        code: code(i),
                        value: json!(n),
                        description: d,
                    })
                    .collect(),
            )
        }
    };
    let data = serde_json::to_string(&Schema {
        context: &context,
        schema: [Field {
            name,
            description,
            choices,
        }],
    })
    .map_err(|e| format!("schema: {e}"))?;
    let requested = serde_json::to_string(name).map_err(|e| format!("name: {e}"))?;
    // Nimble escapes chat delimiters in the schema and the requested name.
    let escape = |s: String| s.replace('<', "\\u003c").replace('>', "\\u003e");
    Ok(format!(
        "{}\n\nRequested field: {}",
        escape(spaced(&data)),
        escape(requested)
    ))
}

/// From a chat completion with `logprobs`, the options' probabilities.
pub fn score(response: &Value, options: &[String]) -> Result<Answer, String> {
    let top = response["choices"][0]["logprobs"]["content"][0]["top_logprobs"]
        .as_array()
        .ok_or_else(|| {
            let text = response.to_string();
            format!(
                "no logprobs in the reply: {}",
                text.chars().take(200).collect::<String>()
            )
        })?;
    let mut logits = Vec::with_capacity(options.len());
    for (i, option) in options.iter().enumerate() {
        let want = code(i);
        let logit = top
            .iter()
            .find(|t| t["token"].as_str() == Some(&want))
            .and_then(|t| t["logprob"].as_f64())
            .ok_or_else(|| {
                format!(
                    "option '{option}' ({want}) is not among the top {} first tokens",
                    top.len()
                )
            })?;
        if !logit.is_finite() {
            return Err(format!("option '{option}' has a non-finite logit"));
        }
        logits.push(logit);
    }
    let peak = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let raw: Vec<f64> = logits.iter().map(|l| (l - peak).exp()).collect();
    let sum: f64 = raw.iter().sum();
    let probabilities: Vec<(String, f64)> = options
        .iter()
        .cloned()
        .zip(raw.iter().map(|p| p / sum))
        .collect();
    // The first of equals wins, as Ollama's `slices.Max` scan does.
    let mut choice = 0;
    for (i, (_, p)) in probabilities.iter().enumerate() {
        if *p > probabilities[choice].1 {
            choice = i;
        }
    }
    Ok(Answer {
        choice: probabilities[choice].0.clone(),
        probabilities,
    })
}

fn http() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            // The first call loads about 9.5 GB from disk.
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default()
    })
}

/// Ask `model`, a section of the router's preset. `name` is the question's name
/// and is part of the prompt.
pub async fn ask<S: Serialize>(
    model: &str,
    state: &S,
    name: &str,
    q: &Question<'_>,
) -> Result<Answer, String> {
    let user = prompt(state, name, q)?;
    let base = std::env::var("LLAMACPP_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let response = http()
        .post(format!(
            "{}/v1/chat/completions",
            base.trim_end_matches('/')
        ))
        .json(&json!({
            "model": model,
            "temperature": 0,
            "max_tokens": 1,
            "logprobs": true,
            "top_logprobs": TOP_LOGPROBS,
            // A reused prefix moves the score on this model (0.827 cached, 0.809 not,
            // same bytes), so an answer would depend on what ran before it.
            "cache_prompt": false,
            "messages": [
                {"role": "system", "content": SYSTEM},
                {"role": "user", "content": user},
            ],
        }))
        .send()
        .await
        .map_err(|e| format!("{model}: {e}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "{model} answered {status}: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{model}: not JSON: {e}"))?;
    score(&v, &options(q)).map_err(|e| format!("{model}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Api<'a> {
        zeta: &'a str,
        alpha: Vec<&'a str>,
    }

    /// What Ollama 0.40.2 renders for this request, checked against its Go code.
    #[test]
    fn the_prompt_is_the_form_ollama_sent() {
        let criteria = vec![
            (
                "news".to_string(),
                "Dated: <stories>, \"quoted\"".to_string(),
            ),
            ("api".to_string(), String::new()),
        ];
        let q = Question::Choice {
            instructions: "Which, kind?",
            criteria: &criteria,
        };
        let state = Api {
            zeta: "a: b, é\n",
            alpha: vec!["x", "y"],
        };
        let p = prompt(&state, "q", &q).unwrap();
        assert_eq!(
            p,
            r#"{"context": "{\"zeta\":\"a: b, é\\n\",\"alpha\":[\"x\",\"y\"]}", "schema": [{"name": "q", "description": "Which, kind?", "choices": [{"code": "A", "value": "news", "description": "Dated: \u003cstories\u003e, \"quoted\""}, {"code": "B", "value": "api", "description": ""}]}]}"#
                .to_string()
                + "\n\nRequested field: \"q\""
        );
    }

    #[test]
    fn a_yes_no_question_says_no_and_yes() {
        let p = prompt(
            &json!({"a": "b"}),
            "same",
            &Question::YesNo {
                instructions: "Is it?",
            },
        )
        .unwrap();
        assert!(
            p.contains(r#"{"code": "A", "value": false, "description": "No"}, {"code": "B", "value": true, "description": "Yes"}"#),
            "{p}"
        );
        assert!(p.ends_with("Requested field: \"same\""), "{p}");
    }

    fn reply(tokens: &[(&str, f64)]) -> Value {
        let top: Vec<Value> = tokens
            .iter()
            .map(|(t, l)| json!({"token": t, "logprob": l}))
            .collect();
        json!({"choices": [{"logprobs": {"content": [{"top_logprobs": top}]}}]})
    }

    #[test]
    fn scores_normalize_over_the_options_alone() {
        let names = vec!["a".to_string(), "b".to_string()];
        let a = score(
            &reply(&[("the", -0.1), ("B", -1.0), ("A", -1.0 - 3f64.ln())]),
            &names,
        )
        .unwrap();
        assert_eq!(a.choice, "b");
        assert!((a.p("b") - 0.75).abs() < 1e-9, "{a:?}");
        assert!((a.p("a") - 0.25).abs() < 1e-9, "{a:?}");
    }

    #[test]
    fn a_missing_option_is_an_error_not_zero() {
        let names = vec!["news".to_string(), "api".to_string()];
        let e = score(&reply(&[("A", -0.1), (" B", -3.0)]), &names).unwrap_err();
        assert!(e.contains("'api'") && e.contains("(B)"), "{e}");
    }

    #[test]
    fn a_reply_with_no_logprobs_says_what_came_back() {
        let e = score(&json!({"error": "model not found"}), &["a".to_string()]).unwrap_err();
        assert!(e.contains("model not found"), "{e}");
    }

    #[test]
    fn too_few_options_are_refused() {
        let one = vec![("only".to_string(), String::new())];
        let q = Question::Choice {
            instructions: "x",
            criteria: &one,
        };
        assert!(prompt(&json!("s"), "q", &q).is_err());
    }
}
