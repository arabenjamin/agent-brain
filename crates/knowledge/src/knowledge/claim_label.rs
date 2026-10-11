//! The label a retrieved claim carries ("[CLAIM · corroborated · …]"): what
//! the core says about a claim note when it shows one. Claims themselves are
//! the epistemics plugin's (`plugins/epistemics/`); their notes are read here.

/// Render a claim for inclusion in an LLM context window.
///
/// This is the piece that makes the whole design work. A `claim` note that
/// reaches `reason` looking like any other note *is* any other note as far as the
/// model is concerned — the type would be cosmetic. The label travels with the
/// text so the model cannot restate the claim without also seeing its standing.
/// How to read a corroborated claim of this kind.
///
/// The distinction that "corroborated" alone destroys: confirming that a group
/// *demonstrated a technique* is not confirming that the technique *works*.
/// Reporting can establish the former and says nothing about the latter, yet
/// both render as "corroborated" without this qualifier.
pub fn kind_qualifier(kind: &str, status: &str) -> Option<String> {
    if status != "corroborated" {
        return None;
    }
    match kind {
        "attribution" => Some("corroborates that it was asserted, not that it is true".to_string()),
        "mechanism" => Some("corroborates the causal claim itself".to_string()),
        _ => None,
    }
}

/// `age` is a pre-rendered relative age ("11 months ago") supplied by the
/// caller rather than computed here, so this stays a pure function of its
/// inputs and its tests stay deterministic. An absolute date alone does not
/// tell a model whether a figure is current — see `services/clock.rs`.
pub fn label_claim(
    content: &str,
    status: &str,
    asserted_by: Option<&str>,
    at: Option<&str>,
    tier: Option<&str>,
    kind: Option<&str>,
    age: Option<&str>,
) -> String {
    let mut parts = vec![format!("CLAIM · {status}")];
    if let Some(k) = kind.filter(|k| !k.is_empty()) {
        parts.push(k.to_string());
        if let Some(q) = kind_qualifier(k, status) {
            parts.push(q);
        }
    }
    // The tier is what separates "two wire services agree" from "five aligned
    // outlets repeat one origin" — both of which read as plain "corroborated".
    if let Some(t) = tier.filter(|t| !t.is_empty() && *t != "none") {
        parts.push(t.to_string());
    }
    if let Some(by) = asserted_by.filter(|s| !s.is_empty()) {
        parts.push(format!("asserted by {by}"));
    }
    if let Some(at) = at.filter(|s| !s.is_empty()) {
        parts.push(at.chars().take(10).collect());
    }
    if let Some(age) = age.filter(|s| !s.is_empty()) {
        parts.push(age.to_string());
    }
    format!("[{}]\n{content}", parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attribution_claim_says_what_corroboration_actually_established() {
        // The failure this exists for: "corroborated" next to a claim about
        // psionic summoning reads as endorsement of efficacy, when all that was
        // corroborated is that a group said they did it.
        let out = label_claim(
            "A group demonstrated psionic summoning.",
            "corroborated",
            Some("NewsNation"),
            Some("2026-08-10"),
            Some("unclassified sources only"),
            Some("attribution"),
            None,
        );
        assert!(
            out.contains("corroborates that it was asserted, not that it is true"),
            "{out}"
        );
    }

    #[test]
    fn an_event_claim_needs_no_qualifier() {
        // "Congress held hearings" corroborated means the hearings happened.
        assert!(kind_qualifier("event", "corroborated").is_none());
        let out = label_claim(
            "Congress held hearings.",
            "corroborated",
            None,
            None,
            Some("primary sources"),
            Some("event"),
            None,
        );
        assert!(!out.contains("not that it is true"), "{out}");
        assert!(out.contains("event"), "{out}");
    }

    #[test]
    fn qualifiers_only_apply_to_corroborated_claims() {
        // An unverified attribution has nothing to qualify, and adding the
        // phrase would imply a verification that never happened.
        assert!(kind_qualifier("attribution", "unverified").is_none());
        assert!(kind_qualifier("attribution", "refuted").is_none());
        assert!(kind_qualifier("mechanism", "corroborated").is_some());
    }

    #[test]
    fn tier_appears_in_the_label_so_the_distinction_is_visible() {
        // "corroborated" alone cannot distinguish two wire services agreeing
        // from five aligned outlets repeating one origin.
        let out = label_claim(
            "Skywatchers demonstrated psionic summoning.",
            "corroborated",
            Some("NewsNation"),
            Some("2026-08-10"),
            Some("unclassified sources only"),
            None,
            None,
        );
        assert!(out.starts_with("[CLAIM · corroborated · unclassified sources only · asserted by NewsNation · 2026-08-10]"), "{out}");
    }

    #[test]
    fn a_none_tier_is_omitted_rather_than_rendered() {
        let out = label_claim("x", "unverified", None, None, Some("none"), None, None);
        assert_eq!(out, "[CLAIM · unverified]\nx");
    }

    #[test]
    fn label_carries_status_and_attribution_into_context() {
        let out = label_claim(
            "The DoD uses TFRs to create controlled UAP zones.",
            "unverified",
            Some("NewsNation"),
            Some("2026-08-10T16:00:00Z"),
            None,
            None,
            None,
        );
        assert!(out.starts_with("[CLAIM · unverified · asserted by NewsNation · 2026-08-10]\n"));
        assert!(out.contains("The DoD uses TFRs"));
    }

    #[test]
    fn label_degrades_gracefully_without_attribution() {
        let out = label_claim(
            "Something was asserted.",
            "disputed",
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(out, "[CLAIM · disputed]\nSomething was asserted.");
        // An empty attribution must not render as "asserted by ".
        let out = label_claim(
            "x",
            "unverified",
            Some(""),
            Some(""),
            Some(""),
            Some(""),
            Some(""),
        );
        assert_eq!(out, "[CLAIM · unverified]\nx");
    }
}
