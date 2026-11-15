// A Raft server for wasm-world, in C++. It implements wit/node.wit, using
// the C bindings in bindings/ that wit-bindgen generated from that file.
//
// Messages between servers are plain text, one per send, like
//   VOTE <term> <candidate> <last_log_index> <last_log_term>
// Any format works: only our own servers ever read them.

#include <algorithm>
#include <cstdlib>
#include <deque>
#include <map>
#include <set>
#include <string>
#include <vector>

#include "bindings/node.h"

namespace {

// The host plays the client, at id 0.
const int CLIENT_ID = 0;

// The host delays each message by 10-150 ms by default. Elections only work
// if a round trip is well under the election timeout (§5.6), and heartbeats
// have to arrive before followers time out.
const int HEARTBEAT_TIMEOUT_MS = 300;
const int MIN_ELECTION_TIMEOUT_MS = 1500;
const int MAX_ELECTION_TIMEOUT_MS = 3000;
const size_t MAX_ENTRIES_PER_REQUEST = 64;

enum class Role { Follower, Candidate, Leader };
enum Op { NOP = 0, ENQUEUE = 1, DEQUEUE = 2 };

struct Entry {
    int term;
    int op;
    int request_id;  // 0 for no-ops
    int value;
};

// Calls to the host

void log(const std::string& text) {
    node_string_t message{(uint8_t*)text.data(), text.size()};
    node_log_str(&message);
}

void send(int to, const std::string& text) {
    node_list_u8_t data{(uint8_t*)text.data(), text.size()};
    node_send_message(to, &data);
}

// Text helpers

std::string num(int value) {
    return std::to_string(value);
}

std::vector<std::string> split(const std::string& text) {
    std::vector<std::string> words;
    std::string word;
    for (char c : text) {
        if (c == ' ' || c == '\n') {
            if (!word.empty()) words.push_back(word);
            word.clear();
        } else {
            word += c;
        }
    }
    if (!word.empty()) words.push_back(word);
    return words;
}

bool to_int(const std::string& word, int* out) {
    char* end;
    long value = std::strtol(word.c_str(), &end, 10);
    if (word.empty() || *end != '\0') return false;
    *out = (int)value;
    return true;
}

class Server {
public:
    Server(int id, int cluster_size) : id_(id) {
        for (int peer = 1; peer <= cluster_size; peer++) view_.push_back(peer);
        // A sentinel at index 0, so the first real entry has index 1 like in the paper.
        log_.push_back(Entry{0, NOP, 0, 0});
    }

    void start() {
        log("server up as follower");
        reset_election_timer();
    }

    void receive(int sender, const std::string& text) {
        std::vector<std::string> words = split(text);
        if (words.empty()) return;
        if (sender == CLIENT_ID) {
            handle_client_request(words);
            return;
        }
        // Every message between servers is a type followed by numbers, the first being the term.
        std::vector<int> fields;
        for (size_t i = 1; i < words.size(); i++) {
            int value;
            if (!to_int(words[i], &value)) {
                log("dropping unreadable message: " + text);
                return;
            }
            fields.push_back(value);
        }
        if (fields.empty()) return;
        // Rule for all servers: a newer term means we're out of date, so follow it.
        if (fields[0] > current_term_) step_down(fields[0]);

        const std::string& type = words[0];
        if (type == "VOTE" && fields.size() == 4) {
            handle_vote_request(sender, fields[0], fields[1], fields[2], fields[3]);
        } else if (type == "VOTE_REPLY" && fields.size() == 2) {
            handle_vote_reply(sender, fields[0], fields[1] != 0);
        } else if (type == "APPEND" && fields.size() >= 6 && fields.size() == 6 + 4 * (size_t)fields[5]) {
            handle_append_entries(sender, fields);
        } else if (type == "APPEND_REPLY" && fields.size() == 3) {
            handle_append_entries_reply(sender, fields[0], fields[1] != 0, fields[2]);
        } else {
            log("dropping unreadable message: " + text);
        }
    }

    void tick(int now_ms) {
        now_ = now_ms;
        if (role_ == Role::Leader) {
            if (now_ >= heartbeat_deadline_) {
                broadcast_append_entries();
                heartbeat_deadline_ = now_ + HEARTBEAT_TIMEOUT_MS;
            }
        } else if (now_ >= election_deadline_) {
            start_election();
        }
    }

private:
    // Leader election (§5.2)

    void start_election() {
        current_term_++;
        role_ = Role::Candidate;
        voted_for_ = id_;
        leader_ = 0;
        votes_ = {id_};
        log("election timeout, standing for term " + num(current_term_));
        reset_election_timer();
        if (is_majority(votes_.size())) {
            become_leader();
            return;
        }
        std::string request = "VOTE " + num(current_term_) + " " + num(id_) + " " + num(last_log_index()) + " " +
                              num(last_log_term());
        for (int peer : peers()) send(peer, request);
    }

    void handle_vote_request(int sender, int term, int candidate, int last_index, int last_term) {
        // §5.4.1: only vote for a candidate whose log is at least as up to date
        // as ours, so a new leader always has every committed entry.
        bool log_ok = last_term > last_log_term() || (last_term == last_log_term() && last_index >= last_log_index());
        bool free_to_vote = voted_for_ == 0 || voted_for_ == candidate;
        bool granted = term == current_term_ && free_to_vote && log_ok;
        if (granted) {
            voted_for_ = candidate;
            reset_election_timer();
            log("voted for node " + num(candidate) + " in term " + num(current_term_));
        }
        send(sender, "VOTE_REPLY " + num(current_term_) + " " + (granted ? "1" : "0"));
    }

    void handle_vote_reply(int sender, int term, bool granted) {
        if (role_ != Role::Candidate || term != current_term_ || !granted) return;
        votes_.insert(sender);
        if (is_majority(votes_.size())) become_leader();
    }

    void become_leader() {
        role_ = Role::Leader;
        leader_ = id_;
        log("became LEADER for term " + num(current_term_));
        node_report_leader(current_term_);
        for (int peer : peers()) {
            next_index_[peer] = last_log_index() + 1;
            match_index_[peer] = 0;
        }
        // §8: a no-op from our own term lets us commit entries left by earlier leaders.
        log_.push_back(Entry{current_term_, NOP, 0, 0});
        broadcast_append_entries();
        heartbeat_deadline_ = now_ + HEARTBEAT_TIMEOUT_MS;
        advance_commit_index();
    }

    void step_down(int term) {
        if (role_ == Role::Leader) {
            log("saw term " + num(term) + ", stepping down as leader of term " + num(current_term_));
            // Leaders don't watch for elections, so start doing that again.
            reset_election_timer();
        }
        current_term_ = term;
        voted_for_ = 0;
        role_ = Role::Follower;
        leader_ = 0;
    }

    // Log replication (§5.3)

    void handle_append_entries(int sender, const std::vector<int>& fields) {
        int term = fields[0], leader = fields[1], prev_index = fields[2], prev_term = fields[3];
        int leader_commit = fields[4], count = fields[5];
        if (term < current_term_) {
            // From a deposed leader. Our term in the reply makes it step down.
            reply_append_entries(sender, false, last_log_index());
            return;
        }
        if (role_ == Role::Candidate) role_ = Role::Follower;
        if (leader_ != leader) {
            leader_ = leader;
            log("following leader node " + num(leader) + " in term " + num(term));
        }
        reset_election_timer();

        // Our log must already hold the entry just before the new ones.
        if (prev_index > last_log_index()) {
            reply_append_entries(sender, false, last_log_index());
            return;
        }
        if (log_[prev_index].term != prev_term) {
            reply_append_entries(sender, false, prev_index - 1);
            return;
        }

        for (int i = 0; i < count; i++) {
            Entry entry{fields[6 + 4 * i], fields[7 + 4 * i], fields[8 + 4 * i], fields[9 + 4 * i]};
            int index = prev_index + 1 + i;
            if (index <= last_log_index()) {
                // Already have it, e.g. from a request that arrived out of order.
                if (log_[index].term == entry.term) continue;
                // A conflicting entry: drop it and everything after it.
                log_.resize(index);
            }
            log_.push_back(entry);
        }

        // Only trust the leader's commit index up to what we know matches.
        int match_index = prev_index + count;
        int new_commit = std::min(leader_commit, match_index);
        if (new_commit > commit_index_) {
            commit_index_ = new_commit;
            apply_committed();
        }
        reply_append_entries(sender, true, match_index);
    }

    void handle_append_entries_reply(int sender, int term, bool success, int log_index) {
        // Replies can be from an earlier term or arrive out of order.
        if (role_ != Role::Leader || term != current_term_ || !next_index_.count(sender)) return;
        int& next = next_index_[sender];
        int& matched = match_index_[sender];
        if (success) {
            matched = std::max(matched, log_index);
            next = std::max(next, matched + 1);
            advance_commit_index();
        } else {
            // The follower is missing entries or has conflicting ones: back up and retry now.
            next = std::max(std::min(log_index + 1, next), matched + 1);
            send_append_entries(sender);
        }
    }

    void advance_commit_index() {
        // §5.4.2: only count replicas of entries from our own term. Earlier
        // entries get committed along with them (Figure 8 shows why).
        for (int index = last_log_index(); index > commit_index_ && log_[index].term == current_term_; index--) {
            size_t replicas = 1;
            for (auto& [peer, matched] : match_index_) {
                if (matched >= index) replicas++;
            }
            if (is_majority(replicas)) {
                commit_index_ = index;
                apply_committed();
                return;
            }
        }
    }

    void broadcast_append_entries() {
        for (int peer : peers()) send_append_entries(peer);
    }

    // Doubles as the heartbeat when the follower is up to date.
    void send_append_entries(int peer) {
        int next = std::min(next_index_[peer], last_log_index() + 1);
        int prev_index = next - 1;
        size_t count = std::min(MAX_ENTRIES_PER_REQUEST, log_.size() - next);
        std::string message = "APPEND " + num(current_term_) + " " + num(id_) + " " + num(prev_index) + " " +
                              num(log_[prev_index].term) + " " + num(commit_index_) + " " + num((int)count);
        for (size_t i = next; i < next + count; i++) {
            const Entry& entry = log_[i];
            message += " " + num(entry.term) + " " + num(entry.op) + " " + num(entry.request_id) + " " + num(entry.value);
        }
        send(peer, message);
    }

    void reply_append_entries(int leader, bool success, int log_index) {
        send(leader, "APPEND_REPLY " + num(current_term_) + " " + (success ? "1" : "0") + " " + num(log_index));
    }

    // The client and the queue (§8)

    void handle_client_request(const std::vector<std::string>& words) {
        int request_id = 0, value = 0, op;
        if (words.size() == 3 && words[0] == "ENQ" && to_int(words[1], &request_id) && to_int(words[2], &value)) {
            op = ENQUEUE;
        } else if (words.size() == 2 && words[0] == "DEQ" && to_int(words[1], &request_id)) {
            op = DEQUEUE;
        } else {
            log("ignoring bad client request");
            return;
        }
        if (role_ != Role::Leader) {
            send(CLIENT_ID, "NOT_LEADER " + num(request_id) + " " + num(leader_));
            return;
        }
        if (request_id == last_request_id_) {
            // Already applied; the client just didn't get our answer.
            send(CLIENT_ID, last_reply_);
            return;
        }
        log("client request " + num(request_id) + ", appending at index " + num(last_log_index() + 1));
        log_.push_back(Entry{current_term_, op, request_id, value});
        broadcast_append_entries();
        advance_commit_index();
    }

    void apply_committed() {
        while (last_applied_ < commit_index_) {
            last_applied_++;
            const Entry& entry = log_[last_applied_];
            std::string reply = apply(entry);
            node_report_applied(last_applied_, entry.term);
            if (role_ == Role::Leader && !reply.empty()) send(CLIENT_ID, reply);
        }
    }

    // Runs one entry on the queue. Returns the reply for the client, if any.
    std::string apply(const Entry& entry) {
        if (entry.op == NOP) return "";
        if (entry.request_id <= last_request_id_) {
            // The same request was appended twice (the client retried). Don't redo it.
            return entry.request_id == last_request_id_ ? last_reply_ : "";
        }
        std::string id = num(entry.request_id);
        std::string reply;
        if (entry.op == ENQUEUE) {
            queue_.push_back(entry.value);
            reply = "OK " + id;
        } else if (queue_.empty()) {
            reply = "EMPTY " + id;
        } else {
            reply = "OK " + id + " " + num(queue_.front());
            queue_.pop_front();
        }
        last_request_id_ = entry.request_id;
        last_reply_ = reply;
        return reply;
    }

    // Helpers

    int last_log_index() const { return (int)log_.size() - 1; }
    int last_log_term() const { return log_.back().term; }
    bool is_majority(size_t count) const { return count * 2 > view_.size(); }

    std::vector<int> peers() const {
        std::vector<int> peers;
        for (int peer : view_) {
            if (peer != id_) peers.push_back(peer);
        }
        return peers;
    }

    void reset_election_timer() {
        election_deadline_ = now_ + node_random(MIN_ELECTION_TIMEOUT_MS, MAX_ELECTION_TIMEOUT_MS);
    }

    int id_;
    std::vector<int> view_;  // every server, including this one
    Role role_ = Role::Follower;
    int leader_ = 0;  // 0 when unknown

    int now_ = 0;
    int election_deadline_ = 0;
    int heartbeat_deadline_ = 0;

    // Persistent state (Figure 2). There's no disk: `kill` only freezes a server.
    int current_term_ = 0;
    int voted_for_ = 0;  // 0 when we haven't voted this term
    std::vector<Entry> log_;

    int commit_index_ = 0;
    int last_applied_ = 0;
    std::set<int> votes_;
    std::map<int, int> next_index_;
    std::map<int, int> match_index_;

    // The queue, plus the client's latest request and its reply, so a
    // retried request isn't applied twice (§8).
    std::deque<int> queue_;
    int last_request_id_ = 0;
    std::string last_reply_;
};

// A pointer rather than a global object, so nothing needs a constructor
// to run before `start`.
Server* server = nullptr;

}  // namespace

// The exports the host calls, declared in bindings/node.h.

void exports_node_start(int32_t id, int32_t cluster_size) {
    server = new Server(id, cluster_size);
    server->start();
}

void exports_node_receive(int32_t sender, node_list_u8_t* data) {
    std::string text((const char*)data->ptr, data->len);
    // The host hands us ownership of the bytes.
    node_list_u8_free(data);
    if (server) server->receive(sender, text);
}

void exports_node_tick(int32_t now_ms) {
    if (server) server->tick(now_ms);
}
