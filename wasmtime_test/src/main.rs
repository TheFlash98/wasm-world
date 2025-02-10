use std::error::Error;
use wasmtime::*;
use std::str;


fn main() -> Result<(), Box<dyn Error>> {
    let engine = Engine::default();
    let module = Module::from_file(&engine, "../wasm-env/target/wasm32-unknown-unknown/release/wasm_env.wasm")?;
   
    let mut linker = Linker::new(&engine);
    linker.func_wrap("env", "double", double)?;
    linker.func_wrap("env", "log_str", log_str)?;

    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &module)?;

    let memory = instance.get_memory(&mut store, "memory")
        .ok_or("failed to find `memory` export")?;
    
    println!("Memory data size: {:?}", memory.data_size(&store));
    println!("Memory size: {:?}", memory.size(&store));   

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
    memory.write(&mut store, 0, first_name)?;
    memory.write(&mut store, first_name.len(), last_name)?;
    
    say_hello.call(&mut store, (0, first_name.len() as i32))?;
    say_hello.call(&mut store, (first_name.len() as i32, last_name.len() as i32))?;
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
