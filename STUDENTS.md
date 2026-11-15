# Writing a Raft server for wasm-world

You'll build one server of a [Raft](https://raft.github.io/raft.pdf) cluster
that keeps a replicated FIFO queue. You can use any language that compiles to
a WebAssembly component. The host runs several copies of your server, carries
messages between them over a simulated network, plays the client, and checks
your answers.

There are working samples to read: Rust in `wasminstance/` and C++ in
`samples/cpp/`.

## The interface

Your server implements [`wit/node.wit`](wit/node.wit). Your language's
tooling generates bindings from that file (see [Building](#building)).

Servers have ids `1..cluster-size`. **Id 0 is the client.**

**You export these. The host calls them.**

| Function | When |
| --- | --- |
| `start(id, cluster-size)` | once, first |
| `receive(sender, data)` | a message arrived; `sender` 0 means the client |
| `tick(now-ms)` | every 10 ms; `now-ms` is your only clock |

**You can call these.**

| Function | What it does |
| --- | --- |
| `send-message(to, data)` | send bytes to server `to`, or to 0 to answer the client |
| `log-str(message)` | print a line labelled with your id (`printf` and `print` work too) |
| `random(min, max)` | a random number from `min` to `max`, inclusive |
| `report-leader(term)` | optional: call it when you become leader |
| `report-applied(index, term)` | optional: call it each time you apply a log entry |

**Call both `report-` functions.** They let the host check two of Raft's
safety properties: one leader per term, and every server applying the same
entry at each index. Without them, many bugs go unnoticed.

Messages between your servers can use **any format**: the host never reads
them. The samples show two choices: the C++ one sends text such as
`VOTE 3 2 7 2`, and the Rust one uses a Rust serialization format.

## The client protocol

The client (id 0) sends plain text, one request per message:

- `ENQ <request_id> <value>`
- `DEQ <request_id>`

Answer it with `send-message(0, ...)`:

| Reply | Meaning |
| --- | --- |
| `OK <request_id>` | the enqueue is committed |
| `OK <request_id> <value>` | the dequeue is committed; here's the value |
| `EMPTY <request_id>` | the dequeue is committed, but the queue was empty |
| `NOT_LEADER <request_id> <leader_id>` | try this server instead (0 if unknown) |

Only the leader answers `OK` or `EMPTY`, and only after the entry is
committed and applied.

The client sends one request at a time and numbers them 1, 2, 3, and so on.
If it gets no answer within 2 s, it **sends the same request again**, maybe to
another server. A request must change the queue only once, however many times
it arrives. Raft §8 explains how. Keeping the last request id you applied, and
its reply, is enough.

## The environment

- **Network:** messages take 10–150 ms. They can arrive out of order, and
  messages to a server that's down are lost. Some tests stretch delays to
  400 ms and lose 10% of messages.
- **Timeouts:** delays are much longer than in the paper, so scale up its
  timeouts. A heartbeat every 300 ms with election timeouts of 1.5–3 s works.
  Pick each election timeout with `random`.
- **Time:** use `now-ms` from `tick`. It's simulated: tests run much faster
  than real time, and the same seed always gives the same run.

## Killed servers

`kill` freezes a server. It gets no ticks, and messages sent to it are lost.
Its memory is kept, so there's no disk to deal with. When it's revived, it
carries on where it stopped, with `now-ms` jumped ahead.

That's just like a server that was cut off from the network for a while.
Raft handles it: the server's timeout has usually passed, so it starts an
election at once. It loses that election if its log is behind.

A server that traps (crashes, panics, aborts) stays down and fails the test.

## Building

**Rust:** use [`wit-bindgen`](https://crates.io/crates/wit-bindgen) and the
`wasm32-wasip2` target. See `wasminstance/`.

```bash
rustup target add wasm32-wasip2
cd wasminstance && cargo build --release
# -> target/wasm32-wasip2/release/wasminstance.wasm
```

**C or C++:** use [wasi-sdk](https://github.com/WebAssembly/wasi-sdk/releases)
22 or newer. `samples/cpp/bindings/` already holds the generated C bindings.

```bash
export WASI_SDK_PATH=/path/to/wasi-sdk
samples/cpp/build.sh      # -> samples/cpp/raft.wasm
```

In C and C++, `receive` hands you ownership of `data`: free it with
`node_list_u8_free`.

**Other languages:** anything that can build a component for `wit/node.wit`
should work, such as Go via TinyGo or JavaScript via jco. Python can build
one with `componentize-py`, but there's no sample or test for it yet.

## Testing

```bash
cd wasmhost
cargo run --release -- test path/to/your.wasm
```

Each scenario starts a fresh cluster:

1. elects a leader and answers a request
2. replicates many requests in order
3. survives the leader crashing
4. keeps working with a minority down
5. stalls without a majority, then recovers
6. copes with slow, reordered messages
7. copes with lost messages
8. repairs a stale old leader when it rejoins
9. won't elect a server that missed writes
10. stays correct through random crashes and lost messages

A scenario fails if any of these happen:

- a request goes unanswered too long;
- an answer doesn't match what a correct queue would say;
- a safety check fails;
- a server crashes.

A failure shows the reason, your last log lines, and the command to rerun
exactly that run. Useful options:

- `--seed S`: try different random runs. Try several before you hand in.
- `--servers 5`: use five servers instead of three.

## Watching it run

```bash
cargo run --release -- path/to/your.wasm
```

Then type commands: `enq 5`, `deq`, `kill leader`, `kill 2`, `revive all`,
`status`, `verbose on` (shows every message), `delay 50 400`, `loss 10`.
You can also pipe in a script: see `wasmhost/demo.txt`.
