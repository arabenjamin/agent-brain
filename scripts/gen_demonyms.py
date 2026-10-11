"""Generate entities/demonyms.yaml from Wikidata P1549 (HBI plan Phase 20, §12.3).

Countries (current sovereign states) and U.S. states. A word shared by two
places is dropped as ambiguous; a demonym equal to its place's name is dropped
(Taiwan, New Zealand); REMOVE below is the hand review. The place is named as
our graph names it when an entity there links to the item, else by its English
label.

Usage: python3 -I scripts/gen_demonyms.py DIR, where DIR holds
- contact.txt: the WIKIMEDIA_CONTACT value (Wikimedia asks for one);
- placelinks.tsv, names.tsv, aliases.tsv: cypher-shell `--format plain`
  exports of the queries in the plan's Phase 20 outcome.
It writes DIR/demonyms.yaml; review the diff before copying it to plugins/core/entities/.
"""
import csv, json, re, sys, urllib.parse, urllib.request
from collections import defaultdict
S = sys.argv[1]
UA = f"agent-brain-dryrun/0.1 ({open(S+'/contact.txt').read().strip()})"
def rows(p):
    with open(p) as f:
        r = csv.reader(f, skipinitialspace=True); next(r); return list(r)
linked = defaultdict(list)
for qid, name, m in rows(S+'/placelinks.tsv'):
    linked[qid].append(name)            # most-mentioned first
names = {r[0] for r in rows(S+'/names.tsv')}
alias_of = {a: e for a, e in rows(S+'/aliases.tsv')}
sparql = """SELECT ?item ?label ?dem ?cls WHERE {
  { ?item wdt:P31 wd:Q3624078 . FILTER NOT EXISTS { ?item wdt:P576 [] } BIND("country" AS ?cls) }
  UNION { ?item wdt:P31 wd:Q35657 . BIND("us state" AS ?cls) }
  ?item rdfs:label ?label . FILTER(LANG(?label)='en')
  ?item wdt:P1549 ?dem . FILTER(LANG(?dem)='en') }"""
req = urllib.request.Request("https://query.wikidata.org/sparql?" + urllib.parse.urlencode({"query": sparql, "format": "json"}), headers={"User-Agent": UA})
b = json.load(urllib.request.urlopen(req, timeout=90))["results"]["bindings"]
labels = {}; cls = {}; dems = defaultdict(set)
for x in b:
    q = x["item"]["value"].rsplit("/", 1)[1]
    labels[q] = x["label"]["value"].lower(); cls[q] = x["cls"]["value"]
    dems[q].add(x["dem"]["value"].strip().lower())
# Our name for a place: an entity linked to its item, unless that entity is
# itself one of its demonyms ("italian" is linked to Italy); else the label.
place = {}
for q in labels:
    ok = [n for n in linked.get(q, []) if n not in dems[q] and n.rstrip("s") not in dems[q]]
    known_label = labels[q] in names or labels[q] in alias_of
    place[q] = labels[q] if known_label or labels[q] in ok or not ok else ok[0]
words = defaultdict(set)
for q in labels:
  for w in dems[q]:
    label = labels[q]
    if not re.fullmatch(r"[a-z][a-z' -]*[a-z]", w) or w == label or w == place[q]:
        continue
    forms = {w}
    if re.search(r"(an|i)$", w):         # american → americans, israeli → israelis
        forms.add(w + "s")
    for f in forms:
        words[f].add(q)
# Hand review (2026-10-10): words that usually mean something else.
REMOVE = {
    "finn": "also a name", "dane": "also a name",
    "vatican": "mostly the Holy See, an institution", "vaticans": "with vatican",
    "washingtonian": "the state or D.C.", "washingtonians": "with washingtonian",
    "persian": "the language, the Gulf, cats", "persians": "with persian",
    "tejano": "an ethnic group and a music", "mongol": "a people, not Mongolia's nationals",
    "hawaii resident": "not a word", "new hampshireman": "awkward form",
    "new hampshiremans": "bad plural", "new hampshirewoman": "awkward form", "new hampshirewomans": "bad plural",
}
RENAME = {"kingdom of denmark": "denmark", "kingdom of the netherlands": "netherlands", "the bahamas": "bahamas"}
for w in REMOVE:
    words.pop(w, None)
for q in place:
    place[q] = RENAME.get(place[q], place[q])
ambiguous = sorted(f for f, qs in words.items() if len(qs) > 1)
by_place = defaultdict(set)
for f, qs in words.items():
    if len(qs) == 1:
        by_place[next(iter(qs))].add(f)
out = []
out.append("# Demonyms: nationality and people words resolved to their place (HBI plan")
out.append("# Phase 20, §12.3; services/entity_resolution.rs). \"Chinese hackers\" mentions")
out.append("# China, with \"chinese\" kept as the mention's surface form.")
out.append("#")
out.append("# Generated 2026-10-10 from Wikidata's English demonyms (P1549) of current")
out.append("# sovereign states and U.S. states, then reviewed by hand. Plurals were")
out.append("# added for -an and -i words. Dropped:")
out.append("# - a demonym equal to its place's name (Taiwan, New Zealand);")
out.append("# - a word two places share, as ambiguous: " + ", ".join(ambiguous) + ".")
out.append("# - by hand, words that usually mean something else: " + "; ".join(f"{w} ({r})" for w, r in REMOVE.items() if not w.endswith("s") or w[:-1] not in REMOVE) + ".")
out.append("# `place` is the entity name the graph uses, where an entity is linked to")
out.append("# the place's Wikidata item, else the item's English label.")
out.append("#")
out.append("# Not every use is a nationality (\"Chinese\" is also a language). A pair the")
out.append("# user answers \"different\" or \"related\" in the merge review stops the word")
out.append("# resolving; remove it here too.")
out.append("")
out.append("demonyms:")
# Curated: places in the graph whose Wikidata item has no English demonym.
CURATED = [
    ("ukraine", "Q212", ["ukrainian", "ukrainians"]),
    ("saudi arabia", "Q851", ["saudi", "saudis", "saudi arabian", "saudi arabians"]),
    ("northern ireland", "Q26", ["northern irish"]),
]
for name, q, ws in CURATED:
    ws = [w for w in ws if w not in words]
    out.append(f"  - place: {json.dumps(name)}   # {q}, added by hand: Wikidata has no English demonym")
    out.append(f"    words: [{', '.join(json.dumps(w, ensure_ascii=False) for w in ws)}]")
for q in sorted(by_place, key=lambda q: place[q]):
    ws = sorted(by_place[q])
    known = place[q] in names or place[q] in alias_of
    out.append(f"  - place: {json.dumps(place[q], ensure_ascii=False)}   # {q}, {cls[q]}{'' if known else ', not in the graph yet'}")
    out.append(f"    words: [{', '.join(json.dumps(w, ensure_ascii=False) for w in ws)}]")
open(S+'/demonyms.yaml', 'w').write("\n".join(out) + "\n")
print("places", len(by_place), "words", sum(len(v) for v in by_place.values()), "ambiguous", len(ambiguous))
print("ambiguous:", ambiguous)
ent_hits = sorted({f for f in words if f in names and len(words[f]) == 1})
print("graph entities that are demonym words:", len(ent_hits), ent_hits)
