// Calls out to the wasm host. Native builds (unit tests) record the calls
// in `mock` instead, so the Raft logic can be tested without a host.
use crate::messages::Events;

const LOG_INFO: i32 = 0;
const LOG_DEBUG: i32 = 1;

pub use imp::{random_between, report_applied, report_leader, set_timer};

/// Always printed by the host.
pub fn log(msg: &str) {
    imp::log_at(LOG_INFO, msg);
}

/// Printed only when the host runs with `verbose on`.
pub fn debug(msg: &str) {
    imp::log_at(LOG_DEBUG, msg);
}

pub fn send(target_id: i32, event: &Events) {
    imp::send_str(target_id, ron::to_string(event).unwrap());
}

#[cfg(target_arch = "wasm32")]
use wasm as imp;
#[cfg(not(target_arch = "wasm32"))]
use mock as imp;

#[cfg(target_arch = "wasm32")]
mod wasm {
    mod ffi {
        #[link(wasm_import_module = "env")]
        extern "C" {
            pub fn log_str(level: i32, ptr: i32, len: i32);
            pub fn send_message(target_id: i32, ptr: i32, len: i32);
            pub fn set_timer(token: i32, delay_ms: i32);
            pub fn random_between(min: i32, max: i32) -> i32;
            pub fn report_leader(term: i32);
            pub fn report_applied(index: i32, term: i32);
        }
    }

    pub fn log_at(level: i32, msg: &str) {
        unsafe { ffi::log_str(level, msg.as_ptr() as i32, msg.len() as i32) }
    }

    pub fn send_str(target_id: i32, msg: String) {
        unsafe { ffi::send_message(target_id, msg.as_ptr() as i32, msg.len() as i32) }
    }

    /// The host calls `on_timer(token)` after `delay_ms`.
    pub fn set_timer(token: i32, delay_ms: i32) {
        unsafe { ffi::set_timer(token, delay_ms) }
    }

    pub fn random_between(min: i32, max: i32) -> i32 {
        unsafe { ffi::random_between(min, max) }
    }

    /// Lets the host check that each term has at most one leader.
    pub fn report_leader(term: i32) {
        unsafe { ffi::report_leader(term) }
    }

    /// Lets the host check that every node applies the same entry at each index.
    pub fn report_applied(index: i32, term: i32) {
        unsafe { ffi::report_applied(index, term) }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub mod mock {
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct Calls {
        pub sent: Vec<(i32, String)>,
        pub timers: Vec<(i32, i32)>,
        pub leaders: Vec<i32>,
        pub applied: Vec<(i32, i32)>,
    }

    thread_local! {
        static CALLS: RefCell<Calls> = RefCell::new(Calls::default());
    }

    /// Returns everything recorded since the last call.
    pub fn take() -> Calls {
        CALLS.with(|calls| calls.take())
    }

    pub fn log_at(_level: i32, msg: &str) {
        println!("{}", msg);
    }

    pub fn send_str(target_id: i32, msg: String) {
        CALLS.with(|calls| calls.borrow_mut().sent.push((target_id, msg)));
    }

    pub fn set_timer(token: i32, delay_ms: i32) {
        CALLS.with(|calls| calls.borrow_mut().timers.push((token, delay_ms)));
    }

    pub fn random_between(min: i32, _max: i32) -> i32 {
        min
    }

    pub fn report_leader(term: i32) {
        CALLS.with(|calls| calls.borrow_mut().leaders.push(term));
    }

    pub fn report_applied(index: i32, term: i32) {
        CALLS.with(|calls| calls.borrow_mut().applied.push((index, term)));
    }
}
