use std::slice;
use std::str;

extern "C" {
    fn double(x: i32) -> i32;
    fn log_str(ptr: i32, len: i32);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
