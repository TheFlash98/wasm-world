use std::collections::VecDeque;

use crate::host::{self, debug, log};
use crate::messages::{ClientRequest, ClientResult, Events, Operation};
use crate::Actor;

// Long enough to ride out an election before giving up on a server.
const RETRY_TIMEOUT_MS: i32 = 2000;

pub struct ClientState {
    id: i32,
    servers: Vec<i32>,
    // Where requests go. Follows the hints in `NotLeader` replies.
    leader_guess: i32,
    next_request_id: i32,
    waiting: VecDeque<(Operation, Option<i32>)>,
    // One request at a time, so they're applied in the order they were made.
    pending: Option<ClientRequest>,
    retry_timer: i32,
    next_timer_token: i32,
}

impl Actor for ClientState {
    fn init(&mut self) {
        log(&format!("client up, servers {:?}", self.servers));
    }

    fn receive(&mut self, sender: i32, event: Events) {
        let Events::ClientResponse(response) = event else {
            debug(&format!("ignoring {:?} from {}", event, sender));
            return;
        };
        if self.pending.as_ref().map(|request| request.request_id) != Some(response.request_id) {
            return; // a late answer to a request we've already finished
        }
        match response.result {
            ClientResult::NotLeader(Some(leader)) if leader != sender => {
                debug(&format!("node {} says node {} is leader", sender, leader));
                self.leader_guess = leader;
                self.send_pending();
            }
            ClientResult::NotLeader(_) => {
                // Nobody knows the leader yet, probably mid-election. Let the
                // retry timer try the next server.
                debug(&format!("node {} doesn't know the leader", sender));
                self.try_next_server();
            }
            result => {
                let outcome = match result {
                    ClientResult::Enqueued(value) => format!("enqueued {}", value),
                    ClientResult::Dequeued(Some(value)) => format!("dequeued {}", value),
                    _ => "dequeue found the queue empty".to_string(),
                };
                log(&format!("request #{} done by leader node {}: {}", response.request_id, sender, outcome));
                self.leader_guess = sender;
                self.pending = None;
                self.send_next();
            }
        }
    }

    fn on_timer(&mut self, token: i32) {
        if token != self.retry_timer || self.pending.is_none() {
            return;
        }
        let silent = self.leader_guess;
        self.try_next_server();
        log(&format!("no answer from node {}, retrying with node {}", silent, self.leader_guess));
        self.send_pending();
    }

    fn status(&self) -> String {
        let pending = self.pending.as_ref().map(|request| format!("#{} {}", request.request_id, request.describe()));
        format!(
            "client pending={:?} waiting={} leader_guess={}",
            pending,
            self.waiting.len(),
            self.leader_guess,
        )
    }
}

impl ClientState {
    pub fn new(id: i32, num_servers: i32) -> Self {
        ClientState {
            id,
            servers: (1..=num_servers).collect(),
            leader_guess: 1,
            next_request_id: 0,
            waiting: VecDeque::new(),
            pending: None,
            retry_timer: 0,
            next_timer_token: 0,
        }
    }

    pub fn submit(&mut self, operation: Operation, value: Option<i32>) {
        self.waiting.push_back((operation, value));
        if self.pending.is_none() {
            self.send_next();
        }
    }

    fn send_next(&mut self) {
        let Some((operation, value)) = self.waiting.pop_front() else {
            return;
        };
        self.next_request_id += 1;
        let request = ClientRequest {
            client_id: self.id,
            request_id: self.next_request_id,
            operation,
            value,
        };
        log(&format!("request #{}: {}", request.request_id, request.describe()));
        self.pending = Some(request);
        self.send_pending();
    }

    fn send_pending(&mut self) {
        let Some(request) = &self.pending else {
            return;
        };
        debug(&format!("sending request #{} to node {}", request.request_id, self.leader_guess));
        host::send(self.leader_guess, &Events::ClientRequest(request.clone()));
        self.next_timer_token += 1;
        self.retry_timer = self.next_timer_token;
        host::set_timer(self.retry_timer, RETRY_TIMEOUT_MS);
    }

    fn try_next_server(&mut self) {
        let position = self.servers.iter().position(|&id| id == self.leader_guess).unwrap_or(0);
        self.leader_guess = self.servers[(position + 1) % self.servers.len()];
    }
}
