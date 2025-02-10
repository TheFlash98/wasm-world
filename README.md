# WASM World

A rust environment to run wasm code (right now compiled from rust).

Creating the WASM build:

```bash
cd wasm-env
rustup target add wasm32-unknown-unknown
cargo build --target=wasm32-unknown-unknown --release
```

Interacting with the WASM:

```
cd wasmtime_test
cargo run
```