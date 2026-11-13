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
    pub requester: Option<i32>,
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
            requester: None,
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
            requester: None,
            request_id: None,
            arguments: None,
        }
    }

    pub fn client_request(index: i32, term: i32, request: &ClientRequest) -> Self {
        LogEntry {
            index,
            term,
            operation: Some(request.operation),
            requester: Some(request.client_id),
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClientRequest {
    pub client_id: i32,
    // Lets servers recognise a retried request and not apply it twice (§8).
    pub request_id: i32,
    pub operation: Operation,
    pub value: Option<i32>,
}

impl ClientRequest {
    pub fn describe(&self) -> String {
        match self.operation {
            Operation::Enqueue => format!("enqueue {}", self.value.unwrap_or_default()),
            Operation::Dequeue => "dequeue".to_string(),
            Operation::Nop => "no-op".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ClientResult {
    Enqueued(i32),
    Dequeued(Option<i32>),
    // Sent by followers and candidates, with the leader if they know it.
    NotLeader(Option<i32>),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ClientResponse {
    pub client_id: i32,
    pub request_id: i32,
    pub result: ClientResult,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Events {
    VoteRequest(VoteRequest),
    VoteResponse(VoteResponse),
    AppendEntryRequest(AppendEntryRequest),
    AppendEntryResponse(AppendEntryResponse),
    ClientRequest(ClientRequest),
    ClientResponse(ClientResponse),
}

impl Events {
    /// The sender's term for messages between servers; client messages have none.
    pub fn term(&self) -> Option<i32> {
        match self {
            Events::VoteRequest(req) => Some(req.term),
            Events::VoteResponse(resp) => Some(resp.term),
            Events::AppendEntryRequest(req) => Some(req.term),
            Events::AppendEntryResponse(resp) => Some(resp.term),
            Events::ClientRequest(_) | Events::ClientResponse(_) => None,
        }
    }
}
