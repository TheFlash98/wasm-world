use std::error::Error;
use wasmtime::*;
use std::str;
use serde::{Serialize, Deserialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
pub struct Example {
    pub field1: HashMap<u32, String>,
    pub field2: Vec<Vec<f32>>,
    pub field3: [f32; 4],
}

fn main() -> Result<(), Box<dyn Error>> {
    let engine = Engine::default();
    let module = Module::from_file(&engine, "../wasm-env/target/wasm32-unknown-unknown/release/wasm_env.wasm")?;
   
    let mut linker = Linker::new(&engine);
    linker.func_wrap("env", "double", double)?;
    linker.func_wrap("env", "log_str", log_str)?;
    linker.func_wrap("env", "log_struct", log_struct)?;

    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &module)?;

    let memory = instance.get_memory(&mut store, "memory")
        .ok_or("failed to find `memory` export")?;
    
    println!("Memory data size: {:?}", memory.data_size(&store));
    println!("Memory size: {:?}", memory.size(&store));   

    let alloc = instance.get_func(&mut store, "allocate")
        .expect("`alloc` was not an exported function");
    let alloc = alloc.typed::<i32, i32>(&store)?;
    let curr_ptr = alloc.call(&mut store, 128)?;
    println!("Allocated memory at: {:?}", curr_ptr);
    let curr_ptr2 = alloc.call(&mut store, 128)?;
    println!("Allocated memory at: {:?}", curr_ptr2);

    let add = instance.get_func(&mut store, "add")
        .expect("`add` was not an exported function");
    
    let add = add.typed::<(i32, i32), i32>(&store)?;
    let result = add.call(&mut store, (2, 2))?;
    println!("Answer: {:?}", result);

    let return_string = instance.get_func(&mut store, "return_string")
        .expect("`return_string` was not an exported function");
    let return_string = return_string.typed::<(), ()>(&store)?;
    return_string.call(&mut store, ())?;

    let say_hello = instance.get_func(&mut store, "say_hello")
        .expect("`say_hello` was not an exported function");
    let say_hello = say_hello.typed::<(i32, i32), ()>(&store)?;
    
    let first_name = b"Sarthak";
    let last_name = b"Khandelwal";
    memory.write(&mut store, curr_ptr.try_into().unwrap(), first_name)?;
    memory.write(&mut store, first_name.len(), last_name)?;
    
    say_hello.call(&mut store, (curr_ptr.try_into().unwrap(), first_name.len() as i32))?;
    say_hello.call(&mut store, (first_name.len() as i32, last_name.len() as i32))?;

    let send_example_to_host = instance.get_func(&mut store, "send_example_to_host")
        .expect("`send_example_to_host` was not an exported function");
    let send_example_to_host = send_example_to_host.typed::<(), ()>(&store)?;
    send_example_to_host.call(&mut store, ())?;
    Ok(())
}

fn double(x: i32) -> i32 {
    x * 2
}

fn log_str(mut caller: Caller<'_, ()>, ptr: i32, len: i32) {
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
        Some(data) => match str::from_utf8(data) {
            Ok(s) => s,
            Err(_) => "invalid utf-8",
        },
        None => "pointer/length out of bounds",
    };
    println!("From wasm: {}", string);
}

fn log_struct(mut caller: Caller<'_, ()>, ptr: i32, len: i32) {
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
        Some(data) => match str::from_utf8(data) {
            Ok(s) => s,
            Err(_) => "invalid utf-8",
        },
        None => "pointer/length out of bounds",
    };
    let deserialized: Example = serde_json::from_str(&serialized).unwrap();
    println!("deserialized = {:?}", deserialized.field1);
    println!("deserialized = {:?}", deserialized.field2);
    println!("deserialized = {:?}", deserialized.field3);
}