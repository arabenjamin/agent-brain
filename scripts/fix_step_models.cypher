// Keep the one schedule whose steps named a model running as it always has
// (HBI plan Phase 10).
//
// Until Phase 10 a step's `model` was not read: serde dropped it, and the
// step ran on its provider_hint alone. Zero Trust Architecture Research
// (93cb3808, runtime-owned) carries `model: gemma4:latest` on both steps, one
// of them beside `provider_hint: ollama-cloud`. Once `model` is honoured, both
// steps would move to host Ollama's gemma4:latest, which shares the 8 GB GPU
// with the llama.cpp sidecar, and step 1's hint contradicts its model, which
// upsert validation now rejects. Removing the never-read field keeps the
// schedule exactly as it has run: step 1 on the cloud default, step 2 on the
// local background model. To pin a model on purpose, set one in the step
// editor. Run BEFORE deploying Phase 10. The old steps stay in
// steps_before_model_fix; undo: SET s.steps = s.steps_before_model_fix.

// 1. Dry run: which schedules name a model in a step
MATCH (s:ScheduledTask) WHERE s.steps CONTAINS '"model":'
RETURN s.id, s.name, s.managed_by, s.steps;

// 2. Apply (the Zero Trust schedule only)
MATCH (s:ScheduledTask {id: '93cb3808-af8f-4115-95fc-ccee05db549d'})
WHERE s.steps CONTAINS ',"model":"gemma4:latest"'
SET s.steps_before_model_fix = s.steps,
    s.steps = replace(s.steps, ',"model":"gemma4:latest"', ''),
    s.updated_at = datetime()
RETURN s.name, s.steps;
