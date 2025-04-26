use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
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
    pub arguments: Option<i32>,
}

impl LogEntry {
    
    pub fn empty() -> Self {
        LogEntry {
            index: 0,
            term: 0,
            operation: None,
            requester: None,
            arguments: None,
        }
    }

    pub fn nop(index: i32, term: i32, requester: i32) -> Self {
        LogEntry {
            index,
            term,
            operation: Some(Operation::Nop),
            requester: Some(requester),
            arguments: None,
        }
    }

    pub fn enqueue(index: i32, term: i32, requester: i32, arguments: i32) -> Self {
        LogEntry {
            index,
            term,
            operation: Some(Operation::Enqueue),
            requester: Some(requester),
            arguments: Some(arguments),
        }
    }

    pub fn dequeue(index: i32, term: i32, requester: i32) -> Self {
        LogEntry {
            index,
            term,
            operation: Some(Operation::Dequeue),
            requester: Some(requester),
            arguments: None,
        }
    }
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

impl AppendEntryRequest {
    pub fn new(
        term: i32,
        leader_id: i32,
        prev_log_index: i32,
        prev_log_term: i32,
        entries: Vec<LogEntry>,
        leader_commit: i32,
    ) -> Self {
        AppendEntryRequest {
            term,
            leader_id,
            prev_log_index,
            prev_log_term,
            entries,
            leader_commit,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AppendEntryResponse {
    pub term: i32,
    pub log_index: i32, // Used to relate request with response
    pub success: bool,
}

impl AppendEntryResponse {
    /// Create a new AppendEntryResponse
    pub fn new(term: i32, log_index: i32, success: bool) -> Self {
        AppendEntryResponse {
            term,
            log_index,
            success,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ClientEnqueueRequest {
    pub val: i32,
    pub client_id: i32,
}

impl ClientEnqueueRequest {
    /// Create a new ClientEnqueueRequest
    pub fn new(val: i32, client_id: i32) -> Self {
        ClientEnqueueRequest { val, client_id }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ClientEnqueueResponse {
    pub val: i32,
    pub client_id: i32,
    pub log_index: i32,
}
impl ClientEnqueueResponse {
    /// Create a new ClientEnqueueResponse
    pub fn new(val: i32, client_id: i32, log_index: i32) -> Self {
        ClientEnqueueResponse {
            val,
            client_id,
            log_index,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Events {
    ClientEnqueueResponse(ClientEnqueueResponse),
    LogEntry(LogEntry),
    AppendEntryRequest(AppendEntryRequest),
    AppendEntryResponse(AppendEntryResponse),
    ClientEnqueueRequest(ClientEnqueueRequest),
}

impl Events {
    /// Create a new event
    pub fn new_log_entry(entry: LogEntry) -> Self {
        Events::LogEntry(entry)
    }

    pub fn new_append_entry_request(request: AppendEntryRequest) -> Self {
        Events::AppendEntryRequest(request)
    }

    pub fn new_append_entry_response(response: AppendEntryResponse) -> Self {
        Events::AppendEntryResponse(response)
    }
}