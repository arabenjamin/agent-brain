"""Refuse answers the brain would reject later, before anything is written.

Runs inside cookiecutter (create and every cruft update), so it only reads
the answers: no file outside the generated plugin is touched here.
"""

import re
import sys

name = "{{ cookiecutter.name }}"
description = """{{ cookiecutter.description }}"""
kind = "{{ cookiecutter.kind }}"
tools = [t.strip() for t in "{{ cookiecutter.tools }}".split(",") if t.strip()]
labels = [l.strip() for l in "{{ cookiecutter.graph_labels }}".split(",") if l.strip()]
env = [e.strip() for e in "{{ cookiecutter.needs_env }}".split(",") if e.strip()]

problems = []
if not re.fullmatch(r"[a-z][a-z0-9]*(-[a-z0-9]+)*", name) or len(name) > 40:
    problems.append(f"name '{name}': lowercase words joined by '-', at most 40 characters")
if name == "core":
    problems.append("name 'core' is the core bundle")
if not description.strip() or '"' in description or "\n" in description:
    problems.append("description: one line, no double quotes")
for t in tools:
    if not re.fullmatch(r"[a-z][a-z0-9]*(_[a-z0-9]+)*", t):
        problems.append(f"tool '{t}': snake_case")
if len(set(tools)) != len(tools):
    problems.append("tools: a name is given twice")
for l in labels:
    if not re.fullmatch(r"[A-Z][A-Za-z0-9]*", l):
        problems.append(f"graph label '{l}': PascalCase")
for e in env:
    if not re.fullmatch(r"[A-Z][A-Z0-9_]*", e):
        problems.append(f"env var '{e}': UPPER_SNAKE_CASE")
if kind == "data" and (tools or labels):
    problems.append("a data plugin has no crate: tools and graph labels need kind 'rust'")

if problems:
    print("templates/plugin: " + "; ".join(problems), file=sys.stderr)
    sys.exit(1)
