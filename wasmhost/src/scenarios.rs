// `wasmhost test`: fixed scenarios that check a Raft implementation from the
// outside. Each runs a fresh cluster in simulated time. It passes if every
// request is answered in time and every answer matches the host's own copy
// of the queue.

use std::time::Instant;

use crate::cluster::{Cluster, Loaded, Op};

const LOG_LINES_ON_FAILURE: usize = 25;

pub struct Scenario {
    pub name: &'static str,
    run: fn(&mut Cluster) -> Result<(), String>,
}

pub const SCENARIOS: &[Scenario] = &[
    Scenario { name: "elects a leader and answers a request", run: first_request },
    Scenario { name: "replicates many requests in order", run: many_requests },
    Scenario { name: "survives the leader crashing", run: leader_crash },
    Scenario { name: "keeps working with a minority down", run: minority_down },
    Scenario { name: "stalls without a majority, then recovers", run: majority_down },
    Scenario { name: "copes with slow, reordered messages", run: slow_network },
    Scenario { name: "copes with lost messages", run: lossy_network },
    Scenario { name: "repairs a stale old leader when it rejoins", run: stale_leader_rejoins },
    Scenario { name: "won't elect a server that missed writes", run: stale_server_loses },
    Scenario { name: "stays correct through random crashes and lost messages", run: chaos },
];

/// Runs the scenarios and prints a line for each. Returns whether all passed.
pub fn run(loaded: &Loaded, module: &str, num_servers: i32, seed: u64, only: Option<usize>) -> bool {
    println!("Testing {} with {} servers, seed {}\n", module, num_servers, seed);
    let mut passed = 0;
    let mut total = 0;
    for (number, scenario) in (1..).zip(SCENARIOS) {
        if only.is_some_and(|only| only != number) {
            continue;
        }
        total += 1;
        let started = Instant::now();
        let mut cluster = match Cluster::new(loaded, num_servers, seed + number as u64, false) {
            Ok(cluster) => cluster,
            Err(error) => {
                println!("  FAIL  {}. {}\n        couldn't start the cluster: {:#}\n", number, scenario.name, error);
                continue;
            }
        };
        let outcome = (scenario.run)(&mut cluster);
        // A violation usually explains a later failure, so it wins.
        let failure = cluster.violations().first().cloned().or(outcome.err());
        match failure {
            None => {
                passed += 1;
                println!(
                    "  PASS  {}. {}  ({:.1} s simulated, {} ms real)",
                    number,
                    scenario.name,
                    cluster.now() as f64 / 1000.0,
                    started.elapsed().as_millis(),
                );
            }
            Some(reason) => {
                println!("  FAIL  {}. {}\n        {}", number, scenario.name, reason);
                println!("        last log lines:");
                for line in cluster.log.recent(LOG_LINES_ON_FAILURE) {
                    println!("          {}", line);
                }
                println!(
                    "        rerun: wasmhost test --servers {} --seed {} --only {} {}\n",
                    num_servers, seed, number, module,
                );
            }
        }
    }
    println!("\n{}/{} passed", passed, total);
    passed == total
}

// Helpers

/// Submits `ops` and waits until all of them are answered.
fn serve(cluster: &mut Cluster, ops: &[Op], limit_s: u64) -> Result<(), String> {
    for &op in ops {
        cluster.submit(op);
    }
    if cluster.run_until(limit_s * 1000, Cluster::client_idle) {
        return Ok(());
    }
    let (request_id, op, waited) = cluster.oldest_pending().unwrap();
    Err(format!("request #{} ({:?}) got no answer in {:.1} s", request_id, op, waited as f64 / 1000.0))
}

fn enqueues(values: std::ops::RangeInclusive<i32>) -> Vec<Op> {
    values.map(Op::Enqueue).collect()
}

fn dequeues(count: usize) -> Vec<Op> {
    vec![Op::Dequeue; count]
}

/// Enqueues `values`, dequeuing after every third, then dequeues until the
/// queue is empty and once more.
fn mixed(values: std::ops::RangeInclusive<i32>) -> Vec<Op> {
    let count = values.clone().count();
    let mut ops = Vec::new();
    for value in values {
        ops.push(Op::Enqueue(value));
        if value % 3 == 0 {
            ops.push(Op::Dequeue);
        }
    }
    ops.extend(dequeues(count - count / 3 + 1));
    ops
}

fn leader(cluster: &Cluster) -> Result<i32, String> {
    cluster.leader().ok_or_else(|| "no live server is known to be leader".to_string())
}

fn kill(cluster: &mut Cluster, id: i32) -> Result<(), String> {
    cluster.kill(id)
}

fn majority(cluster: &Cluster) -> usize {
    cluster.num_servers() as usize / 2 + 1
}

// Scenarios

fn first_request(cluster: &mut Cluster) -> Result<(), String> {
    serve(cluster, &[Op::Enqueue(1)], 10)
}

fn many_requests(cluster: &mut Cluster) -> Result<(), String> {
    serve(cluster, &mixed(1..=20), 60)
}

fn leader_crash(cluster: &mut Cluster) -> Result<(), String> {
    serve(cluster, &enqueues(1..=5), 15)?;
    let old_leader = leader(cluster)?;
    kill(cluster, old_leader)?;
    serve(cluster, &[enqueues(6..=10), dequeues(3)].concat(), 30)?;
    cluster.revive(old_leader)?;
    serve(cluster, &dequeues(8), 20)
}

fn minority_down(cluster: &mut Cluster) -> Result<(), String> {
    serve(cluster, &[Op::Enqueue(1)], 10)?;
    let leader = leader(cluster)?;
    let minority = cluster.num_servers() as usize - majority(cluster);
    let followers: Vec<i32> = (1..=cluster.num_servers()).filter(|&id| id != leader).collect();
    for &id in &followers[..minority] {
        kill(cluster, id)?;
    }
    serve(cluster, &mixed(2..=11), 30)
}

fn majority_down(cluster: &mut Cluster) -> Result<(), String> {
    serve(cluster, &[Op::Enqueue(1)], 10)?;
    for id in 1..=majority(cluster) as i32 {
        kill(cluster, id)?;
    }
    let answered = cluster.finished();
    cluster.submit(Op::Dequeue);
    cluster.run_for(10_000);
    if cluster.finished() > answered {
        return Err("a request was answered while most servers were down".to_string());
    }
    for id in cluster.down_servers() {
        cluster.revive(id)?;
    }
    serve(cluster, &[Op::Enqueue(2), Op::Dequeue], 30)
}

fn slow_network(cluster: &mut Cluster) -> Result<(), String> {
    cluster.network.min_delay = 50;
    cluster.network.max_delay = 400;
    serve(cluster, &mixed(1..=20), 120)
}

fn lossy_network(cluster: &mut Cluster) -> Result<(), String> {
    cluster.network.loss_percent = 10;
    serve(cluster, &mixed(1..=20), 120)
}

/// The leader takes a request it can't commit (its followers are down),
/// then crashes. The others carry on without it. When it comes back, its
/// uncommitted entry must be replaced, and it must still be able to help
/// form a majority.
fn stale_leader_rejoins(cluster: &mut Cluster) -> Result<(), String> {
    serve(cluster, &[Op::Enqueue(1)], 10)?;
    let old_leader = leader(cluster)?;
    let followers: Vec<i32> = (1..=cluster.num_servers()).filter(|&id| id != old_leader).collect();
    for &id in &followers {
        kill(cluster, id)?;
    }
    cluster.submit(Op::Enqueue(2));
    cluster.run_for(3_000);
    kill(cluster, old_leader)?;
    for &id in &followers {
        cluster.revive(id)?;
    }
    serve(cluster, &[Op::Enqueue(3), Op::Dequeue], 30)?;

    cluster.revive(old_leader)?;
    cluster.run_for(3_000);
    // Make the old leader necessary: leave exactly a majority up, with the
    // old leader in it and the new leader out.
    let new_leader = leader(cluster)?;
    let others: Vec<i32> = (1..=cluster.num_servers())
        .filter(|&id| id != old_leader && id != new_leader)
        .collect();
    let extra_kills = cluster.num_servers() as usize - majority(cluster) - 1;
    kill(cluster, new_leader)?;
    for &id in &others[..extra_kills] {
        kill(cluster, id)?;
    }
    serve(cluster, &[Op::Enqueue(4), Op::Dequeue, Op::Dequeue, Op::Dequeue, Op::Dequeue], 30)
}

/// A follower misses some writes, then the leader dies just as the follower
/// comes back. It races the up-to-date servers to time out, but must not
/// win: they refuse to vote for a log that's behind theirs (§5.4.1).
/// Repeated, because which server times out first is down to chance.
fn stale_server_loses(cluster: &mut Cluster) -> Result<(), String> {
    for round in 0..3 {
        let base = round * 10;
        serve(cluster, &[Op::Enqueue(base + 1)], 30)?;
        let leader = leader(cluster)?;
        let stale = (1..=cluster.num_servers()).find(|&id| id != leader).unwrap();
        kill(cluster, stale)?;
        serve(cluster, &enqueues(base + 2..=base + 6), 30)?;
        kill(cluster, leader)?;
        cluster.revive(stale)?;
        serve(cluster, &dequeues(6), 30)?;
        cluster.revive(leader)?;
    }
    Ok(())
}

/// Random crashes and restarts under steady traffic. Killing the leader just
/// after a request leaves uncommitted entries behind, so logs diverge and
/// have to be repaired many times over.
fn chaos(cluster: &mut Cluster) -> Result<(), String> {
    let num_servers = cluster.num_servers();
    let majority = majority(cluster);
    cluster.network.loss_percent = 5;
    let mut next_value = 1;
    for _ in 0..60 {
        for _ in 0..cluster.random(1..4) {
            cluster.submit(Op::Enqueue(next_value));
            next_value += 1;
        }
        cluster.submit(Op::Dequeue);
        let pause = cluster.random(0..400);
        cluster.run_for(pause);

        let down = cluster.down_servers();
        let up = num_servers as usize - down.len();
        match cluster.random(0..4) {
            0 if up > majority - 1 => {
                if let Some(id) = cluster.leader() {
                    kill(cluster, id)?;
                }
            }
            1 if up > majority - 1 => {
                let id = cluster.random(1..num_servers as u64 + 1) as i32;
                if cluster.is_alive(id) {
                    kill(cluster, id)?;
                }
            }
            _ if !down.is_empty() => {
                let id = down[cluster.random(0..down.len() as u64) as usize];
                cluster.revive(id)?;
            }
            _ => {}
        }
        let pause = cluster.random(500..4000);
        cluster.run_for(pause);
    }
    for id in cluster.down_servers() {
        cluster.revive(id)?;
    }
    cluster.network.loss_percent = 0;
    serve(cluster, &dequeues(next_value as usize), 120)
}
