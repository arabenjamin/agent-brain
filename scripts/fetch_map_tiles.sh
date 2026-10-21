#!/usr/bin/env bash
# Self-hosted map tiles (MAP_PLAN.md): Protomaps vector tiles and the
# basemap's fonts and sprites, into ./maps (gitignored). The HBI serves the
# directory read-only at /maps/, so viewing a map never leaves the network.
#
#   scripts/fetch_map_tiles.sh            # world + every region in maps/regions.yaml
#   WORLD_MAXZOOM=8 scripts/fetch_map_tiles.sh
#
# Extracts from Protomaps' daily planet build by HTTP range requests with
# go-pmtiles in Docker (nothing installed on the host): only the tiles asked
# for are downloaded. Re-run to refresh; a few times a year is plenty.
# Sizes measured 2026-10-21: world to z6 45 MB, z7 189 MB, z8 559 MB;
# Michigan to z12 128 MB, z14 561 MB, z15 1.2 GB.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT=maps
PMTILES_IMAGE=protomaps/go-pmtiles:v1.31.2
# protomaps/basemaps-assets, pinned (2025-10-31): fonts + sprites v4.
ASSETS_SHA=028c18f713baecad011301ff7a69acc39bcc2ae7
WORLD_MAXZOOM=${WORLD_MAXZOOM:-7}
BUILD=${PROTOMAPS_BUILD:-$(curl -fsS https://build-metadata.protomaps.dev/builds.json | jq -r '.[-1].key')}
SRC="https://build.protomaps.com/$BUILD"

mkdir -p "$OUT"
if [ ! -f "$OUT/regions.yaml" ]; then
  cat > "$OUT/regions.yaml" <<'EOF'
# Regions extracted in detail on top of the world tiles (MAP_PLAN.md §5.1).
# bbox: min_lon,min_lat,max_lon,max_lat. Add an NIA area as one more entry.
regions:
  - name: michigan
    label: Michigan
    bbox: [-90.5, 41.6, -82.1, 48.4]
    maxzoom: 14
EOF
  echo "wrote $OUT/regions.yaml (default: Michigan to zoom 14)"
fi

extract() { # name, extra args...
  local name=$1; shift
  echo "== $name.pmtiles from $BUILD $*"
  docker run --rm -u "$(id -u):$(id -g)" -v "$PWD/$OUT:/out" "$PMTILES_IMAGE" \
    extract "$SRC" "/out/$name.pmtiles.tmp" "$@"
  mv "$OUT/$name.pmtiles.tmp" "$OUT/$name.pmtiles"
}

extract world --maxzoom="$WORLD_MAXZOOM"

# Regions: name|bbox|maxzoom|label lines from the YAML.
mapfile -t REGIONS < <(python3 -I - "$OUT/regions.yaml" <<'PY'
import sys, yaml
for r in (yaml.safe_load(open(sys.argv[1])) or {}).get("regions") or []:
    b = ",".join(str(x) for x in r["bbox"])
    print(f'{r["name"]}|{b}|{int(r.get("maxzoom", 14))}|{r.get("label", r["name"])}')
PY
)
for line in "${REGIONS[@]}"; do
  IFS='|' read -r name bbox maxzoom _ <<<"$line"
  extract "region-$name" --bbox="$bbox" --maxzoom="$maxzoom"
done

echo "== basemap fonts and sprites ($ASSETS_SHA)"
tmp=$(mktemp -d)
curl -fsSL "https://codeload.github.com/protomaps/basemaps-assets/tar.gz/$ASSETS_SHA" | tar -xz -C "$tmp"
rm -rf "$OUT/assets" && mkdir -p "$OUT/assets"
cp -r "$tmp"/basemaps-assets-*/fonts "$OUT/assets/fonts"
cp -r "$tmp"/basemaps-assets-*/sprites "$OUT/assets/sprites"
rm -rf "$tmp"

# What the HBI reads to know which tile sources exist (no list in the code).
python3 -I - "$OUT" "$BUILD" "$WORLD_MAXZOOM" "${REGIONS[@]}" <<'PY'
import json, sys, datetime
out, build, wz, regions = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4:]
sources = [{"name": "world", "file": "world.pmtiles", "maxzoom": wz, "bbox": [-180, -85, 180, 85], "label": "World"}]
for line in regions:
    name, bbox, mz, label = line.split("|")
    sources.append({"name": name, "file": f"region-{name}.pmtiles", "maxzoom": int(mz),
                    "bbox": [float(x) for x in bbox.split(",")], "label": label})
json.dump({"build": build, "fetched_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
           "sources": sources}, open(f"{out}/manifest.json", "w"), indent=1)
PY
du -sh "$OUT"/*.pmtiles "$OUT/assets"
echo "done: $OUT/manifest.json"
