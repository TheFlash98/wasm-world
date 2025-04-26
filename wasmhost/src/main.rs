use std::error::Error;
use wasmtime::*;
use std::str;
use std::sync::{Arc, Mutex};
use std::thread;
use serde::{Serialize, Deserialize};
use std::collections::{HashMap, BinaryHeap};
use std::sync::mpsc::channel;
use std::sync::mpsc::{Sender, Receiver};
use rand::Rng;
use std::time::{SystemTime, UNIX_EPOCH};
use std::cmp::{Reverse, Ord, Ordering};


#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag="event_type")]
pub enum EventData {
    Timer {
        timer_name: String  
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

pub struct WasmHostState {
    pub instances: HashMap<i32, WasmInstance>,
    pub counter: u32,
    pub config: HashMap<String, String>,
    pub devil_cat: DevilCat,
}

impl Default for WasmHostState {
    fn default() -> Self {
        Self {
            instances: HashMap::new(),
            counter: 0,
            config: HashMap::new(),
            devil_cat: DevilCat::new(10, 5000),
        }
    }
}

#[derive(Debug)]
pub struct WasmInstance {
    instance: Instance,
    sender: Sender<Event>,
    receiver: Receiver<Event>,
    // buffer: Arc<Mutex<BinaryHeap<Reverse<Event>>>>,
    buffer: BinaryHeap<Reverse<Event>>,
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
}

impl Default for HostContext {
    fn default() -> Self {
        Self::new()
    }
}

fn spawn_instance(store: &mut Store<HostContext>, linker: &Linker<HostContext>, module: &Module) -> Result<(), Box<dyn Error>> {
    let context = store.data().clone();
    let instance = linker.instantiate(&mut *store, module)?;
    // let memory = instance.get_memory(&mut *store, "memory")
    //     .expect("memory export not found");
    let (sender, receiver) = channel();
    let wasm_instance = WasmInstance {
        instance,
        // memory,
        sender,
        receiver,
        buffer: BinaryHeap::new(),
        // buffer: Arc::new(Mutex::new(BinaryHeap::new()))
    };
    let instance_id = {
        let mut state = context.state.lock().unwrap();
        state.counter += 1;
        let instance_id = state.counter as i32;
        state.instances.insert(instance_id, wasm_instance);
        instance_id
    };
    let start = instance.get_func(&mut *store, "start")
        .expect("start function not found");
    let start = start.typed::<i32, ()>(&mut *store)
        .expect("start function not found");
    start.call(&mut *store, instance_id)?;
    
    Ok(())
}

fn _handle_send_recv_old(mut store: Store<HostContext>) {
    let context = store.data().clone();
    loop {{
        let mut state = context.state.lock().unwrap();
        for (id, wasm_instance) in state.instances.iter_mut() {
            println!("Checking events for instance {}", id);
            std::thread::sleep(std::time::Duration::from_millis(1));
            let now = get_epoch_ms();
            let receiver = &wasm_instance.receiver;
            // let mut buffer = wasm_instance.buffer.lock().unwrap();
            let mut buffer = &mut wasm_instance.buffer;
            if id == &2 {
                println!("Buffer for instance {}: {:?}", id, buffer.len());
            }
            // println!("Checking events for instance {} {}", id, buffer.len());
            while let Some(buffer_head) = buffer.peek() {
                if buffer_head.0.fire_time > now {
                    // println!("Buffer head is in the future: {:?}", buffer_head);
                    break;
                } else {
                    println!("Buffer head is in the past at {}", now);
                    println!("Processing event from buffer for instance {}: {:?} at time {}", id, buffer_head, buffer_head.0.fire_time);
                    let event = buffer.pop().unwrap().0;
                    match event.data {
                        EventData::RawMessage { message } => {
                            println!("Received message for instance {}: {:?}", id, message);
                            // let mut state = context.state.lock().unwrap();
                            let instance = wasm_instance.instance;
                            if let Some(receive_func) = instance.get_func(&mut store, "receive") {
                                let receive_func = receive_func.typed::<(i32, i32, i32), ()>(&store).unwrap();
                                let alloc_func = instance.get_func(&mut store, "allocate").unwrap().typed::<i32, i32>(&store).unwrap();
                                
                                let msg_ptr = alloc_func.call(&mut store, message.len() as i32).unwrap();
                                let memory = instance.get_memory(&mut store, "memory").unwrap();
                                memory.write(&mut store, msg_ptr as usize, message.as_bytes()).unwrap();
                                
                                receive_func.call(&mut store, (event.sender_id, msg_ptr, message.len() as i32)).unwrap();
                            }
                        },
                        _ => {
                            println!("Received event for instance {}: {:?}", id, event);
                        }
                    }
                }
            }
            let mut recv_iter = receiver.try_iter();
            while let Some(next_event) = recv_iter.next() {
                println!("Received event for instance {}: {:?}", id, next_event);
                buffer.push(Reverse(next_event));
            }
            println!("Done checking events for instance {}", id);
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(10));
}
}

fn handle_send_recv(mut store: Store<HostContext>) {
    let context = store.data().clone();
    loop {
        let events_to_process: Vec<(i32, Event, Instance)> = {
            let mut state = context.state.lock().unwrap();
            let mut events = Vec::new();
            
            for (id, wasm_instance) in state.instances.iter_mut() {
                let now = get_epoch_ms();
                
                // Process events from buffer that are ready
                while let Some(buffer_head) = wasm_instance.buffer.peek() {
                    if buffer_head.0.fire_time > now {
                        break;
                    } else {
                        let event = wasm_instance.buffer.pop().unwrap().0;
                        events.push((*id, event, wasm_instance.instance));
                    }
                }
                
                // Add new events from receiver to buffer
                let mut recv_iter = wasm_instance.receiver.try_iter();
                while let Some(next_event) = recv_iter.next() {
                    wasm_instance.buffer.push(Reverse(next_event));
                }
            }
            
            events
        }; // Release the lock on state here
        
        // Now process all events without holding the lock
        for (id, event, instance) in events_to_process {
            match event.data {
                EventData::RawMessage { message } => {
                    println!("Processing message for instance {}: {:?}", id, message);
                    
                    if let Some(receive_func) = instance.get_func(&mut store, "receive") {
                        let receive_func = receive_func.typed::<(i32, i32, i32), ()>(&store).unwrap();
                        let alloc_func = instance.get_func(&mut store, "allocate").unwrap().typed::<i32, i32>(&store).unwrap();
                        
                        let msg_ptr = alloc_func.call(&mut store, message.len() as i32).unwrap();
                        let memory = instance.get_memory(&mut store, "memory").unwrap();
                        memory.write(&mut store, msg_ptr as usize, message.as_bytes()).unwrap();
                        
                        receive_func.call(&mut store, (event.sender_id, msg_ptr, message.len() as i32)).unwrap();
                    }
                },
                _ => {
                    println!("Processed event for instance {}: {:?}", id, event);
                }
            }
        }
        
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    
    let context = HostContext::new();
    let module = Module::from_file(&context.engine, "../wasminstance/target/wasm32-unknown-unknown/release/wasminstance.wasm")?;
    let mut store = Store::new(&context.engine, context.clone());
    let mut linker = Linker::new(&context.engine);
    linker.func_wrap("env", "log_str", |caller: Caller<'_, HostContext>, ptr, len| {
        log_str(caller, ptr, len)
    })?;
    linker.func_wrap("env", "send_message", send_message)?;

    // let wasm_instance = spawn_instance(&mut store, &linker, &module)?;
    for _ in 1..=4 {
        spawn_instance(&mut store, &linker, &module)?;
    }

    // Make instance #1 leader, ideally can be chosen at random in the future
    // as long as we can remember who it is
    let leader_instance ={
        let mut state = context.state.lock().unwrap();
        state.instances.get_mut(&1).unwrap().instance
    };
    let make_leader_host = leader_instance.get_func(&mut store, "make_leader_host")
        .expect("make_leader_host function not found");
    let make_leader_host = make_leader_host.typed::<(), ()>(&mut store)
        .expect("make_leader_host function call failed");
    make_leader_host.call(&mut store, ())?;

    // Make the last instance spawned as the client who
    // issues enqueue, dequeue requests
    let client_id = {
        let mut state = context.state.lock().unwrap();
        state.counter as i32
    };
    println!("Client ID: {:?}", client_id);
    let client = {
        let mut state = context.state.lock().unwrap();
        state.instances.get_mut(&client_id).unwrap().instance
    };
    let client_enqueue = client.get_func(&mut store, "client_enqueue")
        .expect("client_enqueue function not found");
    let client_enqueue = client_enqueue.typed::<(i32, i32, i32), ()>(&mut store)
        .expect("client_enqueue function call failed");
    client_enqueue.call(&mut store, (111, 1, client_id))?;

    thread::spawn({
        move || {
            handle_send_recv(store);
        }
    }).join().unwrap();

    Ok(())
}


fn log_str(mut caller: Caller<'_, HostContext>, ptr: i32, len: i32) {
    let mem = match caller.get_export("memory") {
        Some(Extern::Memory(mem)) => mem,
        _ => {
            println!("failed to find `memory` export");
            return;
        },
    };
    let data = mem.data(&caller)
        .get(ptr as u32 as usize..)
        .and_then(|arr| arr.get(..len as u32 as usize));
    let string = match data {
        Some(data) => str::from_utf8(data).unwrap_or("invalid utf-8"),
        None => "pointer/length out of bounds",
    };
    println!("--> From wasm: {}", string);
}

fn get_epoch_ms() -> u128 {
    // println!("Getting epoch ms");
    // Get the current time in milliseconds since the UNIX epoch
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    // println!("Current time in milliseconds: {:?}", time);
    time
}

pub fn send_message(mut caller: Caller<'_, HostContext>, target_id: i32, msg_ptr: i32, msg_len: i32) {
    let memory = match caller.get_export("memory") {
        Some(Extern::Memory(mem)) => mem,
        _ => {
            println!("failed to find `memory` export");
            return;
        },
    };
    let get_instance = match caller.get_export("get_instance") {
        Some(Extern::Func(func)) => func,
        _ => {
            println!("failed to find `get_instance` export");
            return;
        },
    };
    let get_instance = get_instance.typed::<(), i32>(&caller).unwrap();
    let instance_id = get_instance.call(&mut caller, ()).unwrap();
    println!("Instance ID: {:?} sending message to {:?}", instance_id, target_id);
    let message = memory.data(&caller)
        .get(msg_ptr as usize..)
        .and_then(|arr| arr.get(..msg_len as usize))
        .map(|s| String::from_utf8_lossy(s).to_string());
    if let Some(message) = message {
        
        let context = caller.data().clone();
        // let state = context.state.clone();
        let mut state = context.state.lock().unwrap();
        println!("Message to send: {:?}", message);
        let delay = {
            let devil_cat = &state.devil_cat; // Immutable borrow
            devil_cat.get_random_delay()
        };
        
        if let Some(wasm_instance) = state.instances.get_mut(&target_id) {
            let sender = &wasm_instance.sender;
            let event = Event::new(get_epoch_ms() + delay, instance_id, EventData::RawMessage { message: message.clone() });
            println!("Message sent to instance {}: {:?}", target_id, event);
            sender.send(event).unwrap();
        }
    }
}
