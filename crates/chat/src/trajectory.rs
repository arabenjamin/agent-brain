//! A sub-agent run as a composer comparing two runs needs it: each tool call
//! with its arguments, outcome and the head of its result, then the answer
//! (HARNESS2_PLAN Phase 1). Written by the brain's deep-diver (gpt-oss,
//! 2026-10-23) with its tests run in the sandbox; one test's count fixed in
//! review.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ARGS_CHARS: usize = 400;
pub const RESULT_CHARS: usize = 600;
pub const ANSWER_CHARS: usize = 4000;
pub const MAX_STEPS: usize = 40;

/// Cut a string to at most `max` characters (UTF‑8 safe). If truncated,
/// append the marker "…[+N chars]" where N is the number of dropped characters.
pub fn cut(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            break;
        }
        out.push(ch);
    }
    let dropped = total - max;
    format!("{}…[+{} chars]", out, dropped)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub tool: String,
    pub args: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stubbed: bool,
    pub result: String,
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Trajectory {
    pub steps: Vec<Step>,
    pub answer: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped: usize,
}

impl Trajectory {
    /// Record a tool call. Arguments are serialized to compact JSON and cut.
    /// Result is cut as well. If the step limit is exceeded, the call is counted
    /// in `dropped` but not stored.
    pub fn push(&mut self, tool: &str, args: &Value, ok: bool, stubbed: bool, result: &str) {
        let args_json = serde_json::to_string(args).unwrap_or_else(|_| "null".to_string());
        let args_cut = cut(&args_json, ARGS_CHARS);
        let result_cut = cut(result, RESULT_CHARS);
        if self.steps.len() < MAX_STEPS {
            self.steps.push(Step {
                tool: tool.to_string(),
                args: args_cut,
                ok,
                stubbed,
                result: result_cut,
            });
        } else {
            self.dropped += 1;
        }
    }

    /// Store the final answer, cutting if necessary.
    pub fn set_answer(&mut self, answer: &str) {
        self.answer = cut(answer, ANSWER_CHARS);
    }

    /// Number of steps whose `ok` flag is false.
    pub fn failed(&self) -> usize {
        self.steps.iter().filter(|s| !s.ok).count()
    }

    /// Serialize the whole trajectory to compact JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }

    /// Render a human‑readable multiline view.
    /// Each line: `n. tool(args) -> STATUS: result`
    /// Newlines in results are replaced with spaces.
    pub fn render(&self) -> String {
        let mut lines = vec![self.calls_line()];
        for (i, step) in self.steps.iter().enumerate() {
            let status = if step.stubbed {
                "STUB"
            } else if step.ok {
                "ok"
            } else {
                "FAILED"
            };
            let result_one_line = step.result.replace('\n', " ");
            lines.push(format!(
                "{}. {}({}) -> {}: {}",
                i + 1,
                step.tool,
                step.args,
                status,
                result_one_line
            ));
        }
        if self.dropped > 0 {
            lines.push(format!("… {} more calls not shown", self.dropped));
        }
        lines.push(format!("ANSWER: {}", self.answer));
        lines.join("\n")
    }

    /// The run's calls in one line, `CALLS: search_web → read_page →
    /// create_task(STUB)`: what a judge comparing several runs reads first.
    /// On 2026-10-23 a composer and a reflector reading three numbered
    /// trajectories credited one run's `create_task` to another and called a
    /// run that never made it defect-free.
    pub fn calls_line(&self) -> String {
        let calls: Vec<String> = self
            .steps
            .iter()
            .map(|s| match (s.stubbed, s.ok) {
                (true, _) => format!("{}(STUB)", s.tool),
                (false, true) => s.tool.clone(),
                (false, false) => format!("{}(FAILED)", s.tool),
            })
            .collect();
        let more = if self.dropped > 0 {
            format!(" → … {} more", self.dropped)
        } else {
            String::new()
        };
        if calls.is_empty() {
            "CALLS: none".to_string()
        } else {
            format!("CALLS: {}{more}", calls.join(" → "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_cut_short() {
        assert_eq!(cut("abc", 5), "abc");
    }

    #[test]
    fn test_cut_utf8() {
        let s = "héllo wörld"; // 11 chars
        assert_eq!(cut(s, 5), "héllo…[+6 chars]");
    }

    #[test]
    fn test_push_and_dropped() {
        let mut traj = Trajectory::default();
        for i in 0..41 {
            traj.push("tool", &json!({"i": i}), true, false, "result");
        }
        assert_eq!(traj.steps.len(), MAX_STEPS);
        assert_eq!(traj.dropped, 1);
        assert_eq!(traj.steps[0].args, r#"{"i":0}"#);
        assert_eq!(traj.steps[0].result, "result");
    }

    #[test]
    fn test_render() {
        let mut traj = Trajectory::default();
        traj.push("t1", &json!({}), false, false, "fail\nmsg");
        traj.push("t2", &json!({}), true, true, "stubbed result");
        traj.set_answer("final answer");
        for _ in 0..MAX_STEPS {
            traj.push("t", &json!({}), true, false, "x");
        }
        let rendered = traj.render();
        assert!(rendered.contains("FAILED"));
        assert!(rendered.contains("STUB"));
        assert!(rendered.contains("… 2 more calls not shown"));
        assert!(rendered.contains("ANSWER: final answer"));
        assert!(
            rendered.starts_with("CALLS: t1(FAILED) → t2(STUB) → t → "),
            "{rendered}"
        );
        assert!(rendered.lines().next().unwrap().ends_with("→ … 2 more"));
        assert_eq!(Trajectory::default().calls_line(), "CALLS: none");
    }

    #[test]
    fn test_to_json_roundtrip() {
        let mut traj = Trajectory::default();
        traj.push("t", &json!({"a":1}), true, false, "res");
        traj.set_answer("ans");
        let json_str = traj.to_json();
        assert!(!json_str.contains("stubbed"));
        assert!(!json_str.contains("dropped"));
        let deserialized: Trajectory = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deserialized, traj);
    }
}
