#!/bin/sh
# Builds raft.wasm from app.py, a component the host can run:
#   wasmhost test samples/python/raft.wasm
#
# Needs componentize-py: pip install -r requirements.txt
#
# For editor autocomplete on `wit_world`, generate the bindings next to app.py:
#   componentize-py -d ../../wit -w node bindings .
set -e
cd "$(dirname "$0")"
componentize-py -d ../../wit -w node componentize app -o raft.wasm
