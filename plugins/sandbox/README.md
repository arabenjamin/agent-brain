# sandbox

Run Python or Rust in an isolated, offline sandbox (execute_code): compute an answer instead of asserting it, or compile and test a crate.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update sandbox` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description |
| `crate/src/lib.rs` | the `Plugin` impl (`Sandbox`) |
| `crate/src/skill.rs` | `execute_code`, the client for the two sandboxes; its schema is built in Rust from the sandboxes configured |
| `python/` | the Python sandbox's image and server; the image also holds cookiecutter, for the developer plugin's `scaffold_plugin` |
| `rust/` | the Rust sandbox's image and server; `crates/Cargo.toml` lists the crates fetched into the image, since it runs offline |

The isolation contract (an internal network with no egress, no `ports:`) is in `docker-compose.yml`; a failed run is a successful tool call.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`); `execute_code` is offered only when `SANDBOX_URL` or `RUST_SANDBOX_URL` is set. Then
`cargo test -p agent-brain-plugin-sandbox --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
