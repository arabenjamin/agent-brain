//! What this process is: the commit, whether the tree had other changes in it,
//! and when the image was built (OUTSIDE_REVIEW_PLAN Phase 3). The values are
//! baked into the image at build time (`scripts/build_identity.sh`, Dockerfile),
//! so they describe the code loaded here, whatever the mounted repo has moved
//! on to since. A build that was not given them says so with nulls.

use serde_json::{Value, json};

fn value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The full commit this image was built from, if it was told.
pub fn commit() -> Option<String> {
    value("BRAIN_GIT_COMMIT")
}

/// Whether the build tree had changes beyond that commit, if it was told.
pub fn dirty() -> Option<bool> {
    value("BRAIN_GIT_DIRTY").map(|v| v == "true")
}

/// The identity `/health` reports. `container` is Docker's id for this
/// container: `docker inspect -f '{{.Image}}' <container>` gives the image.
pub fn identity() -> Value {
    json!({
        "commit": commit(),
        "dirty": dirty(),
        "built_at": value("BRAIN_BUILD_TIME"),
        "container": value("HOSTNAME"),
    })
}
