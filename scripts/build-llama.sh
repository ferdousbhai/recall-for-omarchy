#!/bin/bash
# Builds llama.cpp (MIT), the embedding runtime: the pinned build tag plus scripts/llama.cpp-no-logits.patch, as shared
# libraries in Vendor/llama/lib with the headers in Vendor/llama/include (all out of git). Each GPU backend is a plugin
# loaded at runtime (GGML_BACKEND_DL): CUDA when nvcc is there, Vulkan when its headers, glslc and the SPIR-V headers
# are, and the CPU in variants for every x86 level. The embedder then picks CUDA, else Vulkan, else the CPU, on whatever
# machine it runs on. CMake comes from PATH, or from a venv in Vendor/ (needs uv). Does nothing when that build is
# already there. RECALL_VENDOR puts all of it in another folder (package builds keep it across clean checkouts); point
# RECALL_LLAMA_DIR at its llama/ for cargo.
set -euo pipefail
cd "$(dirname "$0")/.."
TAG=b11461   # the first tags with EmbeddingGemma 2 (text + vision + audio) are b11452+
# A new TAG ships only if `cargo test --release` passes: tests/reference_vectors.rs checks it makes the same vectors, so
# every index is kept. If it fails, keep this TAG, or bump VECTOR_VERSION in src/llama.rs and write the reference vectors
# again (RECALL_WRITE_REFERENCE=1 cargo test --release --test reference_vectors): every index then embeds again.
PATCH=$PWD/scripts/llama.cpp-no-logits.patch
VENDOR=${RECALL_VENDOR:-Vendor}
SOURCE=$VENDOR/llama.cpp
OUT=$VENDOR/llama
BACKENDS="cpu"
if command -v nvcc > /dev/null; then BACKENDS+=" cuda"; fi
if pkg-config --exists vulkan 2> /dev/null && command -v glslc > /dev/null \
   && [[ -f /usr/include/spirv/unified1/spirv.hpp ]]; then   # libvulkan-dev, glslc, spirv-headers
  BACKENDS+=" vulkan"
fi
if [[ ${RECALL_BUILD_VULKAN:-auto} == 1 && "$BACKENDS" != *vulkan* ]]; then
  echo 'Vulkan requested but build dependencies are missing (Vulkan headers/library, glslc, SPIR-V headers).' >&2
  exit 1
fi
if [[ ${RECALL_BUILD_VULKAN:-auto} == 0 ]]; then BACKENDS=${BACKENDS/ vulkan/}; fi
STAMP="$TAG $(sha256sum "$PATCH" | cut -c1-12) $BACKENDS"
BUILT="$OUT/lib/libllama.so"
if [[ -f "$BUILT" && "$(cat "$OUT/BUILD" 2>/dev/null)" == "$STAMP" ]]; then
  exit 0
fi

if [[ ! -d "$SOURCE" ]]; then
  git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$TAG" https://github.com/ggml-org/llama.cpp "$SOURCE"
elif [[ "$(git -C "$SOURCE" describe --tags --exact-match 2>/dev/null)" != "$TAG" ]]; then
  git -C "$SOURCE" fetch --quiet --depth 1 origin tag "$TAG"
  git -C "$SOURCE" checkout --quiet --force "$TAG"   # --force also drops the old patch
fi
# Without the patch, every input token costs 1 MiB of logits that an embedding model never produces (262k vocabulary).
if git -C "$SOURCE" apply --check "$PATCH" 2>/dev/null; then
  git -C "$SOURCE" apply "$PATCH"
fi

if ! command -v cmake > /dev/null; then
  if [[ ! -x $VENDOR/cmake-venv/bin/cmake ]]; then
    uv venv --quiet "$VENDOR/cmake-venv" --python 3.12
    uv pip install --quiet --python "$VENDOR/cmake-venv/bin/python" cmake ninja
  fi
  export PATH="$(realpath "$VENDOR/cmake-venv/bin"):$PATH"
fi
GENERATOR=$(command -v ninja > /dev/null && echo Ninja || echo "Unix Makefiles")
# Only the libraries: no tools, no server, nothing downloaded at build time.
COMMON=(-DCMAKE_BUILD_TYPE=Release -DGGML_NATIVE=OFF -DLLAMA_BUILD_MTMD=ON -DLLAMA_BUILD_COMMON=OFF
  -DLLAMA_BUILD_TOOLS=OFF -DLLAMA_BUILD_SERVER=OFF -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_TESTS=OFF
  -DLLAMA_BUILD_APP=OFF -DLLAMA_BUILD_UI=OFF -DLLAMA_OPENSSL=OFF)
B=$VENDOR/llama-build
mkdir -p "$OUT/lib" "$OUT/include"
CUDA=$([[ "$BACKENDS" == *cuda* ]] && echo ON || echo OFF)
VULKAN=$([[ "$BACKENDS" == *vulkan* ]] && echo ON || echo OFF)
# $ORIGIN: the libraries find each other wherever they're copied, next to the binary in a package too.
cmake -S "$SOURCE" -B "$B" -G "$GENERATOR" "${COMMON[@]}" -DBUILD_SHARED_LIBS=ON -DGGML_BACKEND_DL=ON \
  -DGGML_CPU_ALL_VARIANTS=ON -DGGML_CUDA="$CUDA" -DGGML_VULKAN="$VULKAN" -DGGML_BLAS=OFF \
  -DCMAKE_INSTALL_RPATH='$ORIGIN' -DCMAKE_BUILD_WITH_INSTALL_RPATH=ON > /dev/null
if ! cmake --build "$B" --config Release -j "${RECALL_BUILD_JOBS:-$(nproc)}" > "$B/build.log" 2>&1; then
  tail -n 120 "$B/build.log" >&2
  exit 1
fi
rm -f "$OUT"/lib/*.so*
find "$B/bin" "$B/src" "$B/ggml" "$B/tools/mtmd" -name '*.so*' \( -type f -o -type l \) -exec cp -P {} "$OUT/lib/" \;
cp "$SOURCE/include/llama.h" "$SOURCE"/ggml/include/*.h "$SOURCE/tools/mtmd/mtmd.h" "$SOURCE/tools/mtmd/mtmd-helper.h" \
  "$OUT/include/"
# The build, for logs and `recall status` (indexes record VECTOR_VERSION in src/llama.rs, not this). build.rs reads it.
printf '// Written by scripts/build-llama.sh.\n#define LLAMA_BUILD_TAG "%s"\n#define LLAMA_LIB_DIR "%s"\n' "$TAG" \
  "$(realpath "$OUT/lib")" > "$OUT/include/llama-build.h"
echo "$STAMP" > "$OUT/BUILD"
echo "Built llama.cpp $TAG (patched) into $OUT"
