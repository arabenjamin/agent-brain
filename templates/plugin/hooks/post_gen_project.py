"""Shape the generated plugin to its answers: drop what it does not have, and
write the one-file-per-item parts a template file cannot loop over.

Runs inside the generated plugin's directory, on create and on every cruft
update (in a temporary copy), so it never touches a file outside it. The
repo-level registration (crates/host, the Dockerfile) is
scripts/plugin_template.sh's job.
"""

import json
import os
import re
import shutil

kind = "{{ cookiecutter.kind }}"
tools = [t.strip() for t in "{{ cookiecutter.tools }}".split(",") if t.strip()]
labels = [l.strip() for l in "{{ cookiecutter.graph_labels }}".split(",") if l.strip()]
name = "{{ cookiecutter.name }}"


def snake(label: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", label).lower()


if kind == "data":
    shutil.rmtree("crate")
    shutil.rmtree("schemas", ignore_errors=True)
else:
    if not tools:
        os.remove("crate/src/skill.rs")
    for t in tools:
        os.makedirs("schemas/tools", exist_ok=True)
        schema = {
            "type": "object",
            "properties": {},
            "additionalProperties": False,
        }
        with open(f"schemas/tools/{t}.json", "w") as f:
            f.write(json.dumps(schema, indent=2) + "\n")
    if labels:
        os.makedirs("schemas/graph", exist_ok=True)
        lines = [
            f"// {name}'s Neo4j constraints and indexes, applied by the plugin's `start`",
            "// (agent_brain_plugin_api::graph::apply_schema). Each statement ends with `;`",
            "// and must be idempotent (IF NOT EXISTS). Docs: project-docs/schema.md.",
            "",
        ]
        for l in labels:
            lines.append(
                f"CREATE CONSTRAINT {snake(l)}_id IF NOT EXISTS FOR (n:{l}) REQUIRE n.id IS UNIQUE;"
            )
        with open("schemas/graph/schema.cypher", "w") as f:
            f.write("\n".join(lines) + "\n")
