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



#[derive(Serialize, Deserialize)]
pub struct Example {
    pub field1: HashMap<u32, String>,
    pub field2: Vec<Vec<f32>>,
    pub field3: [f32; 4],
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag="event_type")]
pub enum EventData {
    AppendRequest {
        param1: i32,
        param2: i32,
        param3: i32,
    },
    AppendRequestResponse {
        param4: i32
    },
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
    pub data: EventData,
}

impl Event {
    pub fn new(fire_time: u128, data: EventData) -> Self {
        Self { fire_time, data }
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

fn handle_send_recv(mut store: Store<HostContext>) {
    let context = store.data().clone();
    let mut state = context.state.lock().unwrap();
    loop {
        for (id, wasm_instance) in state.instances.iter_mut() {

            std::thread::sleep(std::time::Duration::from_millis(1));
            let now = get_epoch_ms();
            let receiver = &wasm_instance.receiver;
            // let mut buffer = wasm_instance.buffer.lock().unwrap();
            let mut buffer = &mut wasm_instance.buffer;
            if id == &1 {
                // println!("Buffer for instance {}: {:?}", id, buffer.len());
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
                                let receive_func = receive_func.typed::<(i32, i32), ()>(&store).unwrap();
                                let alloc_func = instance.get_func(&mut store, "allocate").unwrap().typed::<i32, i32>(&store).unwrap();
                                
                                let msg_ptr = alloc_func.call(&mut store, message.len() as i32).unwrap();
                                let memory = instance.get_memory(&mut store, "memory").unwrap();
                                memory.write(&mut store, msg_ptr as usize, message.as_bytes()).unwrap();
                                
                                receive_func.call(&mut store, (msg_ptr, message.len() as i32)).unwrap();
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
        }
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
    linker.func_wrap("env", "double", double)?;
    linker.func_wrap("env", "log_struct", log_struct)?;
    linker.func_wrap("env", "send_message", send_message)?;

    // let wasm_instance = spawn_instance(&mut store, &linker, &module)?;
    for _ in 1..=3 {
        spawn_instance(&mut store, &linker, &module)?;
    }
    thread::spawn({
        let context = context.clone();
        move || {
            handle_send_recv(store);
        }
    }).join().unwrap();

    Ok(())
}

fn double(x: i32) -> i32 {
    x * 2
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
    println!("From wasm: {}", string);
}

fn log_struct(mut caller: Caller<'_, HostContext>, ptr: i32, len: i32) {
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
    let serialized = match data {
        Some(data) => str::from_utf8(data).unwrap_or("invalid utf-8"),
        None => "pointer/length out of bounds",
    };
    let deserialized: Example = ron::from_str(serialized).unwrap();
    println!("deserialized = {:?}", deserialized.field1);
    println!("deserialized = {:?}", deserialized.field2);
    println!("deserialized = {:?}", deserialized.field3);
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
        let delay = {
            let devil_cat = &state.devil_cat; // Immutable borrow
            devil_cat.get_random_delay()
        };
        if let Some(wasm_instance) = state.instances.get_mut(&target_id) {
            let sender = &wasm_instance.sender;
            let event = Event::new(get_epoch_ms() + delay, EventData::RawMessage { message: message.clone() });
            println!("Message sent to instance {}: {:?}", target_id, event);
            sender.send(event).unwrap();
            // let instance = wasm_instance.instance;
            // if let Some(receive_func) = instance.get_func(&mut caller, "receive") {
            //     let receive_func = receive_func.typed::<(i32, i32), ()>(&caller).unwrap();
            //     let alloc_func = instance.get_func(&mut caller, "allocate").unwrap().typed::<i32, i32>(&caller).unwrap();
                
            //     let msg_ptr = alloc_func.call(&mut caller, message.len() as i32).unwrap();
            //     let memory = instance.get_memory(&mut caller, "memory").unwrap();
            //     memory.write(&mut caller, msg_ptr as usize, message.as_bytes()).unwrap();

            //     receive_func.call(&mut caller, (msg_ptr, message.len() as i32)).unwrap();
            // }
        }
    }
}
