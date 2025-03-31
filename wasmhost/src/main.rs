use std::error::Error;
use wasmtime::*;
use std::str;
use std::sync::{Arc, Mutex};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;


#[derive(Serialize, Deserialize)]
pub struct Example {
    pub field1: HashMap<u32, String>,
    pub field2: Vec<Vec<f32>>,
    pub field3: [f32; 4],
}

#[derive(Default)]
pub struct WasmHostState {
    pub instances: HashMap<i32, WasmInstance>,
    pub counter: u32,
    pub config: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct WasmInstance {
    instance: Instance,
    memory: Memory,
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


fn spawn_instance(store: &mut Store<HostContext>, linker: &Linker<HostContext>, module: &Module) -> Result<WasmInstance, Box<dyn Error>> {
    let context = store.data().clone();
    let instance = linker.instantiate(&mut *store, module)?;
    let memory = instance.get_memory(&mut *store, "memory")
        .expect("memory export not found");
    let wasm_instance = WasmInstance {
        instance,
        memory,
    };
    let instance_id = {
        let mut state = context.state.lock().unwrap();
        state.counter += 1;
        let instance_id = state.counter as i32;
        state.instances.insert(instance_id, wasm_instance.clone());
        instance_id
    };
    let start = instance.get_func(&mut *store, "start")
        .expect("start function not found");
    let start = start.typed::<i32, ()>(&mut *store)
        .expect("start function not found");
    start.call(&mut *store, instance_id)?;
    
    Ok(wasm_instance)
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

    let wasm_instance = spawn_instance(&mut store, &linker, &module)?;
    for _ in 1..=3 {
        spawn_instance(&mut store, &linker, &module)?;
    }
    // let instance = wasm_instance.instance;
    let memory = wasm_instance.memory;
    

    println!("Memory data size: {:?}", memory.data_size(&store));
    println!("Memory size: {:?}", memory.size(&store));
    println!("State hash map: {:?}", context.state.lock().unwrap().instances);    
    // let send = instance.get_func(&mut store, "send")
    //     .expect("send was not an exported function");
    // let send = send.typed::<(), ()>(&store)?;
    // send.call(&mut store, ())?;
    
    // Allocate Memory Example
    // let alloc = instance.get_func(&mut store, "allocate")
    //     .expect("`alloc` was not an exported function");
    // let alloc = alloc.typed::<i32, i32>(&store)?;
    // let curr_ptr = alloc.call(&mut store, 128)?;
    // println!("Allocated memory at: {:?}", curr_ptr);

    // let add = instance.get_func(&mut store, "add")
    //     .expect("`add` was not an exported function");
    
    // let add = add.typed::<(i32, i32), i32>(&store)?;
    // let result = add.call(&mut store, (2, 2))?;
    // println!("Answer: {:?}", result);

    // let return_string = instance.get_func(&mut store, "return_string")
    //     .expect("`return_string` was not an exported function");
    // let return_string = return_string.typed::<(), ()>(&store)?;
    // return_string.call(&mut store, ())?;


    // Write to allocated memory memory and check working
    // let say_hello = instance.get_func(&mut store, "say_hello")
    //     .expect("`say_hello` was not an exported function");
    // let say_hello = say_hello.typed::<(i32, i32), ()>(&store)?;
    
    // let first_name = b"Sarthak";
    // let last_name = b"Khandelwal";
    // memory.write(&mut store, curr_ptr.try_into().unwrap(), first_name)?;
    // memory.write(&mut store, first_name.len(), last_name)?;
    
    // say_hello.call(&mut store, (curr_ptr, first_name.len() as i32))?;
    // say_hello.call(&mut store, (first_name.len() as i32, last_name.len() as i32))?;

    // let dealloc = instance.get_func(&mut store, "deallocate")
    //     .expect("`deallocate` was not an exported function");
    // let dealloc = dealloc.typed::<(i32, i32), ()>(&store)?;
    // dealloc.call(&mut store, (curr_ptr, 128))?;
    // println!("Deallocated memory at: {:?}", curr_ptr);

    // say_hello.call(&mut store, (curr_ptr, first_name.len() as i32))?;

    // let send_example_to_host = instance.get_func(&mut store, "send_example_to_host")
    //     .expect("`send_example_to_host` was not an exported function");
    // let send_example_to_host = send_example_to_host.typed::<(), ()>(&store)?;
    // send_example_to_host.call(&mut store, ())?;
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
        if let Some(wasm_instance) = state.instances.get_mut(&target_id) {
            let instance = wasm_instance.instance;
            if let Some(receive_func) = instance.get_func(&mut caller, "receive") {
                let receive_func = receive_func.typed::<(i32, i32), ()>(&caller).unwrap();
                let alloc_func = instance.get_func(&mut caller, "allocate").unwrap().typed::<i32, i32>(&caller).unwrap();
                
                let msg_ptr = alloc_func.call(&mut caller, message.len() as i32).unwrap();
                let memory = instance.get_memory(&mut caller, "memory").unwrap();
                memory.write(&mut caller, msg_ptr as usize, message.as_bytes()).unwrap();

                receive_func.call(&mut caller, (msg_ptr, message.len() as i32)).unwrap();
            }
        }
    }
}
