//! Reading JSON out of model output (moved from the core's `services/llm.rs`,
//! which re-exports it). Never slice first-`{` to last-`}`.

/// The first complete JSON object in `text` that has every key in `must_have`.
///
/// Parses from each `{` in turn and stops at the end of ONE value, so trailing
/// text cannot break it. Slicing first-`{` to last-`}` could: on 2026-09-26 a
/// model wrote its whole ```json answer twice, the slice spanned both objects,
/// the parse failed, and the fenced raw reply was stored as the report. Keys
/// are required so a broken outer object cannot yield one of its inner ones.
pub fn first_json_object(text: &str, must_have: &[&str]) -> Option<serde_json::Value> {
    first_json_object_strict(text, must_have).or_else(|| {
        // Models write LaTeX and Windows paths into JSON strings (`$\approx$`,
        // `C:\data`), and an invalid escape fails the whole value. Seen
        // 2026-09-30: the brain-exercise `reason` step's answer carried
        // `$\approx$`, the parse failed, the raw JSON became a Task goal, and
        // five evaluator retries of that task all failed. Retry once with each
        // stray backslash escaped.
        let repaired = escape_stray_backslashes(text);
        (repaired != text)
            .then(|| first_json_object_strict(&repaired, must_have))
            .flatten()
            .or_else(|| {
                // ministral writes a markdown report into "answer" with raw line
                // breaks and unescaped quotes (`**China's "Operation Fox Hunt"**`).
                // Every daily brief it wrote (2026-10-12..14) failed to parse, so
                // the fenced raw reply, inferences and gaps included, was stored,
                // sent to the owner and fed to claim extraction.
                let start = text.find('{')?;
                let repaired = repair_string_literals(&text[start..]);
                first_json_object_strict(&repaired, must_have)
            })
    })
}

/// Inside JSON string literals: escape raw line breaks and tabs, escape a
/// quote that cannot end the string (one not followed by `,` `}` `]` `:` or
/// the end), and double stray backslashes. Valid JSON comes back unchanged.
/// Only ever a fallback: a quote followed by one of those characters inside a
/// sentence still ends the string, and the parse fails as it would have.
pub fn repair_string_literals(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 32);
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if !in_string {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '"' => {
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                if matches!(next, None | Some(',' | '}' | ']' | ':')) {
                    in_string = false;
                    out.push('"');
                } else {
                    out.push_str("\\\"");
                }
            }
            '\\' => match chars.get(i + 1).copied() {
                Some(n @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u')) => {
                    out.push('\\');
                    out.push(n);
                    i += 1;
                }
                _ => out.push_str("\\\\"),
            },
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

/// Inside JSON string literals, double every backslash that does not begin a
/// valid escape (`\" \\ \/ \b \f \n \r \t \uXXXX`). Text outside strings is
/// left alone. Never makes valid JSON invalid.
pub fn escape_stray_backslashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if !in_string {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
            continue;
        }
        match c {
            '"' => {
                in_string = false;
                out.push(c);
            }
            '\\' => match chars.peek().copied() {
                Some(n @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')) => {
                    out.push('\\');
                    out.push(n);
                    chars.next();
                }
                Some('u') => {
                    let hex: String = chars.clone().skip(1).take(4).collect();
                    if hex.len() == 4 && hex.chars().all(|h| h.is_ascii_hexdigit()) {
                        out.push('\\');
                    } else {
                        out.push_str("\\\\");
                    }
                }
                _ => out.push_str("\\\\"),
            },
            _ => out.push(c),
        }
    }
    out
}

fn first_json_object_strict(text: &str, must_have: &[&str]) -> Option<serde_json::Value> {
    for (i, _) in text.match_indices('{').take(64) {
        let mut values =
            serde_json::Deserializer::from_str(&text[i..]).into_iter::<serde_json::Value>();
        if let Some(Ok(v)) = values.next()
            && let Some(obj) = v.as_object()
            && must_have.iter().all(|k| obj.contains_key(*k))
        {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_report_with_raw_line_breaks_and_inner_quotes_still_parses() {
        // The shape ministral-14b answered the daily brief in, 2026-10-13.
        let raw = "```json\n{\n  \"answer\": \"\n## EXECUTIVE SUMMARY\n- **China's \"Operation Fox Hunt\"** targets dissidents.\n\",\n  \"inferences\": [\"one\"],\n  \"confidence\": 0.9,\n  \"gaps\": []\n}\n```";
        let v = first_json_object(raw, &["answer"]).expect("parses after repair");
        assert_eq!(
            v["answer"],
            "\n## EXECUTIVE SUMMARY\n- **China's \"Operation Fox Hunt\"** targets dissidents.\n"
        );
        assert_eq!(v["inferences"], json!(["one"]));
    }

    #[test]
    fn valid_json_comes_back_unchanged() {
        for s in [
            r#"{"answer":"a \"q\" b","x":[1,2]}"#,
            r#"{"answer": "line\nnext", "gaps": []}"#,
            r#"{"answer":"He said \"hi\": ok","path":"C:\\data"}"#,
        ] {
            assert_eq!(repair_string_literals(s), s);
        }
    }
}
