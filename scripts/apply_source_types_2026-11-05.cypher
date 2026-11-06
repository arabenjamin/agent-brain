// The owner's source tiers for the watchlist (2026-11-05,
// SOURCE_QUALITY_PLAN.md Phase 1 step 2). Each change is kept as a dated
// (:MediaSource)-[:RETYPED]->(:SourceTypeChange), the same record
// `manage_media_source` retype writes, so a later re-rank has the history.
// Idempotent: a source already at its tier is left alone. Run once the new code
// is live. A source not listed stays unset, which counts as commentary.

// 1. Dry run: what would change
UNWIND $tiers AS t
MATCH (s:MediaSource {name: t.name})
WHERE coalesce(s.source_type, '') <> t.to
RETURN s.name AS source, coalesce(s.source_type, 'unset') AS from, t.to AS to;

// 2. Apply
UNWIND $tiers AS t
MATCH (s:MediaSource {name: t.name})
WHERE coalesce(s.source_type, '') <> t.to
WITH s, t, coalesce(s.source_type, '') AS was
CREATE (s)-[:RETYPED]->(:SourceTypeChange {from: was, to: t.to, at: datetime(),
                                           by: 'owner', reason: t.reason})
SET s.source_type = t.to, s.source_type_since = datetime(), s.updated_at = datetime()
RETURN s.source_type AS tier, count(*) AS n;

// $tiers, one row per source:
// :param tiers => [
//   {name:'c-span',            to:'primary',    reason:'The proceedings themselves; each claim is its speaker\'s.'},
//   {name:'citizen-lab',       to:'primary',    reason:'Forensic research reports with published evidence: the finding itself (owner).'},
//   {name:'60-minutes',        to:'reporting',  reason:'Original investigative reporting with named sources.'},
//   {name:'vice-news',         to:'reporting',  reason:'Documentary field reporting.'},
//   {name:'the-record',        to:'reporting',  reason:'Cyber and intel news with its own reporters; owned by Recorded Future.'},
//   {name:'occrp',             to:'reporting',  reason:'Investigations built on documents and published sources.'},
//   {name:'icij',              to:'reporting',  reason:'Cross-border leak investigations.'},
//   {name:'bellingcat',        to:'reporting',  reason:'Open-source investigations with published methods.'},
//   {name:'c4ads',             to:'reporting',  reason:'Data-driven investigations of illicit networks.'},
//   {name:'louis-rossmann',    to:'analysis',   reason:'Open-source journalism that is also commentary (owner): between the two.'},
//   {name:'channel-5',         to:'analysis',   reason:'Open-source field journalism that is also commentary (owner): between the two.'},
//   {name:'cappy-army',        to:'analysis',   reason:'Military analysis from public reporting and imagery.'},
//   {name:'johnny-harris',     to:'analysis',   reason:'Explainer journalism synthesising others\' reporting.'},
//   {name:'two-minute-papers', to:'analysis',   reason:'Summaries of papers; the paper is primary.'},
//   {name:'andrej-karpathy',   to:'analysis',   reason:'A practitioner teaching from his own work.'},
//   {name:'dfrlab',            to:'analysis',   reason:'Think-tank research on influence operations; NATO-aligned.'},
//   {name:'38-north',          to:'analysis',   reason:'Think-tank analysis including satellite imagery (Stimson Center).'},
//   {name:'news-nation',       to:'commentary', reason:'Mostly commentary (owner): opinion-hour clips.'},
//   {name:'adam-kinzinger',    to:'commentary', reason:'A former politician\'s opinion.'},
//   {name:'hasan-minhaj',      to:'commentary', reason:'Political comedy.'},
//   {name:'julian-dorey',      to:'commentary', reason:'Long-form interview podcast; guests\' claims are not vetted.'},
//   {name:'ibm-technology',    to:'commentary', reason:'Vendor explainers restating general knowledge.'},
//   {name:'deep-learning-ai',  to:'commentary', reason:'Course and explainer content.'}
// ]
