use std::slice;
use std::str;
use std::alloc::{alloc, dealloc, Layout};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;

static mut INSTANCE: Option<InstanceState> = None;

extern "C" {
    fn double(x: i32) -> i32;
    fn log_str(ptr: i32, len: i32);
    fn log_struct(ptr: i32, len: i32);
    // fn send_message(target_id: i32, ptr: i32, len: i32);
    fn send_message(target_id: i32, ptr: i32, len: i32);
}

#[derive(Serialize, Deserialize)]
pub struct Example {
    pub field1: HashMap<u32, String>,
    pub field2: Vec<Vec<f32>>,
    pub field3: [f32; 4],
}

#[repr(C)]
pub struct WasmMemory {
    ptr: *mut u8,
    size: usize,
}

impl WasmMemory {
    pub extern "C" fn new(size: usize) -> Self {
        let layout = Layout::from_size_align(size, 1).unwrap();
        let ptr = unsafe { alloc(layout) };
        WasmMemory { ptr, size }
    }
}

impl Drop for WasmMemory {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            let layout = Layout::from_size_align(self.size, 1).unwrap();
            unsafe { dealloc(self.ptr, layout) };
        }
    }
}

#[no_mangle]
pub extern "C" fn allocate(size: usize) -> *mut u8 {
    let mem = WasmMemory::new(size);
    let ptr = mem.ptr;
    std::mem::forget(mem); // Prevent drop
    ptr
}

#[no_mangle]
pub extern "C" fn deallocate(ptr: *mut u8, size: usize) {
    let _ = WasmMemory { ptr, size };
}

pub trait Actor {
    fn init(&mut self);
    fn receive(&mut self, ptr: i32, len: i32);
}

pub struct InstanceState {
    id: i32
}

impl Actor for InstanceState {
    fn init(&mut self) {
        // Initialize the instance
        let s = format!("Hello from instance state struct with id: {}", self.id);
        unsafe {
            log_str(s.as_ptr() as i32, s.len() as i32);
        }
        let s = format!("ping from {}", self.id);
        unsafe {
            send_message(1, s.as_ptr() as i32, s.len() as i32);
        }
    }
    // This function will be called when a message is received
    // It will be called from the host
    fn receive(&mut self, ptr: i32, len: i32) {
        let slice = unsafe { std::slice::from_raw_parts(ptr as _, len as _) };
        let message = std::str::from_utf8(slice).unwrap();
        let ack = format!("pong -> {} {}", message, self.id);
        unsafe {
            log_str(ack.as_ptr() as i32, ack.len() as i32)
        }
    }
}

#[no_mangle]
pub extern fn start(id: i32) {
    // Create a main function that runs once every instance comes up. 
    // Every instance has a main function as well as an init function
    let s = "Hello from the main function";
    unsafe {
        log_str(s.as_ptr() as i32, s.len() as i32);
    }
    init(id);
}


fn init(id: i32) {
    // The init function will call the “structs” init function 
    // that evaluates the actual code needed
    // let gen_id = unsafe { gen_id() };
    // InstanceState {
    //     id: id
    // }.init();
    let instance = InstanceState {
        id
    };
    unsafe {
        INSTANCE = Some(instance);
        if let Some(instance) = &mut INSTANCE {
            instance.init();
        }
    }
}

#[no_mangle]
pub extern "C" fn get_instance() -> i32 {
    unsafe {
        if let Some(instance) = &INSTANCE {
            return instance.id;
        }
    }
    -1
}

#[no_mangle]
pub extern "C" fn receive(ptr: i32, len: i32) {
    unsafe {
        if let Some(instance) = &mut INSTANCE {
            instance.receive(ptr, len);
        }
    }
}

#[no_mangle]
pub extern fn add(left: i32, right: i32) -> i32 {
    unsafe{
        double(left + right)
    }
}

#[no_mangle]
pub extern fn return_string() {
    let s = "Hello";
    unsafe {
        log_str(s.as_ptr() as i32, s.len() as i32);
    }
}

#[no_mangle]
pub extern fn say_hello(ptr: i32, len: i32) {
    let slice = unsafe { slice::from_raw_parts(ptr as _, len as _) };
    let string_from_host = str::from_utf8(slice).unwrap();
    let out_str = format!("Hola, {}!", string_from_host);
    unsafe {
        log_str(out_str.as_ptr() as i32, out_str.len() as i32);
    }
}

#[no_mangle]
pub fn send_example_to_host() {
    let mut field1 = HashMap::new();
    field1.insert(0, String::from("ex"));
    field1.insert(1, String::from("ex-1"));
    let example = Example {
        field1,
        field2: vec![vec![1., 2.], vec![3., 4.]],
        field3: [1., 2., 3., 4.]
    };

    let example_ron = ron::to_string(&example).unwrap();
    unsafe {
        log_struct(example_ron.as_ptr() as i32, example_ron.len() as i32);
    }
}

// #[no_mangle]
// pub extern fn send() {
//     let msg = "ping";
//     unsafe {
//         send_message(2, msg.as_ptr() as i32, msg.len() as i32);
//     }
// }



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
