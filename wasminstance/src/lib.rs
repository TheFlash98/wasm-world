use std::slice;
use std::str;
use std::alloc::{alloc, dealloc, Layout};
use serde::{Serialize, Deserialize};
use std::collections::{HashMap, VecDeque};

mod messages;
use messages::{LogEntry, Events};

static mut INSTANCE: Option<InstanceState> = None;

extern "C" {
    fn log_str(ptr: i32, len: i32);
    fn send_message(target_id: i32, ptr: i32, len: i32);
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
    fn receive(&mut self, sender: i32, ptr: i32, len: i32);
}

pub struct InstanceState {
    id: i32,
    
    view: Vec<i32>,
    current_leader: i32,
    // Time before starting an election.
    min_election_timeout: i32,
    max_election_timeout: i32,
    election_timer: i32,
    
    heartbeat_timeout: i32,
    heartbeat_timer: i32,

    current_term: i32,
    voted_for: i32,

    log: Vec<LogEntry>,

    commit_index: i32,
    last_applied: i32,

    is_leader: bool,
    is_candidate: bool,
    next_index: HashMap<i32, i32>,
    match_index: HashMap<i32, i32>,

    queue: VecDeque<i32>,
}

impl Default for InstanceState {
    fn default() -> Self {
        InstanceState {
            id: -1,
            view: vec![1, 2, 3],
            current_leader: 1,
            min_election_timeout: 0,
            max_election_timeout: 0,
            election_timer: 0,
            heartbeat_timeout: 0,
            heartbeat_timer: 0,
            current_term: 1,
            voted_for: -1,
            log: vec![],
            commit_index: 0,
            last_applied: -1,
            is_leader: false,
            is_candidate: false,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            queue: VecDeque::new(),
        }
    }
}

impl Actor for InstanceState {
    fn init(&mut self) {
        // Initialize the instance
        log(&format!("Hello from instance state struct with id: {}", self.id));
    }
    // This function will be called when a message is received
    // It will be called from the host
    fn receive(&mut self, sender: i32, ptr: i32, len: i32) {
        let slice = unsafe { std::slice::from_raw_parts(ptr as _, len as _) };
        let message = std::str::from_utf8(slice).unwrap();
        // let message_obj: Events = ron::from_str(message).unwrap();
        let event = ron::from_str::<Events>(message)
            .expect("Failed to raw message to event");
        if self.is_leader {
            self.leader_receive(sender, event);
        } else if self.is_candidate {
            self.candidate_receive(sender, event);
        } else {
            self.follower_receive(sender, event);
        }
    }
}

impl InstanceState {
    // Raft state machine implementation
    
    fn leader_receive(&mut self, sender: i32, event: Events) {
        // Handle messages when the instance is a leader
        match event {
            // Leader handle enqueue request from client
            Events::ClientEnqueueRequest(req) => {
                log(&format!("Leader {} got ClientEnqueueRequest: {:?}", self.id, req));
                let log_entry = LogEntry::enqueue(
                    self.current_term,
                    self.id,
                    req.client_id,
                    req.val,
                );
                let append_entry_req = messages::Events::AppendEntryRequest(
                    messages::AppendEntryRequest::new(
                        self.current_term,
                        self.current_leader,
                        self.get_last_log_index(),
                        self.get_last_log_term(),
                        vec![log_entry.clone()],
                        self.commit_index,
                    ),
                );
                self.log.push(log_entry);
                self.broadcast_to_others(append_entry_req);
            }
            // Leader handle append entry response from follower
            Events::AppendEntryResponse(req) => {
                log(&format!("Leader {} got AppendEntryResponse: {:?}", self.id, req));
                if req.term > self.current_term {
                    self.current_term = req.term;
                    self.is_leader = false;
                    self.is_candidate = false;
                } else {
                    if req.success {
                        self.next_index.insert(sender, req.log_index + 1);
                        self.match_index.insert(sender, req.log_index);
                        let replicated_indices_len = self.match_index.values()
                                                    .filter(|&&idx| idx >= req.log_index)
                                                    .count();
                        log(&format!("Leader replicated_indices_len {}", replicated_indices_len));
                        if replicated_indices_len > self.view.len() / 2 {
                            let commit_entry = self.get_log_entry(req.log_index);
                            let update_commit = commit_entry.is_some() && 
                                                commit_entry.unwrap().term == self.current_term &&
                                                commit_entry.unwrap().index > self.commit_index;
                            log(&format!("Leader will commit now: {:?}", commit_entry));
                            if update_commit {
                                if req.log_index > self.last_applied {
                                    self.last_applied = self.last_applied + 1;
                                    let mut client_response = self.commit_log_index(self.last_applied);
                                    if self.last_applied < req.log_index {
                                        self.last_applied = self.last_applied + 1;
                                        client_response = self.commit_log_index(self.last_applied);
                                    }
                                    if let Some(response) = client_response {
                                        let client_id = match response {
                                            messages::Events::ClientEnqueueResponse(ref client_response) => {
                                                client_response.client_id
                                            }
                                            _ => -1,
                                        };
                                        let response_str = ron::to_string(&response).unwrap();
                                        send(client_id, &response_str);
                                    }
                                }
                            }
                        } else {
                            log(&format!("Leader not commiting yet"));
                        }
                    } 
                }
            }
            _ => {
                log(&format!("Leader {} got unknown event: {:?}", self.id, event));
            }
        }
    }

    fn candidate_receive(&mut self, sender: i32, event: Events) {
        // Handle messages when the instance is a candidate
        match event {
            Events::AppendEntryRequest(req) => {
                let s = format!("Candidate {} got AppendEntryRequest: {:?}", self.id, req);
                log(&s);
            }
            _ => {}
        }
    }

    fn follower_receive(&mut self, sender: i32, event: Events) {
        // Handle messages when the instance is a follower
        match event {
            Events::AppendEntryRequest(req) => {
                if req.term < self.current_term {
                    let s = format!("Follower {} got AppendEntryRequest with old term: {:?}", self.id, req);
                    log(&s);
                    let response = messages::Events::AppendEntryResponse(
                        messages::AppendEntryResponse::new(
                            self.current_term,
                            req.prev_log_index,
                            false,
                        ),
                    );
                    let response_str = ron::to_string(&response).unwrap();
                    send(sender, &response_str);
                } else {
                    self.current_term = req.term;
                    if self.current_leader != req.leader_id {
                        self.current_leader = req.leader_id;
                    }
                    
                    let entry_at_prev_log_index = self.get_log_entry(req.prev_log_index);
                    if req.prev_log_index == 0 && 
                        req.prev_log_term == 0 && 
                        req.entries.len() == 0 &&
                        entry_at_prev_log_index.is_none() {
                        log(&format!("Follower {} received first empty heartbeat", self.id));
                        let response = messages::Events::AppendEntryResponse(
                            messages::AppendEntryResponse::new(
                                self.current_term,
                                req.prev_log_index,
                                true,
                            ),
                        );
                        let response_str = ron::to_string(&response).unwrap();
                        send(sender, &response_str);
                    } else {
                        if req.prev_log_index > 0 {
                            if entry_at_prev_log_index.is_none() || 
                            entry_at_prev_log_index.unwrap().term != req.prev_log_term {
                                log(&format!("Follower {} got AppendEntryRequest at wrong log index: {:?}", self.id, req));
                                let response = messages::Events::AppendEntryResponse(
                                    messages::AppendEntryResponse::new(
                                        self.current_term,
                                        req.prev_log_index,
                                        false,
                                    ),
                                );
                                let response_str = ron::to_string(&response).unwrap();
                                send(sender, &response_str);
                                return;
                            }
                        }
                    }

                    let append_index = req.prev_log_index + 1;
                    if self.logged(append_index) {
                        log(&format!("Follower {} already logged at index: {:?}", self.id, append_index));
                        let entry = self.get_log_entry(append_index).unwrap();
                        if req.entries.len() == 0 {
                            self.truncate_log_at_index(append_index);
                        } else {
                            if entry.term != req.entries[0].term {
                                self.truncate_log_at_index(append_index);
                            }
                        }
                    }

                    log(&format!("Follower at appending log entries"));
                    self.add_log_entries(req.entries);

                    if req.leader_commit > self.commit_index {
                        let commit_index = std::cmp::min(req.leader_commit, self.get_last_log_index());
                        if commit_index > self.last_applied {
                            self.last_applied += 1;
                            let _ = self.commit_log_index(self.last_applied);
                            let response = messages::Events::AppendEntryResponse(
                                messages::AppendEntryResponse::new(
                                    self.current_term,
                                    self.get_last_log_index(),
                                    true,
                                ),
                            );
                            let response_str = ron::to_string(&response).unwrap();
                            send(sender, &response_str);
                            self.reset_election_timer();
                        }
                    } else {
                        let response = messages::Events::AppendEntryResponse(
                            messages::AppendEntryResponse::new(
                                self.current_term,
                                self.get_last_log_index(),
                                true,
                            ),
                        );
                        let response_str = ron::to_string(&response).unwrap();
                        send(sender, &response_str);
                    }
                }
            }
            _ => {
                log(&format!("Follower {} got unknown event: {:?}", self.id, event));
            }
        }
    }

    fn get_last_log_index(&self) -> i32 {
        if self.log.is_empty() {
            return 0;
        }
        self.log.len() as i32 - 1
    }

    fn get_last_log_term(&self) -> i32 {
        if self.log.is_empty() {
            return 0;
        }
        self.log.last().unwrap().term
    }

    fn get_log_entry(&self, index: i32) -> Option<&LogEntry> {
        if index < 0 || index >= self.log.len() as i32 {
            return None;
        }
        Some(&self.log[index as usize])
    }

    fn logged(&self, index: i32) -> bool {
        index >= 0 && index < self.log.len() as i32
    }

    fn truncate_log_at_index(&mut self, index: i32) {
        if index < 0 || index >= self.log.len() as i32 {
            return;
        }
        self.log.truncate(index as usize);
    }

    fn add_log_entries(&mut self, entries: Vec<LogEntry>) {
        for entry in entries {
            self.log.push(entry);
        }
    }

    fn commit_log_index(&mut self, index: i32) -> Option<messages::Events> {
        if index < 0 || index >= self.log.len() as i32 {
            return None;
        }
        let entry = &self.log[index as usize];
        match entry.operation {
            Some(messages::Operation::Enqueue) => {
                let value = entry.arguments.unwrap();
                log(&format!("Id {} Committing log entry with value: {} enqueue", self.id, value));
                self.queue.push_back(value);
                let response = messages::Events::ClientEnqueueResponse(
                    messages::ClientEnqueueResponse::new(
                        value,
                        entry.requester.unwrap(),
                        index,
                    ),
                );
                return Some(response); 
            }
            Some(messages::Operation::Dequeue) => {
                log(&format!("Committing log entry with dequeue operation"));
                let _ = self.queue.pop_front();
                return None;
            }
            _ => return None,
        }
    }

    fn reset_election_timer(&mut self) {
        // Reset the election timer
        return;
    }

    fn broadcast_to_others(&self, event: Events) {
        // Broadcast the event to all other instances
        for &id in &self.view {
            if id != self.id {
                let s = ron::to_string(&event).unwrap();
                send(id, &s);
            }
        }
    }
}

#[no_mangle]
pub extern fn start(id: i32) {
    // Create a main function that runs once every instance comes up. 
    // Every instance has a main function as well as an init function
    log(&format!("Start func called"));
    init(id);
}


fn init(id: i32) {
    // The init function will call the “structs” init function 
    // that evaluates the actual code needed
    let instance = InstanceState {
        id,
        ..Default::default()
    };
    unsafe {
        INSTANCE = Some(instance);
        let raw_ptr = &raw mut INSTANCE;
        if let Some(instance) = &mut *raw_ptr {
            instance.init();
        }
    }
}

#[no_mangle]
pub extern "C" fn get_instance() -> i32 {
    unsafe {
        let raw_ptr = &raw const INSTANCE;
        if let Some(instance) = &*raw_ptr {
            return instance.id;
        }
    }
    -1
}

#[no_mangle]
pub extern "C" fn receive(sender: i32, ptr: i32, len: i32) {
    unsafe {
        let raw_ptr = &raw mut INSTANCE;
        if let Some(instance) = &mut *raw_ptr {
            instance.receive(sender, ptr, len);
        }
    }
}

#[no_mangle]
pub extern "C" fn client_enqueue(value: i32, leader: i32, client_id: i32) {
    let client_enqueue_req = messages::Events::ClientEnqueueRequest(
        messages::ClientEnqueueRequest::new(
        value,
        client_id
    ));
    let client_enqueue_req_str = ron::to_string(&client_enqueue_req).unwrap();
    log(&client_enqueue_req_str);
    send(leader, &client_enqueue_req_str);
}

#[no_mangle]
pub extern "C" fn make_leader_host() {
    unsafe {
        let raw_ptr = &raw mut INSTANCE;
        if let Some(instance) = &mut *raw_ptr {
            instance.is_leader = true;
        }
    }
}

fn send(target_id: i32, msg: &str) {
    unsafe {
        send_message(target_id, msg.as_ptr() as i32, msg.len() as i32);
    }
}

fn log(msg: &str) {
    unsafe {
        log_str(msg.as_ptr() as i32, msg.len() as i32);
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
