// Calls out to the wasm host. Native builds (unit tests) record the calls
// in `mock` instead, so the Raft logic can be tested without a host.
use crate::messages::Events;

pub use imp::{log, random_between, report_applied, report_leader, send_bytes};

/// Sends a message to another server.
pub fn send(target_id: i32, event: &Events) {
    send_bytes(target_id, ron::to_string(event).unwrap().as_bytes());
}

#[cfg(target_arch = "wasm32")]
use wasm as imp;
#[cfg(not(target_arch = "wasm32"))]
use mock as imp;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use crate::component as bindings;

    pub fn log(message: &str) {
        bindings::log_str(message);
    }

    pub fn send_bytes(target_id: i32, data: &[u8]) {
        bindings::send_message(target_id, data);
    }

    pub fn random_between(min: i32, max: i32) -> i32 {
        bindings::random(min, max)
    }

    pub fn report_leader(term: i32) {
        bindings::report_leader(term);
    }

    pub fn report_applied(index: i32, term: i32) {
        bindings::report_applied(index, term);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub mod mock {
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct Calls {
        pub sent: Vec<(i32, Vec<u8>)>,
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

    pub fn log(message: &str) {
        println!("{}", message);
    }

    pub fn send_bytes(target_id: i32, data: &[u8]) {
        CALLS.with(|calls| calls.borrow_mut().sent.push((target_id, data.to_vec())));
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
