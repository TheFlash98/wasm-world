use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum Operation {
    Nop,
    Enqueue,
    Dequeue,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LogEntry {
    pub index: i32,
    pub term: i32,
    pub operation: Option<Operation>,
    pub request_id: Option<i32>,
    pub arguments: Option<i32>,
}

impl LogEntry {
    /// Placeholder at index 0, so real entries start at 1 like in the paper.
    pub fn empty() -> Self {
        LogEntry {
            index: 0,
            term: 0,
            operation: None,
            request_id: None,
            arguments: None,
        }
    }

    /// Appended by a new leader so it can commit entries from earlier terms (§8).
    pub fn nop(index: i32, term: i32) -> Self {
        LogEntry {
            index,
            term,
            operation: Some(Operation::Nop),
            request_id: None,
            arguments: None,
        }
    }

    pub fn client_request(index: i32, term: i32, request: &ClientRequest) -> Self {
        LogEntry {
            index,
            term,
            operation: Some(request.operation),
            request_id: Some(request.request_id),
            arguments: request.value,
        }
    }

    pub fn describe(&self) -> String {
        match self.operation {
            Some(Operation::Enqueue) => format!("enqueue {}", self.arguments.unwrap_or_default()),
            Some(Operation::Dequeue) => "dequeue".to_string(),
            Some(Operation::Nop) => "no-op".to_string(),
            None => "empty".to_string(),
        }
    }
}

// Messages between servers. Only our own servers read these, so any format
// works; this one is RON, a Rust serialization format.

#[derive(Serialize, Deserialize, Debug)]
pub struct VoteRequest {
    pub term: i32,
    pub candidate_id: i32,
    pub last_log_index: i32,
    pub last_log_term: i32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct VoteResponse {
    pub term: i32,
    pub vote_granted: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AppendEntryRequest {
    pub term: i32,
    pub leader_id: i32,
    pub prev_log_index: i32,
    pub prev_log_term: i32,
    pub entries: Vec<LogEntry>,
    pub leader_commit: i32,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AppendEntryResponse {
    pub term: i32,
    // On success, the last index now known to match the leader's log.
    // On failure, the index the leader should retry from (minus one).
    pub log_index: i32,
    pub success: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Events {
    VoteRequest(VoteRequest),
    VoteResponse(VoteResponse),
    AppendEntryRequest(AppendEntryRequest),
    AppendEntryResponse(AppendEntryResponse),
}

impl Events {
    pub fn term(&self) -> i32 {
        match self {
            Events::VoteRequest(req) => req.term,
            Events::VoteResponse(resp) => resp.term,
            Events::AppendEntryRequest(req) => req.term,
            Events::AppendEntryResponse(resp) => resp.term,
        }
    }
}

// The client protocol. The host is the client, and it speaks plain text
// (see STUDENTS.md) so that servers in any language can parse it.

#[derive(Debug, Clone, PartialEq)]
pub struct ClientRequest {
    // The host resends the same id when it retries, so a server can tell a
    // retry from a new request and not apply it twice (§8).
    pub request_id: i32,
    pub operation: Operation,
    pub value: Option<i32>,
}

impl ClientRequest {
    /// Parses `ENQ <request_id> <value>` or `DEQ <request_id>`.
    pub fn parse(data: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(data).ok()?;
        match text.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["ENQ", request_id, value] => Some(ClientRequest {
                request_id: request_id.parse().ok()?,
                operation: Operation::Enqueue,
                value: Some(value.parse().ok()?),
            }),
            ["DEQ", request_id] => Some(ClientRequest {
                request_id: request_id.parse().ok()?,
                operation: Operation::Dequeue,
                value: None,
            }),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self.operation {
            Operation::Enqueue => format!("enqueue {}", self.value.unwrap_or_default()),
            Operation::Dequeue => "dequeue".to_string(),
            Operation::Nop => "no-op".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientResult {
    Enqueued,
    Dequeued(Option<i32>),
}

impl ClientResult {
    /// The reply line: `OK <id>`, `OK <id> <value>` or `EMPTY <id>`.
    pub fn reply(&self, request_id: i32) -> String {
        match self {
            ClientResult::Enqueued => format!("OK {}", request_id),
            ClientResult::Dequeued(Some(value)) => format!("OK {} {}", request_id, value),
            ClientResult::Dequeued(None) => format!("EMPTY {}", request_id),
        }
    }
}

/// The reply from a server that isn't the leader. `leader` is 0 if unknown.
pub fn not_leader_reply(request_id: i32, leader: Option<i32>) -> String {
    format!("NOT_LEADER {} {}", request_id, leader.unwrap_or(0))
}
