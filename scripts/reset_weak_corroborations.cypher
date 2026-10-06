// Reset claim corroborations that rested on a platform post or a search page.
//
// Before the 2026-10-06 verifier fix, a "supports" verdict needed only (a) a
// cited link that was among the results and (b) any two distinct result
// domains, cited or not. So the judge's single cited source was the whole
// corroboration, and 86 of the 269 since 2026-09-26 were a Facebook/Instagram/
// YouTube-type post or a search engine's own results page echoing the claim
// verbatim ("Search for <claim> with Ecosia"). This resets those.
//
// Edges written by the fixed verifier carry `r.sources`; older ones do not, so
// `r.sources IS NULL` is the bound and no deploy timestamp is needed.
//
// Run AFTER the fix is deployed (the `platforms` SourceList must exist — step 0
// checks — and the old verifier must be gone, or it re-judges the reset claims
// first under the old rules). Run the statements one at a time; read step 1.
// See project-docs/subsystem-rules.md § Epistemics.

// 0. The platform list is seeded at startup from sources/platforms.yaml.
MATCH (s:SourceList {role:'platform'}) RETURN s.name, size(s.domains) AS domains;

// 1. Count (dry run)
MATCH (s:SourceList {role:'platform'})
WITH collect(s.domains) AS lists
WITH reduce(acc = [], l IN lists | acc + l) AS platforms
MATCH (c:Note {note_type:'claim'})-[r:CORROBORATED_BY]->()
WHERE r.sources IS NULL AND r.found_at >= datetime('2026-09-26T17:48:00Z')
WITH c, r, platforms, replace(toLower(split(r.source, '/')[2]), 'www.', '') AS host
WITH c, r,
     any(p IN platforms WHERE host = p OR host ENDS WITH '.' + p) AS platform,
     (r.source =~ '(?i)^https?://[^/]+(/[^?#]*)?/search([/?#].*)?$'
      OR (host STARTS WITH 'search.' AND r.source CONTAINS '?')) AS search_page
WHERE platform OR search_page
RETURN sum(CASE WHEN platform THEN 1 ELSE 0 END) AS platform_edges,
       sum(CASE WHEN search_page THEN 1 ELSE 0 END) AS search_page_edges,
       count(DISTINCT c) AS claims;

// 2. Tag the evidence notes behind those edges. They stay as the audit trail.
MATCH (s:SourceList {role:'platform'})
WITH reduce(acc = [], l IN collect(s.domains) | acc + l) AS platforms
MATCH (c:Note {note_type:'claim'})-[r:CORROBORATED_BY]->(e:Note)
WHERE r.sources IS NULL AND r.found_at >= datetime('2026-09-26T17:48:00Z')
WITH e, r, platforms, replace(toLower(split(r.source, '/')[2]), 'www.', '') AS host
WHERE any(p IN platforms WHERE host = p OR host ENDS WITH '.' + p)
   OR r.source =~ '(?i)^https?://[^/]+(/[^?#]*)?/search([/?#].*)?$'
   OR (host STARTS WITH 'search.' AND r.source CONTAINS '?')
SET e.weak_corroboration = true
RETURN count(DISTINCT e) AS evidence_notes_tagged;

// 3. Delete the edges and re-derive status the way recompute_status does
//    (services/claims.rs: ClaimStatus::from_evidence). The tier is cleared
//    where corroboration remains and set to 'none' where none does; the next
//    recompute restores it. Clearing last_verify_attempt_at puts these claims
//    at the front of the verification queue.
MATCH (s:SourceList {role:'platform'})
WITH reduce(acc = [], l IN collect(s.domains) | acc + l) AS platforms
MATCH (c:Note {note_type:'claim'})-[r:CORROBORATED_BY]->()
WHERE r.sources IS NULL AND r.found_at >= datetime('2026-09-26T17:48:00Z')
WITH c, r, platforms, replace(toLower(split(r.source, '/')[2]), 'www.', '') AS host
WHERE any(p IN platforms WHERE host = p OR host ENDS WITH '.' + p)
   OR r.source =~ '(?i)^https?://[^/]+(/[^?#]*)?/search([/?#].*)?$'
   OR (host STARTS WITH 'search.' AND r.source CONTAINS '?')
WITH c, collect(r) AS rs
FOREACH (x IN rs | DELETE x)
WITH DISTINCT c
OPTIONAL MATCH (c)-[cor:CORROBORATED_BY]->()
WITH c, count(cor) AS corr, collect(cor) AS cors
OPTIONAL MATCH (c)-[con:CONTRADICTED_BY]->()
WITH c, corr, cors, count(con) AS contra
WITH c, corr, contra,
     reduce(acc = [], r IN cors |
       acc + [d IN COALESCE(r.domains, []) WHERE NOT d IN acc]) AS domains
SET c.claim_status = CASE
      WHEN corr > 0 AND contra > 0 THEN 'disputed'
      WHEN corr > 0 THEN 'corroborated'
      WHEN contra > 0 THEN 'refuted'
      ELSE 'unverified' END,
    c.corroborating_count = corr,
    c.contradicting_count = contra,
    c.corroborating_domains = domains,
    c.corroboration_tier = CASE WHEN corr > 0 THEN null ELSE 'none' END,
    c.last_verify_attempt_at = null,
    c.reset_reason = 'corroborated by a platform post or search page, 2026-09-26..2026-10-06'
RETURN c.claim_status AS status, count(*) AS n;
