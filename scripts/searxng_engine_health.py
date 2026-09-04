#!/usr/bin/env python3
"""Measure which SearXNG engines actually answer from this host's IP.

WHY THIS EXISTS: SearXNG returns HTTP 200 with `{"results": []}` when every
upstream engine fails. That is not an error, so `search_web`'s failover ladder
(`skills/search.rs`) treats rung one as having answered and never falls
through, and the DuckDB `search_usage` ledger records the attempt as a success
— because at the HTTP level it was one. The result is a search backend that is
completely dead and looks completely healthy. It has happened twice:

  2026-08-18  DNS died inside the container; every upstream failed to connect.
  2026-08-31  Every default engine (brave/duckduckgo/google cse/startpage)
              began answering CAPTCHA or "too many requests" to this IP.

Different causes, identical symptom, and neither surfaced anywhere until
someone noticed the brain had quietly stopped citing sources. The signal that
distinguishes them is `unresponsive_engines` in the JSON response, which is
what this script reads.

USAGE
    python3 scripts/searxng_engine_health.py [-n QUERIES] [--url URL]

Runs from the host and reaches SearXNG through the brain container's network,
since the service is deliberately not published to a host port.

Exit status is 1 if no engine returned a single result across all probes —
i.e. the search backend is down — so this is usable from a healthcheck.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import urllib.parse
from collections import defaultdict

# Deliberately short and varied: plain keywords. Long natural-language queries
# are themselves a trigger for bot detection and return worse results, so
# probing with them would conflate two problems.
#
# TWO SHAPES, and the split is the point. Until 2026-09-01 this list held only
# the EVERGREEN probes below, and it chose an engine set that scored 137
# results while being unable to answer a single news question. Encyclopedic and
# niche-index engines (`wikipedia`, `encyclosearch`, `wiby`, and the foreign
# portals) answer "HBM3E production capacity" well and "top US national news
# headlines" not at all — `wiby` deliberately indexes vintage pages, `seznam`
# is Czech, `naver` Korean. A probe set that only asks evergreen questions
# cannot see that, and the daily news brief — the brain's highest-volume search
# consumer at 8 queries/day — silently degraded to empty sections for a day
# while this script reported four healthy engines.
#
# A result COUNT still cannot measure relevance (see the query-rewriting note
# in CLAUDE.md), so a high score on the FRESH probes is not proof the brief
# will be good. But a low score is proof it will be bad, which is what this is
# for. Read the two groups separately; do not sum them.
EVERGREEN_PROBES = [
    "reticulum mesh network",
    "sodium ion battery cathode",
    "HBM3E production capacity",
    "meshtastic lora firmware",
]

# Time-sensitive general queries, mirroring schedules/daily-news.yaml.
FRESH_PROBES = [
    "top US national news headlines",
    "metro Detroit news headlines",
    "AI technology science news",
]

DEFAULT_PROBES = EVERGREEN_PROBES + FRESH_PROBES

CONTAINER = "agent-brain-searxng-1"


def probe(url: str, query: str, timeout: int = 45) -> dict:
    """Run one search from inside the SearXNG container and parse the JSON."""
    script = (
        "import urllib.request, json, sys\n"
        f"u = {url!r} + '?q=' + {urllib.parse.quote(query)!r} + '&format=json'\n"
        "try:\n"
        "    print(urllib.request.urlopen(u, timeout=40).read().decode())\n"
        "except Exception as e:\n"
        "    print(json.dumps({'__error__': str(e)}))\n"
    )
    out = subprocess.run(
        ["docker", "exec", CONTAINER, "python3", "-c", script],
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    if out.returncode != 0:
        return {"__error__": out.stderr.strip() or "docker exec failed"}
    try:
        return json.loads(out.stdout)
    except json.JSONDecodeError:
        return {"__error__": f"unparseable response: {out.stdout[:200]}"}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("-n", type=int, default=len(DEFAULT_PROBES),
                    help="how many probe queries to run")
    ap.add_argument("--url", default="http://localhost:8080/search")
    args = ap.parse_args()

    probes = DEFAULT_PROBES[: args.n]
    # engine -> [results contributed]; engine -> {reason: count}
    contributed: dict[str, int] = defaultdict(int)
    failures: dict[str, dict[str, int]] = defaultdict(lambda: defaultdict(int))
    totals: list[int] = []
    fresh_domains: dict[str, int] = defaultdict(int)

    for q in probes:
        data = probe(args.url, q)
        if "__error__" in data:
            print(f"  {q!r:45} ERROR {data['__error__']}")
            totals.append(0)
            continue

        results = data.get("results", [])
        totals.append(len(results))
        for r in results:
            for e in r.get("engines", []):
                contributed[e] += 1
            if q in FRESH_PROBES:
                netloc = urllib.parse.urlparse(r.get("url", "")).netloc
                if netloc:
                    fresh_domains[netloc] += 1
        for entry in data.get("unresponsive_engines", []):
            # [name, reason] or [name, reason, extra]
            if entry:
                failures[entry[0]][entry[1] if len(entry) > 1 else "?"] += 1

        kind = "fresh" if q in FRESH_PROBES else "evergreen"
        print(f"  [{kind:9}] {q!r:45} {len(results):>3} results")

    # Report the two shapes separately — a healthy evergreen score masks a dead
    # news path, which is exactly how the 2026-09-01 empty brief happened.
    fresh_total = sum(n for q, n in zip(probes, totals) if q in FRESH_PROBES)
    fresh_count = sum(1 for q in probes if q in FRESH_PROBES)
    if fresh_count:
        print()
        print(f"  fresh/news probes: {fresh_total} results over {fresh_count} queries")
        if fresh_total == 0:
            print("  WARNING: no engine answered a time-sensitive query — the daily")
            print("           news brief will render empty sections. Check whether the")
            print("           mainstream engines are CAPTCHA-blocked in settings.yml.")
        elif fresh_domains:
            # A COUNT CANNOT CATCH THIS, and pretending otherwise is worse than
            # not checking. On 2026-09-01 the fresh probes returned 145 results
            # and the news brief still came out empty, because the results were
            # a disability-rights page from 2004, rationalwiki, and whale.to.
            # SearXNG results carry no `publishedDate` from these engines (0/52
            # measured), so there is no recency signal to test either. The only
            # reliable check is a human glancing at the domains — so print them.
            print("  top domains on fresh queries — EYEBALL THESE, a high count")
            print("  above proves nothing about whether they are usable news:")
            for dom, n in sorted(fresh_domains.items(), key=lambda kv: -kv[1])[:8]:
                print(f"    {n:>3}  {dom}")

    print()
    print(f"{'ENGINE':<20} {'RESULTS':>8}   STATUS")
    print("-" * 62)
    healthy = sorted(contributed.items(), key=lambda kv: -kv[1])
    for name, n in healthy:
        print(f"{name:<20} {n:>8}   ok")
    for name, reasons in sorted(failures.items()):
        detail = ", ".join(f"{r} x{c}" for r, c in reasons.items())
        print(f"{name:<20} {0:>8}   FAILING: {detail}")

    print()
    total = sum(totals)
    print(f"{len(probes)} probes, {total} results, "
          f"{len(healthy)} engine(s) contributing")

    if total == 0:
        print()
        print("SEARCH BACKEND IS DOWN. Every probe returned zero results, which "
              "SearXNG reports as HTTP 200 and the ladder reads as a successful "
              "rung. Check the STATUS column above: 'CAPTCHA' / 'too many "
              "requests' means bot detection (swap engines in "
              "searxng/settings.yml); connection errors mean DNS or egress "
              "(check `getent hosts` inside the container).")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
