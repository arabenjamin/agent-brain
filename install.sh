#!/usr/bin/env bash
# Install the brain: choose the local engine, write .env, fetch the models, start it.
#
#   ./install.sh                                  # llama.cpp, GPU if one is found
#   ./install.sh --gpu cpu                        # llama.cpp with no GPU
#   ./install.sh --engine openai --url URL --model NAME --embed-model NAME
#                [--capabilities chat,tools,json_schema,embeddings]
#                                                 # your own OpenAI-wire server
#   --preset NAME   a file in llamacpp/presets/ (default: chosen from the card)
#   --no-fetch      keep the models already in the volume
#   --no-start      stop after .env and the models
#
# Safe to run again: existing .env values and secrets are kept, except the engine
# settings it is asked to change, and .env is backed up first. Never prints a secret.
set -euo pipefail
cd "$(dirname "$0")"

ENGINE=llamacpp GPU="" PRESET="" URL="" MODEL="" EMBED="" CAPS="" FETCH=1 START=1
die() { echo "install: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
    case "$1" in
        --engine) ENGINE="$2"; shift 2 ;;
        --gpu) GPU="$2"; shift 2 ;;
        --preset) PRESET="$2"; shift 2 ;;
        --url) URL="$2"; shift 2 ;;
        --model) MODEL="$2"; shift 2 ;;
        --embed-model) EMBED="$2"; shift 2 ;;
        --capabilities) CAPS="$2"; shift 2 ;;
        --no-fetch) FETCH=0; shift ;;
        --no-start) START=0; shift ;;
        -h|--help) sed -n '2,14p' "$0" | cut -c3-; exit 0 ;;
        *) die "unknown option $1 (--help lists them)" ;;
    esac
done

command -v docker >/dev/null || die "docker is not installed"
compose_version=$(docker compose version --short 2>/dev/null) || die "docker compose v2 is not installed"
case "$(printf '%s\n2.24.4\n' "$compose_version" | sort -V | head -1)" in
    2.24.4) ;;
    *) die "docker compose $compose_version is too old; need 2.24.4 or later" ;;
esac

# ── .env: only what is asked for changes ──────────────────────────────────────
[ -e .env ] || { touch .env; chmod 600 .env; }
cp -p .env ".env.bak.$(date +%Y%m%d%H%M%S)"
has_key() { grep -q "^$1=" .env; }
set_key() { # key value: replace the line, or add it
    local tmp; tmp=$(mktemp .env.XXXXXX); chmod 600 "$tmp"
    grep -v "^$1=" .env > "$tmp" || true
    printf '%s=%s\n' "$1" "$2" >> "$tmp"
    [ -s "$tmp" ] && mv "$tmp" .env
}
default_key() { has_key "$1" || set_key "$1" "$2"; }
secret() { head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'; }

default_key NEO4J_PASSWORD "$(secret)"
default_key MCP_API_KEY "$(secret)"
if [ -r /etc/timezone ]; then default_key TZ "$(cat /etc/timezone)"; fi

# The brain's container runs as uid 1000 and writes to telemetry/ and workspace/.
[ "$(id -u)" = 1000 ] || echo "note: the container runs as uid 1000 and you are $(id -u); give it write access to telemetry/ and workspace/"

# Host folders compose mounts. Docker would create a missing one as root.
mkdir -p telemetry workspace/no-extra-code "${VAULT_HOST_DIR:-$HOME/.config/agent-brain/vault}"
if [ ! -d ../agent-brain-personal ]; then
    mkdir -p workspace/no-private-bundle
    default_key PLUGINS_PRIVATE_HOST_DIR ./workspace/no-private-bundle
fi

# Compose declares this network external, so another stack can share it.
docker network inspect ai-network >/dev/null 2>&1 || docker network create ai-network >/dev/null

# ── The engine ────────────────────────────────────────────────────────────────
files=docker-compose.yml
case "$ENGINE" in
    llamacpp)
        if [ -z "$GPU" ]; then
            if command -v nvidia-smi >/dev/null && nvidia-smi >/dev/null 2>&1; then GPU=cuda; else GPU=cpu; fi
        fi
        case "$GPU" in
            cuda)
                command -v nvidia-smi >/dev/null || die "--gpu cuda needs nvidia-smi"
                # Into a variable: under pipefail, `docker info | grep -q` fails at random
                # when grep exits first and docker dies on the closed pipe.
                info=$(docker info 2>/dev/null || true)
                case "$info" in *nvidia*|*Nvidia*|*NVIDIA*) ;; *) false ;; esac \
                    || die "docker has no nvidia runtime (install nvidia-container-toolkit), or use --gpu cpu"
                mib=$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits | head -1)
                cap=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1 | tr -d '. ')
                cuda=$(nvidia-smi | sed -n 's/.*CUDA Version: *\([0-9.]*\).*/\1/p' | head -1)
                [ "$mib" -ge 6000 ] || die "this card has $mib MiB; the models need about 5.5 GB. Use --gpu cpu"
                image=""
                for pair in 12.8:12.8.1 12.6:12.6.2 12.4:12.4.1 12.2:12.2.2; do
                    want=${pair%%:*}
                    if [ "$(printf '%s\n%s\n' "$want" "$cuda" | sort -V | head -1)" = "$want" ]; then
                        image=${pair##*:}; break
                    fi
                done
                [ -n "$image" ] || die "driver supports CUDA $cuda; need 12.2 or later, or use --gpu cpu"
                [ "$image" = 12.2.2 ] && [ "$cap" = 86 ] \
                    || echo "note: CUDA $image on compute_cap $cap has not been run; 12.2.2 on 86 has"
                set_key LLAMA_BUILD_IMAGE "nvidia/cuda:$image-devel-ubuntu22.04"
                set_key LLAMA_RUN_IMAGE "nvidia/cuda:$image-runtime-ubuntu22.04"
                set_key LLAMA_GGML_CUDA ON
                set_key LLAMA_CUDA_ARCH "$cap"
                if [ -z "$PRESET" ]; then
                    if [ "$mib" -ge 24000 ]; then PRESET=gpu-24gb; else PRESET=gpu-8gb; fi
                fi
                echo "engine: llama.cpp, $mib MiB card, preset $PRESET"
                ;;
            cpu)
                set_key LLAMA_BUILD_IMAGE ubuntu:22.04
                set_key LLAMA_RUN_IMAGE ubuntu:22.04
                set_key LLAMA_GGML_CUDA OFF
                set_key LLAMA_CUDA_ARCH ""
                PRESET=${PRESET:-cpu}
                files="$files:docker-compose.cpu.yml"
                echo "engine: llama.cpp, no GPU, preset $PRESET. It works, but slowly"
                ;;
            *) die "--gpu is cuda or cpu" ;;
        esac
        [ -f "llamacpp/presets/$PRESET.ini" ] || die "no preset llamacpp/presets/$PRESET.ini"
        set_key LOCAL_ENGINE_PRESET "$PRESET"
        set_key LOCAL_ENGINE_URL http://llamacpp:8080
        ;;
    openai)
        [ -n "$URL" ] && [ -n "$MODEL" ] && [ -n "$EMBED" ] \
            || die "--engine openai needs --url, --model and --embed-model"
        set_key LOCAL_ENGINE_URL "$URL"
        set_key LOCAL_MODEL "$MODEL"
        set_key LOCAL_EMBED_MODEL "$EMBED"
        [ -z "$CAPS" ] || set_key LOCAL_ENGINE_CAPABILITIES "$CAPS"
        files="$files:docker-compose.byo.yml"
        FETCH=0
        echo "engine: your server at $URL"
        ;;
    *) die "--engine is llamacpp or openai" ;;
esac
set_key LOCAL_ENGINE "$ENGINE"
set_key LOCAL_ENGINE_INSTALLED "$ENGINE"
set_key COMPOSE_FILE "$files"
echo ".env written (backup beside it)"

# ── Models, then start ────────────────────────────────────────────────────────
if [ "$FETCH" = 1 ]; then
    need=$(awk '!/^#/ && NF {s+=$3} END {printf "%d", s/1073741824 + 2}' llamacpp/models.manifest)
    echo "fetching the models, $need GB at most; run again to resume"
    docker compose --profile install run --rm models-fetch
fi
[ "$START" = 1 ] || { echo "not started: docker compose up -d --build"; exit 0; }

docker compose up -d --build
echo "waiting for the brain..."
port=$(docker compose port agent-brain 3000 2>/dev/null | sed 's/.*://')
for _ in $(seq 1 90); do
    if curl -fsS -m 3 "http://127.0.0.1:${port:-3001}/health" 2>/dev/null; then
        echo
        echo "up. HBI: http://127.0.0.1:3002  MCP: http://127.0.0.1:${port:-3001}/mcp (key: MCP_API_KEY in .env)"
        exit 0
    fi
    sleep 5
done
die "the brain did not answer; docker compose logs agent-brain"
