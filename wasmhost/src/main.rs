use anyhow::{anyhow, Context, Result};
use wasmtime::*;
use std::str;
use std::sync::{Arc, Mutex};
use std::thread;
use serde::{Serialize, Deserialize};
use std::collections::{HashMap, BinaryHeap};
use std::io::BufRead;
use std::sync::mpsc::channel;
use std::sync::mpsc::{Sender, Receiver};
use rand::Rng;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::cmp::{Reverse, Ord, Ordering};

const WASM_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../wasminstance/target/wasm32-unknown-unknown/release/wasminstance.wasm"
);
const DEFAULT_SERVERS: i32 = 3;
const LOG_DEBUG: i32 = 1;

const HELP: &str = "\
commands:
  enq <n>             the client enqueues n
  deq                 the client dequeues
  kill <id|leader>    crash a server; messages to it are lost
  revive <id|all>     restart crashed servers (they keep their term, vote and log)
  status              show every node's Raft state
  verbose on|off      also show every message, heartbeat and debug line
  delay <min> <max>   set the network delay range in ms
  sleep <ms>          wait before reading the next command (for scripts)
  help, quit";


#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag="event_type")]
pub enum EventData {
    Timer {
        token: i32
    },
    RawMessage {
        message: String
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Event {
    pub fire_time: u128,
    pub sender_id: i32,
    pub data: EventData,
}

impl Event {
    pub fn new(fire_time: u128, sender_id: i32, data: EventData) -> Self {
        Self { fire_time, sender_id, data }
    }
}

impl PartialEq for Event {
    fn eq(&self, other: &Self) -> bool {
        self.fire_time == other.fire_time
    }
}

impl Eq for Event {}

impl PartialOrd for Event {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Event {
    fn cmp(&self, other: &Self) -> Ordering {
        self.fire_time.cmp(&other.fire_time)
    }
}

#[derive(Default)]
pub struct DevilCat {
    pub min_delay: i32,
    pub max_delay: i32,
}

impl DevilCat {
    pub fn new(min_delay: i32, max_delay: i32) -> Self {
        Self { min_delay, max_delay }
    }

    pub fn get_random_delay(&self) -> u128 {
        let mut rng = rand::thread_rng();
        let delay = rng.gen_range(self.min_delay..=self.max_delay);
        delay as u128
    }
}

/// Checks two of Raft's guarantees as nodes report them: at most one leader
/// per term (Election Safety), and every node applying the same entry at a
/// given log index (State Machine Safety).
#[derive(Default)]
pub struct SafetyChecker {
    leaders: HashMap<i32, i32>, // term -> leader
    applied: HashMap<i32, i32>, // log index -> term of the entry applied there
    violations: u32,
}

impl SafetyChecker {
    fn record_leader(&mut self, term: i32, node: i32) -> Option<String> {
        let first = *self.leaders.entry(term).or_insert(node);
        (first != node).then(|| {
            self.violations += 1;
            format!("two leaders in term {}: nodes {} and {}", term, first, node)
        })
    }

    fn record_applied(&mut self, index: i32, term: i32, node: i32) -> Option<String> {
        let first = *self.applied.entry(index).or_insert(term);
        (first != term).then(|| {
            self.violations += 1;
            format!("node {} applied an entry from term {} at index {}, others applied term {}", node, term, index, first)
        })
    }

    fn summary(&self) -> String {
        format!(
            "safety: {} terms had a leader, {} log entries applied, {} violations",
            self.leaders.len(),
            self.applied.len(),
            self.violations,
        )
    }
}

pub struct WasmHostState {
    pub instances: HashMap<i32, WasmInstance>,
    pub counter: u32,
    pub config: HashMap<String, String>,
    pub devil_cat: DevilCat,
    pub num_servers: i32,
    pub verbose: bool,
    pub started: Instant,
    pub safety: SafetyChecker,
}

impl Default for WasmHostState {
    fn default() -> Self {
        Self {
            instances: HashMap::new(),
            counter: 0,
            config: HashMap::new(),
            // Raft needs message delays well under its 1.5-3 s election timeout.
            devil_cat: DevilCat::new(10, 150),
            num_servers: DEFAULT_SERVERS,
            verbose: false,
            started: Instant::now(),
            safety: SafetyChecker::default(),
        }
    }
}

impl WasmHostState {
    fn label(&self, id: i32) -> String {
        if id == self.num_servers + 1 { "client".to_string() } else { format!("node {}", id) }
    }

    fn print(&self, who: &str, message: &str) {
        println!("[{:>8.3}s] {:<7}| {}", self.started.elapsed().as_secs_f64(), who, message);
    }

    fn is_server(&self, id: i32) -> bool {
        (1..=self.num_servers).contains(&id)
    }

    fn is_alive(&self, id: i32) -> bool {
        self.instances.get(&id).is_some_and(|instance| instance.alive)
    }
}

#[derive(Debug)]
pub struct WasmInstance {
    instance: Instance,
    sender: Sender<Event>,
    receiver: Receiver<Event>,
    buffer: BinaryHeap<Reverse<Event>>,
    alive: bool,
}

impl WasmInstance {
    /// Drops everything in flight to this node, like a crashed machine would.
    fn shut_down(&mut self) {
        self.alive = false;
        self.buffer.clear();
        self.receiver.try_iter().for_each(drop);
    }
}

#[derive(Clone)]
pub struct HostContext {
    pub state: Arc<Mutex<WasmHostState>>,
    pub engine: Arc<Engine>,
}

impl HostContext {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(WasmHostState::default())),
            engine: Arc::new(Engine::default()),
        }
    }

    fn print(&self, who: &str, message: &str) {
        self.state.lock().unwrap().print(who, message);
    }
}

impl Default for HostContext {
    fn default() -> Self {
        Self::new()
    }
}

enum Command {
    Enqueue(i32),
    Dequeue,
    // None kills whichever server is leading right now.
    Kill(Option<i32>),
    // None revives every server that is down.
    Revive(Option<i32>),
    Status,
    Verbose(bool),
    Delay(i32, i32),
    Help,
    Quit,
}

fn spawn_instance(store: &mut Store<HostContext>, linker: &Linker<HostContext>, module: &Module) -> Result<()> {
    let context = store.data().clone();
    let instance = linker.instantiate(&mut *store, module)?;
    let (sender, receiver) = channel();
    let wasm_instance = WasmInstance {
        instance,
        sender,
        receiver,
        buffer: BinaryHeap::new(),
        alive: true,
    };
    let (instance_id, num_servers) = {
        let mut state = context.state.lock().unwrap();
        state.counter += 1;
        let instance_id = state.counter as i32;
        state.instances.insert(instance_id, wasm_instance);
        (instance_id, state.num_servers)
    };
    call_export::<(i32, i32), ()>(store, instance, "start", (instance_id, num_servers))
}

fn call_export<Params: WasmParams, Results: WasmResults>(
    store: &mut Store<HostContext>,
    instance: Instance,
    name: &str,
    params: Params,
) -> Result<Results> {
    instance.get_typed_func::<Params, Results>(&mut *store, name)?.call(&mut *store, params)
}

/// Calls an export on a live node. A trap (say, a panic in the guest) counts
/// as that node crashing.
fn call_node<Params: WasmParams, Results: WasmResults>(
    store: &mut Store<HostContext>,
    id: i32,
    name: &str,
    params: Params,
) -> Option<Results> {
    let context = store.data().clone();
    let instance = {
        let state = context.state.lock().unwrap();
        match state.instances.get(&id) {
            Some(wasm_instance) if wasm_instance.alive => wasm_instance.instance,
            _ => return None,
        }
    };
    match call_export(store, instance, name, params) {
        Ok(results) => Some(results),
        Err(error) => {
            crash(&context, id, &error);
            None
        }
    }
}

fn crash(context: &HostContext, id: i32, error: &anyhow::Error) {
    let mut state = context.state.lock().unwrap();
    if let Some(wasm_instance) = state.instances.get_mut(&id) {
        wasm_instance.shut_down();
    }
    let label = state.label(id);
    state.print("host", &format!("{} crashed and is now down: {:#}", label, error));
}

fn deliver_message(store: &mut Store<HostContext>, instance: Instance, sender_id: i32, message: &str) -> Result<()> {
    let len = message.len() as i32;
    let msg_ptr: i32 = call_export(store, instance, "allocate", len)?;
    let memory = instance.get_memory(&mut *store, "memory")
        .ok_or_else(|| anyhow!("memory export not found"))?;
    memory.write(&mut *store, msg_ptr as usize, message.as_bytes())?;
    call_export::<(i32, i32, i32), ()>(store, instance, "receive", (sender_id, msg_ptr, len))?;
    call_export::<(i32, i32), ()>(store, instance, "deallocate", (msg_ptr, len))
}

fn run_simulation(store: &mut Store<HostContext>, commands: Receiver<(String, Command)>) {
    let context = store.data().clone();
    loop {
        while let Ok((line, command)) = commands.try_recv() {
            context.print("host", &format!("> {}", line));
            if !handle_command(store, command) {
                return;
            }
        }

        let mut events_to_process: Vec<(i32, Event, Instance)> = {
            let mut state = context.state.lock().unwrap();
            let now = get_epoch_ms();
            let mut events = Vec::new();

            for (id, wasm_instance) in state.instances.iter_mut() {
                // Add new events from receiver to buffer
                wasm_instance.buffer.extend(wasm_instance.receiver.try_iter().map(Reverse));

                // Take the events from the buffer that are ready
                while wasm_instance.buffer.peek().is_some_and(|head| head.0.fire_time <= now) {
                    let event = wasm_instance.buffer.pop().unwrap().0;
                    events.push((*id, event, wasm_instance.instance));
                }
            }

            events
        }; // Release the lock: the guest calls back into the host, which locks it again.
        events_to_process.sort_by_key(|(_, event, _)| event.fire_time);

        for (id, event, instance) in events_to_process {
            // A node can crash while handling an earlier event in this batch.
            if !context.state.lock().unwrap().is_alive(id) {
                continue;
            }
            let result = match event.data {
                EventData::RawMessage { message } => deliver_message(store, instance, event.sender_id, &message),
                EventData::Timer { token } => call_export::<i32, ()>(store, instance, "on_timer", token),
            };
            if let Err(error) = result {
                crash(&context, id, &error);
            }
        }

        thread::sleep(Duration::from_millis(5));
    }
}

/// Returns false when the simulation should stop.
fn handle_command(store: &mut Store<HostContext>, command: Command) -> bool {
    let context = store.data().clone();
    let client_id = context.state.lock().unwrap().num_servers + 1;
    match command {
        Command::Enqueue(value) => {
            call_node::<i32, ()>(store, client_id, "client_enqueue", value);
        }
        Command::Dequeue => {
            call_node::<(), ()>(store, client_id, "client_dequeue", ());
        }
        Command::Kill(target) => kill(store, target),
        Command::Revive(target) => revive(store, target),
        Command::Status => status(store),
        Command::Verbose(on) => context.state.lock().unwrap().verbose = on,
        Command::Delay(min_delay, max_delay) => {
            if min_delay < 0 || min_delay > max_delay {
                context.print("host", "delay needs 0 <= min <= max");
            } else {
                context.state.lock().unwrap().devil_cat = DevilCat::new(min_delay, max_delay);
            }
        }
        Command::Help => println!("{}", HELP),
        Command::Quit => return false,
    }
    true
}

/// The live server with the highest term that thinks it is leader.
fn find_leader(store: &mut Store<HostContext>) -> Option<i32> {
    let num_servers = store.data().state.lock().unwrap().num_servers;
    (1..=num_servers)
        .filter_map(|id| call_node::<(), i32>(store, id, "leader_term", ()).map(|term| (term, id)))
        .filter(|&(term, _)| term >= 0)
        .max()
        .map(|(_, id)| id)
}

fn kill(store: &mut Store<HostContext>, target: Option<i32>) {
    let context = store.data().clone();
    let Some(id) = target.or_else(|| find_leader(store)) else {
        context.print("host", "no live leader to kill right now");
        return;
    };
    let mut state = context.state.lock().unwrap();
    if !state.is_server(id) {
        state.print("host", &format!("{} is not a server", state.label(id)));
        return;
    }
    if !state.is_alive(id) {
        state.print("host", &format!("node {} is already down", id));
        return;
    }
    state.instances.get_mut(&id).unwrap().shut_down();
    state.print("host", &format!("node {} killed", id));
}

fn revive(store: &mut Store<HostContext>, target: Option<i32>) {
    let context = store.data().clone();
    let ids: Vec<i32> = {
        let state = context.state.lock().unwrap();
        match target {
            Some(id) => vec![id],
            None => (1..=state.num_servers).filter(|&id| !state.is_alive(id)).collect(),
        }
    };
    if ids.is_empty() {
        context.print("host", "every server is already running");
    }
    for id in ids {
        {
            let mut state = context.state.lock().unwrap();
            if !state.is_server(id) {
                state.print("host", &format!("{} is not a server", state.label(id)));
                continue;
            }
            if state.is_alive(id) {
                state.print("host", &format!("node {} is already running", id));
                continue;
            }
            state.instances.get_mut(&id).unwrap().alive = true;
            state.print("host", &format!("node {} revived", id));
        }
        call_node::<(), ()>(store, id, "restart", ());
    }
}

fn status(store: &mut Store<HostContext>) {
    let context = store.data().clone();
    let num_servers = context.state.lock().unwrap().num_servers;
    for id in 1..=num_servers + 1 {
        if call_node::<(), ()>(store, id, "status", ()).is_none() {
            let state = context.state.lock().unwrap();
            state.print(&state.label(id), "DOWN");
        }
    }
    let state = context.state.lock().unwrap();
    let network = format!("network delay {}-{} ms", state.devil_cat.min_delay, state.devil_cat.max_delay);
    state.print("host", &format!("{}; {}", network, state.safety.summary()));
}

fn parse_number(word: &str) -> Result<i32, String> {
    word.parse().map_err(|_| format!("`{}` is not a number", word))
}

fn parse_command(line: &str) -> Result<Command, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["enq" | "enqueue", value] => Ok(Command::Enqueue(parse_number(value)?)),
        ["deq" | "dequeue"] => Ok(Command::Dequeue),
        ["kill", "leader"] => Ok(Command::Kill(None)),
        ["kill", id] => Ok(Command::Kill(Some(parse_number(id)?))),
        ["revive", "all"] => Ok(Command::Revive(None)),
        ["revive", id] => Ok(Command::Revive(Some(parse_number(id)?))),
        ["status"] => Ok(Command::Status),
        ["verbose", "on"] => Ok(Command::Verbose(true)),
        ["verbose", "off"] => Ok(Command::Verbose(false)),
        ["delay", min_delay, max_delay] => Ok(Command::Delay(parse_number(min_delay)?, parse_number(max_delay)?)),
        ["help"] => Ok(Command::Help),
        ["quit" | "exit"] => Ok(Command::Quit),
        _ => Err(format!("unknown command `{}`, try `help`", line)),
    }
}

/// Reads commands from stdin, typed or piped in from a script.
fn spawn_command_reader() -> Receiver<(String, Command)> {
    let (sender, receiver) = channel();
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Handled here so it pauses the script, not the simulation.
            if let Some(ms) = line.strip_prefix("sleep ") {
                match ms.trim().parse() {
                    Ok(ms) => thread::sleep(Duration::from_millis(ms)),
                    Err(_) => println!("usage: sleep <ms>"),
                }
                continue;
            }
            match parse_command(line) {
                Ok(command) => {
                    if sender.send((line.to_string(), command)).is_err() {
                        return;
                    }
                }
                Err(message) => println!("{}", message),
            }
        }
        // End of input (Ctrl-D, or the end of a script) stops the simulation.
        let _ = sender.send(("quit".to_string(), Command::Quit));
    });
    receiver
}

fn main() -> Result<()> {
    let num_servers: i32 = match std::env::args().nth(1) {
        Some(arg) => arg.parse().ok().filter(|&n| n >= 1)
            .ok_or_else(|| anyhow!("usage: wasmhost [number of servers, default {}]", DEFAULT_SERVERS))?,
        None => DEFAULT_SERVERS,
    };

    let context = HostContext::new();
    context.state.lock().unwrap().num_servers = num_servers;
    let module = Module::from_file(&context.engine, WASM_PATH)
        .with_context(|| format!("couldn't load {}; build wasminstance first", WASM_PATH))?;
    let mut store = Store::new(&context.engine, context.clone());
    let mut linker = Linker::new(&context.engine);
    linker.func_wrap("env", "log_str", log_str)?;
    linker.func_wrap("env", "send_message", send_message)?;
    linker.func_wrap("env", "set_timer", set_timer)?;
    linker.func_wrap("env", "random_between", random_between)?;
    linker.func_wrap("env", "report_leader", report_leader)?;
    linker.func_wrap("env", "report_applied", report_applied)?;

    println!("{}\n", HELP);
    // Servers get ids 1..=num_servers; the instance after them is the client.
    for _ in 0..=num_servers {
        spawn_instance(&mut store, &linker, &module)?;
    }

    let commands = spawn_command_reader();
    run_simulation(&mut store, commands);

    let state = context.state.lock().unwrap();
    state.print("host", &state.safety.summary());
    Ok(())
}


/// The id of the instance calling into the host.
fn caller_id(caller: &mut Caller<'_, HostContext>) -> i32 {
    let Some(Extern::Func(get_instance)) = caller.get_export("get_instance") else {
        return -1;
    };
    get_instance.typed::<(), i32>(&*caller)
        .and_then(|get_instance| get_instance.call(&mut *caller, ()))
        .unwrap_or(-1)
}

fn read_string(caller: &mut Caller<'_, HostContext>, ptr: i32, len: i32) -> Option<String> {
    let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
        return None;
    };
    let bytes = memory.data(&*caller)
        .get(ptr as u32 as usize..)?
        .get(..len as u32 as usize)?;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn log_str(mut caller: Caller<'_, HostContext>, level: i32, ptr: i32, len: i32) {
    let id = caller_id(&mut caller);
    let message = read_string(&mut caller, ptr, len)
        .unwrap_or_else(|| "pointer/length out of bounds".to_string());
    let state = caller.data().state.lock().unwrap();
    if level < LOG_DEBUG || state.verbose {
        state.print(&state.label(id), &message);
    }
}

fn get_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

pub fn send_message(mut caller: Caller<'_, HostContext>, target_id: i32, msg_ptr: i32, msg_len: i32) {
    let instance_id = caller_id(&mut caller);
    let Some(message) = read_string(&mut caller, msg_ptr, msg_len) else {
        return;
    };
    let state = caller.data().state.lock().unwrap();
    let route = format!("{} -> {}", state.label(instance_id), state.label(target_id));
    match state.instances.get(&target_id) {
        Some(wasm_instance) if wasm_instance.alive => {
            let delay = state.devil_cat.get_random_delay();
            if state.verbose {
                state.print("net", &format!("{} in {}ms: {}", route, delay, message));
            }
            let event = Event::new(get_epoch_ms() + delay, instance_id, EventData::RawMessage { message });
            wasm_instance.sender.send(event).unwrap();
        }
        Some(_) => {
            if state.verbose {
                state.print("net", &format!("{} lost, target is down: {}", route, message));
            }
        }
        None => state.print("net", &format!("{} lost, no such node", route)),
    }
}

/// Schedules `on_timer(token)` on the calling instance. Timers skip the network delay.
fn set_timer(mut caller: Caller<'_, HostContext>, token: i32, delay_ms: i32) {
    let instance_id = caller_id(&mut caller);
    let state = caller.data().state.lock().unwrap();
    if let Some(wasm_instance) = state.instances.get(&instance_id) {
        let event = Event::new(get_epoch_ms() + delay_ms.max(0) as u128, instance_id, EventData::Timer { token });
        wasm_instance.sender.send(event).unwrap();
    }
}

fn random_between(min: i32, max: i32) -> i32 {
    if min >= max { min } else { rand::thread_rng().gen_range(min..=max) }
}

fn report_leader(mut caller: Caller<'_, HostContext>, term: i32) {
    let instance_id = caller_id(&mut caller);
    let mut state = caller.data().state.lock().unwrap();
    if let Some(problem) = state.safety.record_leader(term, instance_id) {
        state.print("SAFETY", &format!("VIOLATION: {}", problem));
    }
}

fn report_applied(mut caller: Caller<'_, HostContext>, index: i32, term: i32) {
    let instance_id = caller_id(&mut caller);
    let mut state = caller.data().state.lock().unwrap();
    if let Some(problem) = state.safety.record_applied(index, term, instance_id) {
        state.print("SAFETY", &format!("VIOLATION: {}", problem));
    }
}
