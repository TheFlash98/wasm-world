"""A Raft server for wasm-world, in Python.

It implements wit/node.wit. componentize-py turns this file into a wasm
component, and generates the `wit_world` module it imports from that file.

Messages between servers are JSON. Any format works: only our own servers
ever read them.
"""

import json
from collections import deque

import wit_world
from wit_world import log_str, report_applied, report_leader, send_message
from wit_world import random as random_between

# The host plays the client, at id 0.
CLIENT_ID = 0

# The host delays each message by 10-150 ms by default. Elections only work
# if a round trip is well under the election timeout (§5.6), and heartbeats
# have to arrive before followers time out.
HEARTBEAT_TIMEOUT_MS = 300
MIN_ELECTION_TIMEOUT_MS = 1500
MAX_ELECTION_TIMEOUT_MS = 3000
MAX_ENTRIES_PER_REQUEST = 64

FOLLOWER, CANDIDATE, LEADER = "Follower", "Candidate", "Leader"


def send(to, message):
    send_message(to, json.dumps(message).encode())


def answer_client(text):
    send_message(CLIENT_ID, text.encode())


class Server:
    def __init__(self, node_id, cluster_size):
        self.id = node_id
        self.view = list(range(1, cluster_size + 1))  # every server, including this one
        self.role = FOLLOWER
        self.leader = 0  # 0 when unknown

        self.now = 0
        self.election_deadline = 0
        self.heartbeat_deadline = 0

        # Persistent state (Figure 2). There's no disk: `kill` only freezes a server.
        self.current_term = 0
        self.voted_for = 0  # 0 when we haven't voted this term
        # A sentinel at index 0, so the first real entry has index 1 like in the paper.
        self.log = [{"term": 0, "op": "nop"}]

        self.commit_index = 0
        self.last_applied = 0
        self.votes = set()
        self.next_index = {}
        self.match_index = {}

        # The queue, plus the client's latest request and its reply, so a
        # retried request isn't applied twice (§8).
        self.queue = deque()
        self.last_request_id = 0
        self.last_reply = None

    def start(self):
        log_str("server up as follower")
        self.reset_election_timer()

    def receive(self, sender, data):
        if sender == CLIENT_ID:
            self.handle_client_request(data.decode(errors="replace").split())
            return
        try:
            message = json.loads(data)
        except ValueError:
            log_str(f"dropping unreadable message from node {sender}")
            return
        # Rule for all servers: a newer term means we're out of date, so follow it.
        if message["term"] > self.current_term:
            self.step_down(message["term"])
        handlers = {
            "vote": self.handle_vote_request,
            "vote_reply": self.handle_vote_reply,
            "append": self.handle_append_entries,
            "append_reply": self.handle_append_entries_reply,
        }
        handlers[message["type"]](sender, message)

    def tick(self, now_ms):
        self.now = now_ms
        if self.role == LEADER:
            if self.now >= self.heartbeat_deadline:
                self.broadcast_append_entries()
                self.heartbeat_deadline = self.now + HEARTBEAT_TIMEOUT_MS
        elif self.now >= self.election_deadline:
            self.start_election()

    # Leader election (§5.2)

    def start_election(self):
        self.current_term += 1
        self.role = CANDIDATE
        self.voted_for = self.id
        self.leader = 0
        self.votes = {self.id}
        log_str(f"election timeout, standing for term {self.current_term}")
        self.reset_election_timer()
        if self.is_majority(len(self.votes)):
            self.become_leader()
            return
        request = {
            "type": "vote",
            "term": self.current_term,
            "candidate": self.id,
            "last_log_index": self.last_log_index(),
            "last_log_term": self.last_log_term(),
        }
        for peer in self.peers():
            send(peer, request)

    def handle_vote_request(self, sender, message):
        # §5.4.1: only vote for a candidate whose log is at least as up to date
        # as ours, so a new leader always has every committed entry.
        theirs = (message["last_log_term"], message["last_log_index"])
        log_ok = theirs >= (self.last_log_term(), self.last_log_index())
        free_to_vote = self.voted_for in (0, message["candidate"])
        granted = message["term"] == self.current_term and free_to_vote and log_ok
        if granted:
            self.voted_for = message["candidate"]
            self.reset_election_timer()
            log_str(f"voted for node {self.voted_for} in term {self.current_term}")
        send(sender, {"type": "vote_reply", "term": self.current_term, "granted": granted})

    def handle_vote_reply(self, sender, message):
        if self.role != CANDIDATE or message["term"] != self.current_term or not message["granted"]:
            return
        self.votes.add(sender)
        if self.is_majority(len(self.votes)):
            self.become_leader()

    def become_leader(self):
        self.role = LEADER
        self.leader = self.id
        log_str(f"became LEADER for term {self.current_term}")
        report_leader(self.current_term)
        for peer in self.peers():
            self.next_index[peer] = self.last_log_index() + 1
            self.match_index[peer] = 0
        # §8: a no-op from our own term lets us commit entries left by earlier leaders.
        self.log.append({"term": self.current_term, "op": "nop"})
        self.broadcast_append_entries()
        self.heartbeat_deadline = self.now + HEARTBEAT_TIMEOUT_MS
        self.advance_commit_index()

    def step_down(self, term):
        if self.role == LEADER:
            log_str(f"saw term {term}, stepping down as leader of term {self.current_term}")
            # Leaders don't watch for elections, so start doing that again.
            self.reset_election_timer()
        self.current_term = term
        self.voted_for = 0
        self.role = FOLLOWER
        self.leader = 0

    # Log replication (§5.3)

    def handle_append_entries(self, sender, message):
        if message["term"] < self.current_term:
            # From a deposed leader. Our term in the reply makes it step down.
            self.reply_append_entries(sender, False, self.last_log_index())
            return
        if self.role == CANDIDATE:
            self.role = FOLLOWER
        if self.leader != message["leader"]:
            self.leader = message["leader"]
            log_str(f"following leader node {self.leader} in term {message['term']}")
        self.reset_election_timer()

        # Our log must already hold the entry just before the new ones.
        prev_index = message["prev_log_index"]
        if prev_index > self.last_log_index():
            self.reply_append_entries(sender, False, self.last_log_index())
            return
        if self.log[prev_index]["term"] != message["prev_log_term"]:
            self.reply_append_entries(sender, False, prev_index - 1)
            return

        for offset, entry in enumerate(message["entries"]):
            index = prev_index + 1 + offset
            if index <= self.last_log_index():
                if self.log[index]["term"] == entry["term"]:
                    continue  # already have it, e.g. from a request that arrived out of order
                del self.log[index:]  # a conflicting entry: drop it and everything after it
            self.log.append(entry)

        # Only trust the leader's commit index up to what we know matches.
        match_index = prev_index + len(message["entries"])
        new_commit = min(message["leader_commit"], match_index)
        if new_commit > self.commit_index:
            self.commit_index = new_commit
            self.apply_committed()
        self.reply_append_entries(sender, True, match_index)

    def handle_append_entries_reply(self, sender, message):
        # Replies can be from an earlier term or arrive out of order.
        if self.role != LEADER or message["term"] != self.current_term or sender not in self.next_index:
            return
        if message["success"]:
            self.match_index[sender] = max(self.match_index[sender], message["log_index"])
            self.next_index[sender] = max(self.next_index[sender], self.match_index[sender] + 1)
            self.advance_commit_index()
        else:
            # The follower is missing entries or has conflicting ones: back up and retry now.
            backed_up = min(message["log_index"] + 1, self.next_index[sender])
            self.next_index[sender] = max(backed_up, self.match_index[sender] + 1)
            self.send_append_entries(sender)

    def advance_commit_index(self):
        # §5.4.2: only count replicas of entries from our own term. Earlier
        # entries get committed along with them (Figure 8 shows why).
        index = self.last_log_index()
        while index > self.commit_index and self.log[index]["term"] == self.current_term:
            replicas = 1 + sum(1 for matched in self.match_index.values() if matched >= index)
            if self.is_majority(replicas):
                self.commit_index = index
                self.apply_committed()
                return
            index -= 1

    def broadcast_append_entries(self):
        for peer in self.peers():
            self.send_append_entries(peer)

    def send_append_entries(self, peer):
        """Doubles as the heartbeat when the follower is up to date."""
        next_index = min(self.next_index[peer], self.last_log_index() + 1)
        prev_index = next_index - 1
        send(peer, {
            "type": "append",
            "term": self.current_term,
            "leader": self.id,
            "prev_log_index": prev_index,
            "prev_log_term": self.log[prev_index]["term"],
            "entries": self.log[next_index:next_index + MAX_ENTRIES_PER_REQUEST],
            "leader_commit": self.commit_index,
        })

    def reply_append_entries(self, leader, success, log_index):
        send(leader, {"type": "append_reply", "term": self.current_term, "success": success, "log_index": log_index})

    # The client and the queue (§8)

    def handle_client_request(self, words):
        try:
            if len(words) == 3 and words[0] == "ENQ":
                op, request_id, value = "enqueue", int(words[1]), int(words[2])
            elif len(words) == 2 and words[0] == "DEQ":
                op, request_id, value = "dequeue", int(words[1]), None
            else:
                raise ValueError
        except ValueError:
            log_str(f"ignoring bad client request {' '.join(words)!r}")
            return
        if self.role != LEADER:
            answer_client(f"NOT_LEADER {request_id} {self.leader}")
            return
        if request_id == self.last_request_id:
            # Already applied; the client just didn't get our answer.
            answer_client(self.last_reply)
            return
        log_str(f"client request {request_id}, appending at index {self.last_log_index() + 1}")
        self.log.append({"term": self.current_term, "op": op, "request_id": request_id, "value": value})
        self.broadcast_append_entries()
        self.advance_commit_index()

    def apply_committed(self):
        while self.last_applied < self.commit_index:
            self.last_applied += 1
            entry = self.log[self.last_applied]
            reply = self.apply(entry)
            report_applied(self.last_applied, entry["term"])
            if self.role == LEADER and reply:
                answer_client(reply)

    def apply(self, entry):
        """Runs one entry on the queue. Returns the reply for the client, if any."""
        if entry["op"] == "nop":
            return None
        request_id = entry["request_id"]
        if request_id <= self.last_request_id:
            # The same request was appended twice (the client retried). Don't redo it.
            return self.last_reply if request_id == self.last_request_id else None
        if entry["op"] == "enqueue":
            self.queue.append(entry["value"])
            reply = f"OK {request_id}"
        elif self.queue:
            reply = f"OK {request_id} {self.queue.popleft()}"
        else:
            reply = f"EMPTY {request_id}"
        self.last_request_id = request_id
        self.last_reply = reply
        return reply

    # Helpers

    def last_log_index(self):
        return len(self.log) - 1

    def last_log_term(self):
        return self.log[-1]["term"]

    def is_majority(self, count):
        return count * 2 > len(self.view)

    def peers(self):
        return [peer for peer in self.view if peer != self.id]

    def reset_election_timer(self):
        self.election_deadline = self.now + random_between(MIN_ELECTION_TIMEOUT_MS, MAX_ELECTION_TIMEOUT_MS)


# A module-level global, so it doesn't matter how many WitWorld objects
# componentize-py creates.
server = None


class WitWorld(wit_world.WitWorld):
    """The exports the host calls."""

    def start(self, id: int, cluster_size: int) -> None:
        global server
        server = Server(id, cluster_size)
        server.start()

    def receive(self, sender: int, data: bytes) -> None:
        server.receive(sender, data)

    def tick(self, now_ms: int) -> None:
        server.tick(now_ms)
