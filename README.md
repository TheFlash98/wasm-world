# WASM World

A sandbox for testing [Raft](https://raft.github.io/raft.pdf) implementations
written in any language that compiles to WebAssembly.

Each Raft server is a wasm component that implements
[`wit/node.wit`](wit/node.wit). The host (`wasmhost`, built on wasmtime)
does four things:

- runs one instance per server;
- carries messages between them over a simulated network, with random
  delays, loss and crashes;
- plays the client of a replicated FIFO queue;
- checks every answer.

**Students: start with [STUDENTS.md](STUDENTS.md).**

## Layout

| Path | What |
| --- | --- |
| `wit/node.wit` | the interface every server implements |
| `wasmhost/` | the host: interactive runs and `wasmhost test` |
| `wasminstance/` | reference solution in Rust |
| `samples/cpp/` | sample solution in C++ |

## Build and run

```bash
rustup target add wasm32-wasip2
cd wasminstance && cargo build --release && cd ..

cd wasmhost
cargo run --release                     # 3 servers; type `help` for commands
cargo run --release -- --servers 5      # 5 servers
cargo run --release < demo.txt          # scripted leader failover
cargo run --release -- test             # the test scenarios
cargo run --release -- test ../samples/cpp/raft.wasm
```

With no file given, the host runs the Rust reference solution.

## How the host checks a server

- **Model check:** the client sends one request at a time. It retries with
  the same id until a server answers, and checks every answer against its
  own copy of the queue. A lost write, a retry applied twice, or a stale
  read shows up as a wrong answer.
- **Safety checks:** these use the optional `report-leader` and
  `report-applied` calls. At most one leader per term, and every server
  applies the same entry at each log index.
- **Simulated time:** the host drives every tick and message delivery.
  Tests run in well under a second, and a seed always reproduces the same
  run.

The Rust reference has unit tests that run natively:

```bash
cd wasminstance
cargo test --target $(rustc -vV | sed -n 's/host: //p')
```
