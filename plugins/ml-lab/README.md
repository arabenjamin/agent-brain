# ml-lab

ML Lab: a machine-learning curriculum where every lesson ends in an exercise that ran, and the Kaggle Squares in Squares runner's daily report.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update ml-lab` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description, what it requires |
| `workflows/ml-study.yaml`, `ml-note-review.yaml` | the curriculum: the next lesson, read, then an exercise run in the Python sandbox; the weekly digest of the lab's notes |
| `workflows/squares-study.yaml`, `squares-report.yaml` | Kaggle Squares in Squares: one public method studied and reproduced; the runner's daily report |
| `workflows/playground-study.yaml`, `playground-report.yaml` | the Kaggle Playground Series: one source on the current competition a day, and the runner's cross-validated scores |
| `contexts/ml-student.yaml` | the agent that studies |
| `programs/ml-lab.yaml` | the program that owns these workflows |
| `views/ml_lab_status.yaml` | the `lookup` view of the lab's state |
| `runner/` | the `ml-runner` service's image (Python): the Squares solver and the Playground experiments it runs |

See `project-docs/ML_LAB_PLAN.md`.

Seeds go in the directories the brain reads (`workflows/`, `contexts/`,
`views/`, …); `scripts/check_schemas.sh` lists the kinds it knows. A data
plugin has no Rust: its workflows use the brain's own tools.

## Enable and check

Off by default: add `ml-lab` to `PLUGINS_ENABLED` (`.env`), and start the runner with `COMPOSE_PROFILES=ml-lab` (12 cores overnight; it submits to Kaggle).
