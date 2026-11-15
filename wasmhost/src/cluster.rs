// The simulated cluster: one wasm component instance per Raft server, a
// network that delays and drops messages, and the client, played by the host.
//
// Time is simulated. `advance_to` runs every tick and message delivery up to
// a given time, so tests run as fast as the nodes allow, and the same seed
// always gives the same run.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamResult};
use wasmtime_wasi::{Deterministic, HostMonotonicClock, HostWallClock, WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    world: "node",
    path: "../wit",
});

/// The id the client uses. Servers are 1..=num_servers.
pub const CLIENT_ID: i32 = 0;
const TICK_MS: u64 = 10;
const CLIENT_RETRY_MS: u64 = 2000;
const RECENT_LINES: usize = 400;

/// What each server's store holds. Calls from the guest land here, and the
/// cluster picks them up after every call into the guest.
struct NodeState {
    wasi: WasiCtx,
    table: ResourceTable,
    rng: StdRng,
    outbox: Vec<(i32, Vec<u8>)>,
    logs: Vec<String>,
    leader_terms: Vec<i32>,
    applied: Vec<(i32, i32)>,
}

impl WasiView for NodeState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl NodeImports for NodeState {
    fn send_message(&mut self, to: i32, data: Vec<u8>) {
        self.outbox.push((to, data));
    }

    fn log_str(&mut self, message: String) {
        self.logs.push(message);
    }

    fn random(&mut self, min: i32, max: i32) -> i32 {
        if min >= max { min } else { self.rng.gen_range(min..=max) }
    }

    fn report_leader(&mut self, term: i32) {
        self.leader_terms.push(term);
    }

    fn report_applied(&mut self, index: i32, term: i32) {
        self.applied.push((index, term));
    }
}

/// Collects a node's stdout and stderr, so `printf` output can be labelled
/// with the node it came from.
#[derive(Clone, Default)]
struct CapturedOutput(Arc<Mutex<Vec<u8>>>);

impl CapturedOutput {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

// Claiming a terminal keeps C's and Python's stdout line-buffered, so each
// line shows up right away instead of sitting in a buffer.
impl IsTerminal for CapturedOutput {
    fn is_terminal(&self) -> bool {
        true
    }
}

impl StdoutStream for CapturedOutput {
    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }

    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(tokio::io::sink())
    }
}

#[wasmtime_wasi::async_trait]
impl OutputStream for CapturedOutput {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.0.lock().unwrap().extend_from_slice(&bytes);
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(64 * 1024)
    }
}

#[wasmtime_wasi::async_trait]
impl Pollable for CapturedOutput {
    async fn ready(&mut self) {}
}

/// WASI clocks that read simulated time, so a node can't tell it apart
/// from `now-ms`.
#[derive(Clone)]
struct SimulatedClock(Arc<AtomicU64>);

impl HostWallClock for SimulatedClock {
    fn resolution(&self) -> Duration {
        Duration::from_millis(1)
    }

    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::Relaxed))
    }
}

impl HostMonotonicClock for SimulatedClock {
    fn resolution(&self) -> u64 {
        1_000_000
    }

    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed) * 1_000_000
    }
}

struct Server {
    store: Store<NodeState>,
    bindings: Node,
    output: CapturedOutput,
    // The start of a line the node hasn't finished printing yet.
    partial_line: Vec<u8>,
    alive: bool,
    // A trap leaves the instance unusable, so it can't be revived.
    crashed: bool,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Message {
    deliver_at: u64,
    // Breaks ties so runs are repeatable.
    seq: u64,
    from: i32,
    to: i32,
    data: Vec<u8>,
}

pub struct Network {
    pub min_delay: u64,
    pub max_delay: u64,
    pub loss_percent: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Enqueue(i32),
    Dequeue,
}

impl Op {
    fn request(&self, request_id: i32) -> String {
        match self {
            Op::Enqueue(value) => format!("ENQ {} {}", request_id, value),
            Op::Dequeue => format!("DEQ {}", request_id),
        }
    }
}

enum Reply {
    Ok(Option<i32>),
    Empty,
    NotLeader(i32),
}

impl Reply {
    fn parse(text: &str) -> Option<(i32, Reply)> {
        let number = |word: &str| word.parse::<i32>().ok();
        match text.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["OK", id] => Some((number(id)?, Reply::Ok(None))),
            ["OK", id, value] => Some((number(id)?, Reply::Ok(Some(number(value)?)))),
            ["EMPTY", id] => Some((number(id)?, Reply::Empty)),
            ["NOT_LEADER", id, leader] => Some((number(id)?, Reply::NotLeader(number(leader)?))),
            _ => None,
        }
    }
}

struct Pending {
    request_id: i32,
    op: Op,
    first_sent: u64,
    retry_at: u64,
}

/// The client, played by the host. It sends one request at a time, retries
/// with the same id until a server answers, and checks every answer against
/// its own copy of the queue.
struct Client {
    next_request_id: i32,
    target: i32,
    waiting: VecDeque<Op>,
    pending: Option<Pending>,
    model: VecDeque<i32>,
    finished: usize,
}

/// Keeps recent output lines, and prints them as they come if `echo` is on.
pub struct Log {
    pub echo: bool,
    pub verbose: bool,
    recent: VecDeque<String>,
}

impl Log {
    fn line(&mut self, now: u64, who: &str, message: &str) {
        let line = format!("[{:>8.3}s] {:<7}| {}", now as f64 / 1000.0, who, message);
        if self.echo {
            println!("{}", line);
        }
        self.recent.push_back(line);
        if self.recent.len() > RECENT_LINES {
            self.recent.pop_front();
        }
    }

    pub fn recent(&self, count: usize) -> impl Iterator<Item = &String> {
        self.recent.iter().skip(self.recent.len().saturating_sub(count))
    }
}

fn label(id: i32) -> String {
    if id == CLIENT_ID { "client".to_string() } else { format!("node {}", id) }
}

pub struct Cluster {
    num_servers: i32,
    servers: BTreeMap<i32, Server>,
    clock: Arc<AtomicU64>,
    next_tick: u64,
    messages: BinaryHeap<Reverse<Message>>,
    next_seq: u64,
    rng: StdRng,
    pub network: Network,
    client: Client,
    leaders: BTreeMap<i32, i32>, // term -> the node that reported leading it
    applied: BTreeMap<i32, (i32, i32)>, // log index -> (term, node) of the first report
    last_answered_by: Option<i32>,
    violations: Vec<String>,
    pub log: Log,
}

/// Compiles the module once, so tests can start many clusters from it.
pub struct Loaded {
    engine: Engine,
    component: Component,
    linker: Linker<NodeState>,
}

impl Loaded {
    pub fn new(path: &str) -> Result<Self> {
        let engine = Engine::default();
        let component = Component::from_file(&engine, path)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        Node::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)?;
        Ok(Loaded { engine, component, linker })
    }
}

impl Cluster {
    pub fn new(loaded: &Loaded, num_servers: i32, seed: u64, echo: bool) -> Result<Self> {
        let clock = Arc::new(AtomicU64::new(0));
        let mut rng = StdRng::seed_from_u64(seed);
        let mut servers = BTreeMap::new();
        for id in 1..=num_servers {
            let output = CapturedOutput::default();
            let random_bytes: Vec<u8> = (0..4096).map(|_| rng.gen()).collect();
            let wasi = WasiCtx::builder()
                .stdout(output.clone())
                .stderr(output.clone())
                .wall_clock(SimulatedClock(clock.clone()))
                .monotonic_clock(SimulatedClock(clock.clone()))
                .secure_random(Deterministic::new(random_bytes.clone()))
                .insecure_random(Deterministic::new(random_bytes))
                .insecure_random_seed(rng.gen())
                .build();
            let state = NodeState {
                wasi,
                table: ResourceTable::new(),
                rng: StdRng::seed_from_u64(rng.gen()),
                outbox: Vec::new(),
                logs: Vec::new(),
                leader_terms: Vec::new(),
                applied: Vec::new(),
            };
            let mut store = Store::new(&loaded.engine, state);
            let bindings = Node::instantiate(&mut store, &loaded.component, &loaded.linker)?;
            servers.insert(id, Server {
                store,
                bindings,
                output,
                partial_line: Vec::new(),
                alive: true,
                crashed: false,
            });
        }
        let mut cluster = Cluster {
            num_servers,
            servers,
            clock,
            next_tick: TICK_MS,
            messages: BinaryHeap::new(),
            next_seq: 0,
            rng,
            network: Network { min_delay: 10, max_delay: 150, loss_percent: 0 },
            client: Client {
                next_request_id: 1,
                target: 1,
                waiting: VecDeque::new(),
                pending: None,
                model: VecDeque::new(),
                finished: 0,
            },
            leaders: BTreeMap::new(),
            applied: BTreeMap::new(),
            last_answered_by: None,
            violations: Vec::new(),
            log: Log { echo, verbose: false, recent: VecDeque::new() },
        };
        for id in 1..=num_servers {
            cluster.call(id, |node, store| node.call_start(store, id, num_servers));
        }
        Ok(cluster)
    }

    pub fn now(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }

    pub fn num_servers(&self) -> i32 {
        self.num_servers
    }

    pub fn violations(&self) -> &[String] {
        &self.violations
    }

    pub fn finished(&self) -> usize {
        self.client.finished
    }

    /// True when every submitted request has been answered.
    pub fn client_idle(&self) -> bool {
        self.client.pending.is_none() && self.client.waiting.is_empty()
    }

    /// The oldest unanswered request and how long it has been waiting.
    pub fn oldest_pending(&self) -> Option<(i32, Op, u64)> {
        self.client
            .pending
            .as_ref()
            .map(|pending| (pending.request_id, pending.op, self.now() - pending.first_sent))
    }

    pub fn print(&mut self, who: &str, message: &str) {
        let now = self.now();
        self.log.line(now, who, message);
    }

    /// A number from the cluster's seeded generator, so scenarios that use
    /// randomness stay repeatable.
    pub fn random(&mut self, range: std::ops::Range<u64>) -> u64 {
        self.rng.gen_range(range)
    }

    pub fn submit(&mut self, op: Op) {
        self.client.waiting.push_back(op);
    }

    /// Runs the cluster until simulated time `target`.
    pub fn advance_to(&mut self, target: u64) {
        loop {
            let next_message = self.messages.peek().map(|Reverse(message)| message.deliver_at);
            let next = next_message.unwrap_or(u64::MAX).min(self.next_tick);
            if next > target {
                break;
            }
            self.clock.store(next, Ordering::Relaxed);
            if next_message == Some(next) {
                let Reverse(message) = self.messages.pop().unwrap();
                self.deliver(message);
            } else {
                self.next_tick += TICK_MS;
                for id in 1..=self.num_servers {
                    self.call(id, |node, store| node.call_tick(store, next as i32));
                }
                self.poll_client();
            }
        }
        self.clock.store(target.max(self.now()), Ordering::Relaxed);
    }

    pub fn run_for(&mut self, ms: u64) {
        self.advance_to(self.now() + ms);
    }

    /// Runs until `done` holds or `limit_ms` passes. Returns whether it held.
    pub fn run_until(&mut self, limit_ms: u64, done: impl Fn(&Cluster) -> bool) -> bool {
        let deadline = self.now() + limit_ms;
        while !done(self) {
            if self.now() >= deadline {
                return false;
            }
            self.run_for(TICK_MS);
        }
        true
    }

    pub fn is_alive(&self, id: i32) -> bool {
        self.servers.get(&id).is_some_and(|server| server.alive)
    }

    /// The live node that most recently reported winning an election, or
    /// else the last one to answer the client.
    pub fn leader(&self) -> Option<i32> {
        self.leaders
            .values()
            .rev()
            .copied()
            .find(|&id| self.is_alive(id))
            .or(self.last_answered_by.filter(|&id| self.is_alive(id)))
    }

    pub fn kill(&mut self, id: i32) -> Result<(), String> {
        let server = self.servers.get_mut(&id).ok_or(format!("{} is not a server", label(id)))?;
        if !server.alive {
            return Err(format!("node {} is already down", id));
        }
        server.alive = false;
        self.print("host", &format!("node {} killed", id));
        Ok(())
    }

    pub fn revive(&mut self, id: i32) -> Result<(), String> {
        let server = self.servers.get_mut(&id).ok_or(format!("{} is not a server", label(id)))?;
        if server.crashed {
            return Err(format!("node {} crashed and can't be revived", id));
        }
        if server.alive {
            return Err(format!("node {} is already running", id));
        }
        server.alive = true;
        self.print("host", &format!("node {} revived", id));
        let now = self.now() as i32;
        self.call(id, |node, store| node.call_restart(store, now));
        Ok(())
    }

    pub fn down_servers(&self) -> Vec<i32> {
        self.servers.iter().filter(|(_, server)| !server.alive).map(|(&id, _)| id).collect()
    }

    pub fn status(&mut self) {
        for id in 1..=self.num_servers {
            if self.is_alive(id) {
                self.call(id, |node, store| node.call_status(store));
            } else {
                self.print(&label(id), "DOWN");
            }
        }
        let client = match self.oldest_pending() {
            Some((request_id, op, waited)) => {
                format!("waiting {} ms on request #{} {:?}", waited, request_id, op)
            }
            None => "idle".to_string(),
        };
        let queued = self.client.waiting.len();
        self.print("client", &format!("{}, {} more queued, model queue {:?}", client, queued, self.client.model));
        let summary = self.summary();
        self.print("host", &summary);
    }

    pub fn summary(&self) -> String {
        format!(
            "network {}-{} ms, {}% loss; {} requests answered, {} terms had a leader, {} violations",
            self.network.min_delay,
            self.network.max_delay,
            self.network.loss_percent,
            self.client.finished,
            self.leaders.len(),
            self.violations.len(),
        )
    }

    fn violation(&mut self, problem: String) {
        self.print("CHECK", &format!("VIOLATION: {}", problem));
        self.violations.push(problem);
    }

    /// Calls into a live node, then handles what it did: messages, log lines,
    /// printed output and leader reports. A trap counts as a crash.
    fn call(&mut self, id: i32, f: impl FnOnce(&Node, &mut Store<NodeState>) -> wasmtime::Result<()>) {
        let Some(server) = self.servers.get_mut(&id) else { return };
        if !server.alive {
            return;
        }
        let result = f(&server.bindings, &mut server.store);
        let state = server.store.data_mut();
        let outbox = std::mem::take(&mut state.outbox);
        let logs = std::mem::take(&mut state.logs);
        let leader_terms = std::mem::take(&mut state.leader_terms);
        let applied = std::mem::take(&mut state.applied);
        server.partial_line.extend(server.output.take());
        let mut printed = Vec::new();
        while let Some(end) = server.partial_line.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = server.partial_line.drain(..=end).collect();
            printed.push(String::from_utf8_lossy(&line).trim_end().to_string());
        }
        if let Err(error) = &result {
            server.alive = false;
            server.crashed = true;
            printed.extend(format!("{:#}", error).lines().map(str::to_string));
        }

        let who = label(id);
        for line in logs.iter().chain(&printed) {
            self.print(&who, line);
        }
        if result.is_err() {
            self.violation(format!("node {} crashed (trapped); it stays down", id));
        }
        for term in leader_terms {
            let first = *self.leaders.entry(term).or_insert(id);
            if first != id {
                self.violation(format!("two leaders in term {}: nodes {} and {}", term, first, id));
            }
        }
        for (index, term) in applied {
            let (first_term, first_node) = *self.applied.entry(index).or_insert((term, id));
            if first_term != term {
                self.violation(format!(
                    "node {} applied an entry from term {} at index {}, but node {} applied one from term {}",
                    id, term, index, first_node, first_term,
                ));
            }
        }
        for (to, data) in outbox {
            self.send(id, to, data);
        }
    }

    /// Puts a message on the network. It may be lost, and arrives after a
    /// random delay unless the receiver is down by then.
    fn send(&mut self, from: i32, to: i32, data: Vec<u8>) {
        if to != CLIENT_ID && !self.servers.contains_key(&to) {
            self.print(&label(from), &format!("sent a message to node {}, which doesn't exist", to));
            return;
        }
        let lost = self.rng.gen_range(0..100) < self.network.loss_percent;
        let delay = self.rng.gen_range(self.network.min_delay..=self.network.max_delay);
        if self.log.verbose {
            let fate = if lost { "lost".to_string() } else { format!("in {} ms", delay) };
            let route = format!("{} -> {} {}: {}", label(from), label(to), fate, String::from_utf8_lossy(&data));
            self.print("net", &route);
        }
        if lost {
            return;
        }
        let message = Message { deliver_at: self.now() + delay, seq: self.next_seq, from, to, data };
        self.next_seq += 1;
        self.messages.push(Reverse(message));
    }

    fn deliver(&mut self, message: Message) {
        if message.to == CLIENT_ID {
            self.client_receive(message.from, &message.data);
        } else {
            let Message { from, to, data, .. } = message;
            self.call(to, |node, store| node.call_receive(store, from, &data));
        }
    }

    // The client

    fn poll_client(&mut self) {
        let now = self.now();
        match &self.client.pending {
            None => {
                let Some(op) = self.client.waiting.pop_front() else { return };
                let request_id = self.client.next_request_id;
                self.client.next_request_id += 1;
                self.client.pending = Some(Pending { request_id, op, first_sent: now, retry_at: now });
                self.print("client", &format!("request #{}: {}", request_id, op.request(request_id)));
                self.send_request();
            }
            Some(pending) if now >= pending.retry_at => {
                let silent = self.client.target;
                self.client.target = self.next_server(silent);
                let message = format!("no answer from node {}, retrying with node {}", silent, self.client.target);
                self.print("client", &message);
                self.send_request();
            }
            Some(_) => {}
        }
    }

    fn send_request(&mut self) {
        let retry_at = self.now() + CLIENT_RETRY_MS;
        let Some(pending) = self.client.pending.as_mut() else { return };
        pending.retry_at = retry_at;
        let request = pending.op.request(pending.request_id);
        self.send(CLIENT_ID, self.client.target, request.into_bytes());
    }

    fn next_server(&self, id: i32) -> i32 {
        id % self.num_servers + 1
    }

    fn client_receive(&mut self, from: i32, data: &[u8]) {
        let text = String::from_utf8_lossy(data).to_string();
        let Some((request_id, reply)) = Reply::parse(&text) else {
            self.violation(format!("node {} sent the client a reply it can't read: {:?}", from, text));
            return;
        };
        let Some(pending) = &self.client.pending else { return };
        if pending.request_id != request_id {
            return; // a late answer to a request that's already done
        }
        let (op, first_sent) = (pending.op, pending.first_sent);
        match reply {
            Reply::NotLeader(leader) => {
                if (1..=self.num_servers).contains(&leader) && leader != from {
                    self.client.target = leader;
                    self.send_request();
                } else {
                    // No leader known yet: try the next server when the retry timer fires.
                    self.client.target = self.next_server(from);
                }
            }
            reply => {
                if let Err(problem) = self.check_against_model(op, &reply) {
                    self.violation(format!("request #{} ({}): {}", request_id, op.request(request_id), problem));
                }
                let took = self.now() - first_sent;
                self.print("client", &format!("request #{} answered by node {} in {} ms: {}", request_id, from, took, text));
                self.client.finished += 1;
                self.client.pending = None;
                self.client.target = from;
                self.last_answered_by = Some(from);
            }
        }
    }

    /// Every request must be applied exactly once, in order. So each answer
    /// must match what the host's own copy of the queue gives.
    fn check_against_model(&mut self, op: Op, reply: &Reply) -> Result<(), String> {
        let model = &mut self.client.model;
        match (op, reply) {
            (Op::Enqueue(value), Reply::Ok(None)) => {
                model.push_back(value);
                Ok(())
            }
            (Op::Dequeue, Reply::Ok(Some(value))) => match model.front() {
                Some(front) if front == value => {
                    model.pop_front();
                    Ok(())
                }
                front => Err(format!("dequeued {}, but the front of the queue is {:?}", value, front)),
            },
            (Op::Dequeue, Reply::Empty) if model.is_empty() => Ok(()),
            (Op::Dequeue, Reply::Empty) => Err(format!("said EMPTY, but the queue holds {:?}", model)),
            (Op::Enqueue(_), _) => Err("an enqueue must be answered with `OK <id>`".to_string()),
            (Op::Dequeue, _) => Err("a dequeue must be answered with `OK <id> <value>` or `EMPTY <id>`".to_string()),
        }
    }
}
