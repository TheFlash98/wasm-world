use std::slice;
use std::str;
use std::alloc::{alloc, dealloc, Layout};
use serde::{Serialize, Deserialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize)]
pub struct Example {
    pub field1: HashMap<u32, String>,
    pub field2: Vec<Vec<f32>>,
    pub field3: [f32; 4],
}

extern "C" {
    fn double(x: i32) -> i32;
    fn log_str(ptr: i32, len: i32);
    fn log_struct(ptr: i32, len: i32);
}

#[no_mangle]
pub extern "C" fn allocate(size: usize) -> *mut u8 {
    let layout = Layout::from_size_align(size, 1).unwrap();
    unsafe { alloc(layout) }
}

#[no_mangle]
pub extern "C" fn deallocate(ptr: *mut u8, size: usize) {
    if !ptr.is_null() {
        let layout = Layout::from_size_align(size, 1).unwrap();
        unsafe { dealloc(ptr, layout) }
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
    let string_from_host = str::from_utf8(&slice).unwrap();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
