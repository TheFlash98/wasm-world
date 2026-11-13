use std::alloc::{alloc, dealloc, Layout};
use std::sync::atomic::{AtomicI32, Ordering};

mod client;
mod host;
mod messages;
mod raft;

use client::ClientState;
use messages::{Events, Operation};
use raft::InstanceState;

// Kept apart from INSTANCE because the host reads it (through `get_instance`)
// while a call into INSTANCE is still running.
static INSTANCE_ID: AtomicI32 = AtomicI32::new(-1);
static mut INSTANCE: Option<Node> = None;

#[repr(C)]
pub struct WasmMemory {
    ptr: *mut u8,
    size: usize,
}

impl WasmMemory {
    pub extern "C" fn new(size: usize) -> Self {
        // Zero-sized allocations are undefined behaviour, so always take a byte.
        let layout = Layout::from_size_align(size.max(1), 1).unwrap();
        let ptr = unsafe { alloc(layout) };
        WasmMemory { ptr, size }
    }
}

impl Drop for WasmMemory {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            let layout = Layout::from_size_align(self.size.max(1), 1).unwrap();
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
    // Called by the host when a message arrives.
    fn receive(&mut self, sender: i32, event: Events);
    // Called by the host when a timer from `host::set_timer` fires.
    fn on_timer(&mut self, token: i32);
    fn status(&self) -> String;
}

// Ids 1..=num_servers are Raft servers. The host starts one more instance
// after them to act as the client.
enum Node {
    Server(InstanceState),
    Client(ClientState),
}

impl Node {
    fn actor(&mut self) -> &mut dyn Actor {
        match self {
            Node::Server(server) => server,
            Node::Client(client) => client,
        }
    }
}

fn with_node<R>(f: impl FnOnce(&mut Node) -> R) -> Option<R> {
    unsafe {
        let raw_ptr = &raw mut INSTANCE;
        (*raw_ptr).as_mut().map(f)
    }
}

#[no_mangle]
pub extern "C" fn start(id: i32, num_servers: i32) {
    INSTANCE_ID.store(id, Ordering::Relaxed);
    let node = if id <= num_servers {
        Node::Server(InstanceState::new(id, num_servers))
    } else {
        Node::Client(ClientState::new(id, num_servers))
    };
    unsafe {
        INSTANCE = Some(node);
    }
    with_node(|node| node.actor().init());
}

#[no_mangle]
pub extern "C" fn get_instance() -> i32 {
    INSTANCE_ID.load(Ordering::Relaxed)
}

#[no_mangle]
pub extern "C" fn receive(sender: i32, ptr: i32, len: i32) {
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let event = std::str::from_utf8(slice)
        .map_err(|err| err.to_string())
        .and_then(|message| ron::from_str::<Events>(message).map_err(|err| err.to_string()));
    match event {
        Ok(event) => {
            with_node(|node| node.actor().receive(sender, event));
        }
        Err(err) => host::log(&format!("dropping unreadable message from {}: {}", sender, err)),
    }
}

#[no_mangle]
pub extern "C" fn on_timer(token: i32) {
    with_node(|node| node.actor().on_timer(token));
}

#[no_mangle]
pub extern "C" fn status() {
    if let Some(status) = with_node(|node| node.actor().status()) {
        host::log(&status);
    }
}

#[no_mangle]
pub extern "C" fn restart() {
    with_node(|node| {
        if let Node::Server(server) = node {
            server.restart();
        }
    });
}

#[no_mangle]
pub extern "C" fn leader_term() -> i32 {
    with_node(|node| match node {
        Node::Server(server) => server.leader_term(),
        Node::Client(_) => -1,
    })
    .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn client_enqueue(value: i32) {
    with_client(|client| client.submit(Operation::Enqueue, Some(value)));
}

#[no_mangle]
pub extern "C" fn client_dequeue() {
    with_client(|client| client.submit(Operation::Dequeue, None));
}

fn with_client(f: impl FnOnce(&mut ClientState)) {
    with_node(|node| match node {
        Node::Client(client) => f(client),
        Node::Server(_) => host::log("only the client can make requests"),
    });
}
