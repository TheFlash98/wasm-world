use std::collections::{HashMap, HashSet, VecDeque};

use crate::host::{self, log};
use crate::messages::{
    not_leader_reply, AppendEntryRequest, AppendEntryResponse, ClientRequest, ClientResult, Events,
    LogEntry, Operation, VoteRequest, VoteResponse,
};

// The host plays the client, at id 0.
const CLIENT_ID: i32 = 0;

// The host delays each message by 10-150 ms by default. Elections only work
// if a round trip is well under the election timeout (§5.6), and the heartbeat
// has to arrive before followers time out.
const HEARTBEAT_TIMEOUT_MS: i32 = 300;
const MIN_ELECTION_TIMEOUT_MS: i32 = 1500;
const MAX_ELECTION_TIMEOUT_MS: i32 = 3000;
const MAX_ENTRIES_PER_REQUEST: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

pub struct InstanceState {
    id: i32,
    // Every server in the cluster, including this one.
    view: Vec<i32>,
    role: Role,
    current_leader: Option<i32>,

    min_election_timeout: i32,
    max_election_timeout: i32,
    heartbeat_timeout: i32,
    // Times from the host's clock, which only moves forward in `tick`.
    now: i32,
    election_deadline: i32,
    heartbeat_deadline: i32,

    // Persistent state (Figure 2). There's no disk: it stays in wasm memory,
    // which survives `kill`, so `restart` behaves as if it had been saved.
    current_term: i32,
    voted_for: Option<i32>,
    // log[0] is a sentinel, so the first real entry has index 1.
    log: Vec<LogEntry>,

    commit_index: i32,
    last_applied: i32,
    votes_received: HashSet<i32>,
    // Leader only, reset after every election.
    next_index: HashMap<i32, i32>,
    match_index: HashMap<i32, i32>,

    // The replicated state machine: the queue, plus the client's latest
    // request and result so a retried request isn't applied twice (§8).
    queue: VecDeque<i32>,
    last_request: Option<(i32, ClientResult)>,
}

impl InstanceState {
    pub fn new(id: i32, num_servers: i32) -> Self {
        InstanceState {
            id,
            view: (1..=num_servers).collect(),
            role: Role::Follower,
            current_leader: None,
            min_election_timeout: MIN_ELECTION_TIMEOUT_MS,
            max_election_timeout: MAX_ELECTION_TIMEOUT_MS,
            heartbeat_timeout: HEARTBEAT_TIMEOUT_MS,
            now: 0,
            election_deadline: 0,
            heartbeat_deadline: 0,
            current_term: 0,
            voted_for: None,
            log: vec![LogEntry::empty()],
            commit_index: 0,
            last_applied: 0,
            votes_received: HashSet::new(),
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            queue: VecDeque::new(),
            last_request: None,
        }
    }

    pub fn start(&mut self) {
        log(&format!("server up as follower, cluster {:?}", self.view));
        self.reset_election_timer();
    }

    pub fn receive(&mut self, sender: i32, data: &[u8]) {
        if sender == CLIENT_ID {
            match ClientRequest::parse(data) {
                Some(request) => self.handle_client_request(request),
                None => log(&format!("ignoring bad client request {:?}", String::from_utf8_lossy(data))),
            }
            return;
        }
        let event = std::str::from_utf8(data)
            .map_err(|err| err.to_string())
            .and_then(|text| ron::from_str::<Events>(text).map_err(|err| err.to_string()));
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                log(&format!("dropping unreadable message from {}: {}", sender, err));
                return;
            }
        };
        // Rule for all servers: a newer term means we're out of date, so follow it.
        if event.term() > self.current_term {
            self.step_down(event.term());
        }
        match event {
            Events::VoteRequest(req) => self.handle_vote_request(sender, req),
            Events::VoteResponse(resp) => self.handle_vote_response(sender, resp),
            Events::AppendEntryRequest(req) => self.handle_append_entries(sender, req),
            Events::AppendEntryResponse(resp) => self.handle_append_entries_response(sender, resp),
        }
    }

    pub fn tick(&mut self, now_ms: i32) {
        self.now = now_ms;
        if self.role == Role::Leader {
            if self.now >= self.heartbeat_deadline {
                self.broadcast_append_entries();
                self.reset_heartbeat_timer();
            }
        } else if self.now >= self.election_deadline {
            self.start_election();
        }
    }

    /// Called when the host revives a killed server. Volatile state is lost in
    /// a crash; the term, vote and log were "persisted" and are kept.
    pub fn restart(&mut self, now_ms: i32) {
        self.now = now_ms;
        self.role = Role::Follower;
        self.current_leader = None;
        self.votes_received.clear();
        self.next_index.clear();
        self.match_index.clear();
        // The state machine is rebuilt by replaying the log once the leader
        // tells us how much of it is committed.
        self.commit_index = 0;
        self.last_applied = 0;
        self.queue.clear();
        self.last_request = None;
        log(&format!(
            "restarted as follower in term {} with {} log entries",
            self.current_term,
            self.last_log_index(),
        ));
        self.reset_election_timer();
    }

    pub fn status(&self) -> String {
        let terms: Vec<String> = self.log[1..].iter().map(|entry| entry.term.to_string()).collect();
        format!(
            "{:?} term={} leader={:?} voted_for={:?} commit={} applied={} queue={:?} log terms=[{}]",
            self.role,
            self.current_term,
            self.current_leader,
            self.voted_for,
            self.commit_index,
            self.last_applied,
            self.queue,
            terms.join(" "),
        )
    }

    // Leader election (§5.2)

    fn start_election(&mut self) {
        self.current_term += 1;
        self.role = Role::Candidate;
        self.voted_for = Some(self.id);
        self.current_leader = None;
        self.votes_received = HashSet::from([self.id]);
        log(&format!("election timeout, standing for term {}", self.current_term));
        self.reset_election_timer();

        if self.is_majority(self.votes_received.len()) {
            self.become_leader();
            return;
        }
        self.broadcast_to_others(&Events::VoteRequest(VoteRequest {
            term: self.current_term,
            candidate_id: self.id,
            last_log_index: self.last_log_index(),
            last_log_term: self.last_log_term(),
        }));
    }

    fn handle_vote_request(&mut self, sender: i32, req: VoteRequest) {
        // §5.4.1: only vote for a candidate whose log is at least as up to date
        // as ours, so a new leader always has every committed entry.
        let log_ok = (req.last_log_term, req.last_log_index) >= (self.last_log_term(), self.last_log_index());
        let free_to_vote = self.voted_for.is_none() || self.voted_for == Some(req.candidate_id);
        let vote_granted = req.term == self.current_term && free_to_vote && log_ok;

        if vote_granted {
            self.voted_for = Some(req.candidate_id);
            self.reset_election_timer();
            log(&format!("voted for node {} in term {}", req.candidate_id, self.current_term));
        } else {
            let reason = if req.term < self.current_term {
                format!("its term {} is out of date", req.term)
            } else if !free_to_vote {
                format!("already voted for node {}", self.voted_for.unwrap())
            } else {
                "its log is behind ours".to_string()
            };
            log(&format!("refused vote to node {} in term {}: {}", req.candidate_id, self.current_term, reason));
        }
        host::send(sender, &Events::VoteResponse(VoteResponse {
            term: self.current_term,
            vote_granted,
        }));
    }

    fn handle_vote_response(&mut self, sender: i32, resp: VoteResponse) {
        if self.role != Role::Candidate || resp.term != self.current_term || !resp.vote_granted {
            return;
        }
        self.votes_received.insert(sender);
        if self.is_majority(self.votes_received.len()) {
            self.become_leader();
        }
    }

    fn become_leader(&mut self) {
        self.role = Role::Leader;
        self.current_leader = Some(self.id);
        let mut voters: Vec<i32> = self.votes_received.iter().copied().collect();
        voters.sort();
        log(&format!("became LEADER for term {} with votes from {:?}", self.current_term, voters));
        host::report_leader(self.current_term);

        let next_index = self.last_log_index() + 1;
        for peer in self.peers() {
            self.next_index.insert(peer, next_index);
            self.match_index.insert(peer, 0);
        }
        self.log.push(LogEntry::nop(next_index, self.current_term));
        self.broadcast_append_entries();
        self.reset_heartbeat_timer();
        self.advance_commit_index();
    }

    /// Called whenever a message carries a newer term than ours.
    fn step_down(&mut self, term: i32) {
        match self.role {
            Role::Leader => {
                log(&format!("saw term {}, stepping down as leader of term {}", term, self.current_term));
                // Leaders don't watch for elections, so start doing that again.
                self.reset_election_timer();
            }
            Role::Candidate => log(&format!("saw term {}, giving up on term {}", term, self.current_term)),
            Role::Follower => {}
        }
        self.current_term = term;
        self.voted_for = None;
        self.role = Role::Follower;
        self.current_leader = None;
    }

    // Log replication (§5.3)

    fn handle_append_entries(&mut self, sender: i32, req: AppendEntryRequest) {
        if req.term < self.current_term {
            // From a deposed leader. Our term in the reply makes it step down.
            self.reply_append_entries(sender, false, self.last_log_index());
            return;
        }
        if self.role == Role::Leader {
            log(&format!("BUG: node {} also claims to lead term {}", req.leader_id, req.term));
            return;
        }
        if self.role == Role::Candidate {
            log(&format!("node {} won term {}, back to follower", req.leader_id, req.term));
            self.role = Role::Follower;
        }
        if self.current_leader != Some(req.leader_id) {
            self.current_leader = Some(req.leader_id);
            log(&format!("following leader node {} in term {}", req.leader_id, req.term));
        }
        self.reset_election_timer();

        // Our log must already hold the entry just before the new ones.
        if req.prev_log_index > self.last_log_index() {
            self.reply_append_entries(sender, false, self.last_log_index());
            return;
        }
        if self.term_at(req.prev_log_index) != req.prev_log_term {
            self.reply_append_entries(sender, false, req.prev_log_index - 1);
            return;
        }

        let match_index = req.prev_log_index + req.entries.len() as i32;
        for (offset, entry) in req.entries.into_iter().enumerate() {
            let index = req.prev_log_index + 1 + offset as i32;
            if index <= self.last_log_index() {
                if self.term_at(index) == entry.term {
                    // Already have it, e.g. from a request that arrived out of order.
                    continue;
                }
                // A conflicting entry: drop it and everything after it.
                self.log.truncate(index as usize);
            }
            self.log.push(entry);
        }

        // Only trust the leader's commit index up to what we know matches.
        let leader_commit = req.leader_commit.min(match_index);
        if leader_commit > self.commit_index {
            self.commit_index = leader_commit;
            self.apply_committed();
        }
        self.reply_append_entries(sender, true, match_index);
    }

    fn handle_append_entries_response(&mut self, sender: i32, resp: AppendEntryResponse) {
        // Replies can be from an earlier term or arrive out of order.
        if self.role != Role::Leader || resp.term != self.current_term {
            return;
        }
        let (Some(&next), Some(&matched)) = (self.next_index.get(&sender), self.match_index.get(&sender)) else {
            return;
        };
        if resp.success {
            let matched = matched.max(resp.log_index);
            self.match_index.insert(sender, matched);
            self.next_index.insert(sender, next.max(matched + 1));
            self.advance_commit_index();
        } else {
            // The follower is missing entries or has conflicting ones: back up and retry now.
            let next = (resp.log_index + 1).min(next).max(matched + 1);
            self.next_index.insert(sender, next);
            self.send_append_entries(sender);
        }
    }

    fn advance_commit_index(&mut self) {
        // §5.4.2: only count replicas of entries from our own term. Earlier
        // entries get committed along with them (Figure 8 shows why).
        let mut index = self.last_log_index();
        while index > self.commit_index && self.term_at(index) == self.current_term {
            let replicas = 1 + self.match_index.values().filter(|&&matched| matched >= index).count();
            if self.is_majority(replicas) {
                self.commit_index = index;
                self.apply_committed();
                return;
            }
            index -= 1;
        }
    }

    fn broadcast_append_entries(&self) {
        for peer in self.peers() {
            self.send_append_entries(peer);
        }
    }

    /// Doubles as the heartbeat when the follower is up to date.
    fn send_append_entries(&self, peer: i32) {
        let next = self.next_index.get(&peer).copied().unwrap_or(1).min(self.last_log_index() + 1);
        let prev_log_index = next - 1;
        let entries = self.log[next as usize..]
            .iter()
            .take(MAX_ENTRIES_PER_REQUEST)
            .cloned()
            .collect();
        host::send(peer, &Events::AppendEntryRequest(AppendEntryRequest {
            term: self.current_term,
            leader_id: self.id,
            prev_log_index,
            prev_log_term: self.term_at(prev_log_index),
            entries,
            leader_commit: self.commit_index,
        }));
    }

    fn reply_append_entries(&self, leader: i32, success: bool, log_index: i32) {
        host::send(leader, &Events::AppendEntryResponse(AppendEntryResponse {
            term: self.current_term,
            log_index,
            success,
        }));
    }

    // The client and the state machine (§8)

    fn handle_client_request(&mut self, req: ClientRequest) {
        if self.role != Role::Leader {
            host::send_bytes(CLIENT_ID, not_leader_reply(req.request_id, self.current_leader).as_bytes());
            return;
        }
        if let Some((last_request_id, result)) = &self.last_request {
            if *last_request_id == req.request_id {
                // Already applied; the client just didn't get our answer.
                host::send_bytes(CLIENT_ID, result.reply(req.request_id).as_bytes());
                return;
            }
        }
        let index = self.last_log_index() + 1;
        log(&format!("client asks to {}, appending at index {}", req.describe(), index));
        self.log.push(LogEntry::client_request(index, self.current_term, &req));
        self.broadcast_append_entries();
        self.advance_commit_index();
    }

    fn apply_committed(&mut self) {
        while self.last_applied < self.commit_index {
            self.last_applied += 1;
            let entry = self.log[self.last_applied as usize].clone();
            let outcome = self.apply(&entry);
            host::report_applied(self.last_applied, entry.term);
            if self.role == Role::Leader {
                log(&format!("committed #{} {}, queue now {:?}", self.last_applied, entry.describe(), self.queue));
                if let Some((request_id, result)) = outcome {
                    host::send_bytes(CLIENT_ID, result.reply(request_id).as_bytes());
                }
            }
        }
    }

    /// Runs one entry on the queue. Returns the request it answers, if any.
    fn apply(&mut self, entry: &LogEntry) -> Option<(i32, ClientResult)> {
        let request_id = entry.request_id?;
        if let Some((last_request_id, result)) = &self.last_request {
            if request_id <= *last_request_id {
                // The same request was appended twice (the client retried). Don't redo it.
                return (request_id == *last_request_id).then(|| (request_id, result.clone()));
            }
        }
        let result = match entry.operation {
            Some(Operation::Enqueue) => {
                self.queue.push_back(entry.arguments.unwrap_or_default());
                ClientResult::Enqueued
            }
            Some(Operation::Dequeue) => ClientResult::Dequeued(self.queue.pop_front()),
            _ => return None,
        };
        self.last_request = Some((request_id, result.clone()));
        Some((request_id, result))
    }

    // Helpers

    fn last_log_index(&self) -> i32 {
        self.log.len() as i32 - 1
    }

    fn last_log_term(&self) -> i32 {
        self.log.last().unwrap().term
    }

    fn term_at(&self, index: i32) -> i32 {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.log.get(index))
            .map_or(-1, |entry| entry.term)
    }

    fn is_majority(&self, count: usize) -> bool {
        count * 2 > self.view.len()
    }

    fn peers(&self) -> Vec<i32> {
        self.view.iter().copied().filter(|&id| id != self.id).collect()
    }

    fn broadcast_to_others(&self, event: &Events) {
        for peer in self.peers() {
            host::send(peer, event);
        }
    }

    fn reset_election_timer(&mut self) {
        let timeout = host::random_between(self.min_election_timeout, self.max_election_timeout);
        self.election_deadline = self.now + timeout;
    }

    fn reset_heartbeat_timer(&mut self) {
        self.heartbeat_deadline = self.now + self.heartbeat_timeout;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::mock;

    fn peer_messages() -> Vec<(i32, Events)> {
        mock::take()
            .sent
            .into_iter()
            .map(|(to, data)| (to, ron::from_str(std::str::from_utf8(&data).unwrap()).unwrap()))
            .collect()
    }

    fn client_replies() -> Vec<String> {
        mock::take()
            .sent
            .into_iter()
            .filter(|(to, _)| *to == CLIENT_ID)
            .map(|(_, data)| String::from_utf8(data).unwrap())
            .collect()
    }

    fn send(server: &mut InstanceState, sender: i32, event: Events) {
        server.receive(sender, ron::to_string(&event).unwrap().as_bytes());
    }

    fn entry(index: i32, term: i32) -> LogEntry {
        LogEntry::nop(index, term)
    }

    fn terms(server: &InstanceState) -> Vec<i32> {
        server.log[1..].iter().map(|entry| entry.term).collect()
    }

    fn vote(term: i32, granted: bool) -> Events {
        Events::VoteResponse(VoteResponse { term, vote_granted: granted })
    }

    fn vote_request(term: i32, candidate_id: i32, last_log_index: i32, last_log_term: i32) -> Events {
        Events::VoteRequest(VoteRequest { term, candidate_id, last_log_index, last_log_term })
    }

    fn append(term: i32, prev_log_index: i32, prev_log_term: i32, entries: Vec<LogEntry>, leader_commit: i32) -> Events {
        Events::AppendEntryRequest(AppendEntryRequest {
            term,
            leader_id: 1,
            prev_log_index,
            prev_log_term,
            entries,
            leader_commit,
        })
    }

    fn append_ok(term: i32, log_index: i32) -> Events {
        Events::AppendEntryResponse(AppendEntryResponse { term, log_index, success: true })
    }

    /// Lets server 1 of 3 time out and win the election with node 2's vote.
    fn elect(server: &mut InstanceState) {
        let now = server.election_deadline;
        server.tick(now);
        send(server, 2, vote(server.current_term, true));
        mock::take();
    }

    fn leader() -> InstanceState {
        let mut server = InstanceState::new(1, 3);
        server.start();
        elect(&mut server);
        server
    }

    #[test]
    fn election_timeout_starts_an_election_and_a_majority_wins() {
        let mut server = InstanceState::new(1, 3);
        server.start();
        server.tick(MIN_ELECTION_TIMEOUT_MS - 10);
        assert_eq!(server.role, Role::Follower);
        server.tick(MIN_ELECTION_TIMEOUT_MS);
        assert_eq!(server.role, Role::Candidate);
        assert_eq!(server.current_term, 1);
        assert_eq!(server.voted_for, Some(1));
        let requests = peer_messages();
        assert_eq!(requests.len(), 2);
        assert!(matches!(requests[0], (2, Events::VoteRequest(VoteRequest { term: 1, candidate_id: 1, .. }))));

        send(&mut server, 2, vote(1, true));
        assert_eq!(server.role, Role::Leader);
        // The new leader appends a no-op for its term and sends it to both peers.
        assert_eq!(server.log[1].operation, Some(Operation::Nop));
        let calls = mock::take();
        assert_eq!(calls.leaders, vec![1]);
        assert_eq!(calls.sent.len(), 2);
    }

    #[test]
    fn heartbeats_hold_off_an_election() {
        let mut server = InstanceState::new(2, 3);
        server.start();
        for now in (0..10_000).step_by(100) {
            if now % 1000 == 0 {
                send(&mut server, 1, append(1, 0, 0, vec![], 0));
            }
            server.tick(now);
        }
        assert_eq!(server.role, Role::Follower);
        assert_eq!(server.current_term, 1);
    }

    #[test]
    fn leader_sends_heartbeats_on_tick() {
        let mut server = leader();
        let now = server.now;
        server.tick(now + HEARTBEAT_TIMEOUT_MS);
        let heartbeats = peer_messages();
        assert_eq!(heartbeats.len(), 2);
        assert!(heartbeats.iter().all(|(_, event)| matches!(event, Events::AppendEntryRequest(_))));
    }

    #[test]
    fn votes_once_per_term() {
        let mut server = InstanceState::new(1, 3);
        send(&mut server, 2, vote_request(1, 2, 0, 0));
        send(&mut server, 3, vote_request(1, 3, 0, 0));
        let replies = peer_messages();
        assert!(matches!(replies[0], (2, Events::VoteResponse(VoteResponse { vote_granted: true, .. }))));
        assert!(matches!(replies[1], (3, Events::VoteResponse(VoteResponse { vote_granted: false, .. }))));
        assert_eq!(server.voted_for, Some(2));
    }

    #[test]
    fn refuses_vote_to_candidate_with_older_log() {
        let mut server = InstanceState::new(1, 3);
        server.current_term = 2;
        server.log.push(entry(1, 2));
        // A newer term, but its last entry is from term 1 while ours is from term 2.
        send(&mut server, 2, vote_request(3, 2, 5, 1));
        assert_eq!(server.current_term, 3);
        assert_eq!(server.voted_for, None);
        assert!(matches!(
            peer_messages()[..],
            [(2, Events::VoteResponse(VoteResponse { term: 3, vote_granted: false }))]
        ));
    }

    #[test]
    fn leader_steps_down_when_it_sees_a_newer_term() {
        let mut server = leader();
        send(&mut server, 3, vote_request(5, 3, 10, 5));
        assert_eq!(server.role, Role::Follower);
        assert_eq!(server.current_term, 5);
        assert_eq!(server.voted_for, Some(3));
    }

    #[test]
    fn rejects_append_when_log_is_too_short_and_says_where_it_ends() {
        let mut server = InstanceState::new(2, 3);
        server.log.push(entry(1, 1));
        send(&mut server, 1, append(1, 5, 1, vec![entry(6, 1)], 0));
        assert_eq!(server.last_log_index(), 1);
        assert!(matches!(
            peer_messages()[..],
            [(1, Events::AppendEntryResponse(AppendEntryResponse { success: false, log_index: 1, .. }))]
        ));
    }

    #[test]
    fn truncates_only_conflicting_entries() {
        let mut server = InstanceState::new(2, 3);
        server.current_term = 2;
        server.log.extend([entry(1, 1), entry(2, 1), entry(3, 1)]);
        // A late request for index 1 must not cut off entries 2 and 3.
        send(&mut server, 1, append(2, 0, 0, vec![entry(1, 1)], 0));
        assert_eq!(terms(&server), vec![1, 1, 1]);
        // A different term at index 2 replaces entry 2 and everything after it.
        send(&mut server, 1, append(2, 1, 1, vec![entry(2, 2)], 0));
        assert_eq!(terms(&server), vec![1, 2]);
    }

    #[test]
    fn commit_index_never_moves_backwards() {
        let mut server = InstanceState::new(2, 3);
        send(&mut server, 1, append(1, 0, 0, vec![entry(1, 1), entry(2, 1), entry(3, 1)], 3));
        assert_eq!(server.commit_index, 3);
        // An older heartbeat that only proves the log matches up to index 1.
        send(&mut server, 1, append(1, 1, 1, vec![], 3));
        assert_eq!(server.commit_index, 3);
    }

    #[test]
    fn only_entries_from_the_current_term_are_committed_by_counting() {
        // Figure 8: an entry from an earlier term isn't safe just because a majority has it.
        let mut server = InstanceState::new(1, 3);
        server.current_term = 1;
        server.log.push(entry(1, 1));
        elect(&mut server); // term 2, no-op at index 2
        send(&mut server, 2, append_ok(2, 1));
        assert_eq!(server.commit_index, 0);
        send(&mut server, 2, append_ok(2, 2));
        assert_eq!(server.commit_index, 2);
        assert_eq!(server.last_applied, 2);
        assert_eq!(mock::take().applied, vec![(1, 1), (2, 2)]);
    }

    #[test]
    fn leader_backs_up_and_retries_after_a_rejection() {
        let mut server = leader();
        server.next_index.insert(2, 2);
        send(&mut server, 2, Events::AppendEntryResponse(AppendEntryResponse { term: 1, log_index: 0, success: false }));
        assert_eq!(server.next_index[&2], 1);
        assert!(matches!(
            peer_messages()[..],
            [(2, Events::AppendEntryRequest(AppendEntryRequest { prev_log_index: 0, .. }))]
        ));
    }

    #[test]
    fn leader_answers_the_client_once_committed() {
        let mut server = leader();
        server.receive(CLIENT_ID, b"ENQ 1 7");
        assert!(client_replies().is_empty());
        send(&mut server, 2, append_ok(1, 2));
        assert_eq!(server.queue, VecDeque::from([7]));
        server.receive(CLIENT_ID, b"DEQ 2");
        send(&mut server, 2, append_ok(1, 3));
        server.receive(CLIENT_ID, b"DEQ 3");
        send(&mut server, 2, append_ok(1, 4));
        assert_eq!(client_replies(), vec!["OK 1", "OK 2 7", "EMPTY 3"]);
    }

    #[test]
    fn retried_client_request_is_applied_once() {
        let mut server = leader();
        server.receive(CLIENT_ID, b"ENQ 1 7");
        server.receive(CLIENT_ID, b"ENQ 1 7"); // retry before the first one committed
        assert_eq!(server.last_log_index(), 3);
        send(&mut server, 2, append_ok(1, 3));
        assert_eq!(server.queue, VecDeque::from([7]));
        // A retry after it committed gets the same answer without a new entry.
        server.receive(CLIENT_ID, b"ENQ 1 7");
        assert_eq!(server.last_log_index(), 3);
        assert_eq!(client_replies(), vec!["OK 1", "OK 1", "OK 1"]);
    }

    #[test]
    fn follower_points_client_at_the_leader() {
        let mut server = InstanceState::new(2, 3);
        server.receive(CLIENT_ID, b"DEQ 1");
        send(&mut server, 1, append(1, 0, 0, vec![], 0));
        server.receive(CLIENT_ID, b"DEQ 2");
        assert_eq!(client_replies(), vec!["NOT_LEADER 1 0", "NOT_LEADER 2 1"]);
        assert!(server.status().starts_with("Follower term=1 leader=Some(1)"));
    }

    #[test]
    fn restart_keeps_term_vote_and_log_but_replays_the_state_machine() {
        let mut server = InstanceState::new(2, 3);
        let request = ClientRequest { request_id: 1, operation: Operation::Enqueue, value: Some(9) };
        send(&mut server, 1, append(1, 0, 0, vec![LogEntry::client_request(1, 1, &request)], 1));
        assert_eq!(server.queue, VecDeque::from([9]));

        server.restart(50_000);
        assert_eq!((server.current_term, server.last_log_index()), (1, 1));
        assert!(server.queue.is_empty());
        // Timeouts count from the restart, not from the last tick before the kill.
        server.tick(50_010);
        assert_eq!(server.role, Role::Follower);
        send(&mut server, 1, append(1, 1, 1, vec![], 1));
        assert_eq!(server.queue, VecDeque::from([9]));
    }
}
