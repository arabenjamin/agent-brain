# llama.cpp OpenAI-compatible server, built from source against CUDA 12.2 to
# match this host's NVIDIA 535 / CUDA 12.2 driver.
#
# Why not the upstream prebuilt `ghcr.io/ggml-org/llama.cpp:server-cuda`? It ships
# CUDA 12.8 kernels. On the 535 driver that fails two ways: the bundled
# forward-compat stub aborts CUDA init on a GeForce ("forward compatibility was
# attempted on non supported HW"), and even with the stub removed the 12.8 cubins
# won't load ("CUDA error: device kernel image is invalid") because the 535
# driver's runtime is 12.2. Compiling our own kernels with CUDA 12.2 removes both.
#
# Targets sm_86 only (RTX 3060 Ti) for a smaller, faster build. If the GPU or
# host driver changes, bump CUDA_IMAGE / CMAKE_CUDA_ARCHITECTURES accordingly.
#
# The `-hf` model auto-download needs HTTPS: b10900's downloader uses a native
# TLS client (not just libcurl), so it requires LLAMA_OPENSSL=ON + libssl-dev at
# build and libssl3 at runtime — without it the server errors "HTTPS is not
# supported" and can only load an already-cached GGUF.

ARG CUDA_IMAGE=12.2.2
ARG LLAMA_TAG=b10900

FROM nvidia/cuda:${CUDA_IMAGE}-devel-ubuntu22.04 AS build
ARG LLAMA_TAG
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential cmake git ca-certificates libcurl4-openssl-dev libssl-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN git clone --depth 1 --branch ${LLAMA_TAG} \
      https://github.com/ggml-org/llama.cpp.git .
RUN cmake -B build \
      -DCMAKE_BUILD_TYPE=Release \
      -DGGML_CUDA=ON \
      -DCMAKE_CUDA_ARCHITECTURES=86 \
      -DLLAMA_CURL=ON \
      -DLLAMA_OPENSSL=ON \
      -DLLAMA_BUILD_SERVER=ON \
      -DCMAKE_EXE_LINKER_FLAGS=-Wl,--allow-shlib-undefined \
 && cmake --build build --config Release --target llama-server -j"$(nproc)"
# --allow-shlib-undefined: libggml-cuda.so references CUDA Driver API symbols
# (cuMemCreate, cuDeviceGet, ...) that live in libcuda.so, which the devel image
# keeps under lib64/stubs and does not link by default. Allowing the undefined
# symbols at exe-link time is how upstream's .devops/cuda.Dockerfile does it;
# they resolve at runtime when cudart loads the injected real libcuda.so.1.

FROM nvidia/cuda:${CUDA_IMAGE}-runtime-ubuntu22.04 AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
      libcurl4 libgomp1 libssl3 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
# The server binary plus the shared ggml/llama .so files it links against all
# land in build/bin; copy the lot and resolve them from /app.
COPY --from=build /src/build/bin/ /app/
ENV LD_LIBRARY_PATH=/app
WORKDIR /app
ENTRYPOINT ["/app/llama-server"]
