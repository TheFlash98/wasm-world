# WASM World

A rust environment to run wasm code (right now compiled from rust).

Each wasm instance is a node in a simulated network. The host (`wasmhost`, built
on wasmtime) delivers messages between them with random delays, and the nodes
(`wasminstance`) run [Raft](https://raft.github.io/raft.pdf) to keep a
replicated FIFO queue: leader election, log replication, and a client that
finds the leader and retries. You can kill and revive servers while it runs.

Creating the WASM build:

```bash
cd wasminstance
rustup target add wasm32-unknown-unknown
cargo build --release
```

Running the cluster (3 servers by default, or pass a number):

```bash
cd wasmhost
cargo run          # type commands, e.g. `kill leader`
cargo run -- 5     # 5 servers
cargo run < demo.txt
```

Commands:

| Command | What it does |
| --- | --- |
| `enq <n>` / `deq` | the client enqueues `n` / dequeues |
| `kill <id\|leader>` | crash a server; messages to it are lost |
| `revive <id\|all>` | restart crashed servers |
| `status` | every node's role, term, commit index, queue and log |
| `verbose on\|off` | also show every message and heartbeat |
| `delay <min> <max>` | network delay range in ms (default 10-150) |
| `sleep <ms>` | pause before the next command, for scripts |

`demo.txt` waits for an election, enqueues a few values, kills the leader,
and shows the cluster electing a new one and catching the old one up.

## How it fits together

- Servers are instances `1..=n`; instance `n + 1` is the client.
- The guest calls the host to log, send messages (`send_message`) and set
  timers (`set_timer`). The host calls the guest's `receive` and `on_timer`.
- Elections use randomized 1.5-3 s timeouts, and leaders send heartbeats
  every 300 ms. Raft needs network delays well below the election timeout,
  so raising `delay` towards a second or more causes extra elections.
- There's no disk. A server's term, vote and log stay in its wasm memory,
  which `kill` doesn't touch, so `revive` acts like a restart from disk. It
  replays the committed log to rebuild the queue.
- Clients tag requests with ids, so a retried request is only applied once.
- The host checks Raft's safety guarantees as it runs: at most one leader per
  term, and every node applying the same entry at each log index. `status`
  and `quit` print the result.

Unit tests for the Raft logic run natively:

```bash
cd wasminstance
cargo test --target $(rustc -vV | sed -n 's/host: //p')
```
