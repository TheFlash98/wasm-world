#!/bin/sh
# Builds raft.wasm, a component the host can run:
#   wasmhost test samples/cpp/raft.wasm
#
# Needs wasi-sdk 22 or newer: https://github.com/WebAssembly/wasi-sdk/releases
# Set WASI_SDK_PATH to the folder you unpacked it to.
#
# bindings/ is generated from wit/node.wit. If that file changes, regenerate
# it with wit-bindgen (https://github.com/bytecodealliance/wit-bindgen):
#   wit-bindgen c ../../wit --world node --out-dir bindings
set -e
cd "$(dirname "$0")"
: "${WASI_SDK_PATH:?set WASI_SDK_PATH to the folder you unpacked wasi-sdk to}"

"$WASI_SDK_PATH/bin/clang" --target=wasm32-wasip2 -O2 -c bindings/node.c -o node.o
"$WASI_SDK_PATH/bin/clang++" --target=wasm32-wasip2 -mexec-model=reactor -O2 -std=c++17 -fno-exceptions \
    raft.cpp node.o bindings/node_component_type.o -o raft.wasm
rm node.o
echo "built $(pwd)/raft.wasm"
