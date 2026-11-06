{%- set tools = cookiecutter.tools.split(',') | map('trim') | reject('equalto', '') | list -%}
{%- set labels = cookiecutter.graph_labels.split(',') | map('trim') | reject('equalto', '') | list -%}
# {{ cookiecutter.name }}

{{ cookiecutter.description }}

Stamped from `templates/plugin`; `.cruft.json` records the template commit.
`scripts/plugin_template.sh update {{ cookiecutter.name }}` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description, what it requires |
{%- if cookiecutter.kind == 'rust' %}
| `crate/src/lib.rs` | the `Plugin` impl (`{{ cookiecutter.__struct }}`) |
{%- if tools %}
| `crate/src/skill.rs` | the tools: {{ tools | join(', ') }} |
| `schemas/tools/<tool>.json` | each tool's input schema, loaded by `tool_schema!` |
{%- endif %}
{%- if labels %}
| `schemas/graph/schema.cypher` | constraints for {{ labels | join(', ') }}, applied on `start` |
{%- endif %}
{%- endif %}

Seeds go in the directories the brain reads (`workflows/`, `contexts/`,
`views/`, …); `scripts/check_schemas.sh` lists the kinds it knows. Graph
access goes in `crate/src/store.rs`.

## Enable and check

Add `{{ cookiecutter.name }}` to `PLUGINS_ENABLED` (`.env`).
{%- if cookiecutter.kind == 'rust' %} Then
`cargo test -p {{ cookiecutter.__crate }} --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
{%- endif %}
