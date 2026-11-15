mod cluster;
mod scenarios;

use std::io::BufRead;
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

use cluster::{Cluster, Loaded, Op};

const DEFAULT_MODULE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../wasminstance/target/wasm32-wasip2/release/wasminstance.wasm"
);

const USAGE: &str = "\
usage:
  wasmhost [--servers N] [--seed S] [MODULE.wasm]                 run a cluster and type commands
  wasmhost test [--servers N] [--seed S] [--only K] [MODULE.wasm]  run the test scenarios

MODULE defaults to the Rust reference solution in wasminstance/.";

const HELP: &str = "\
commands:
  enq <n>             the client enqueues n
  deq                 the client dequeues
  kill <id|leader>    freeze a server; messages to it are lost
  revive <id|all>     unfreeze killed servers
  status              show which servers are up and what they've reported
  verbose on|off      also show every message
  delay <min> <max>   set the network delay range in ms
  loss <percent>      lose this share of messages
  sleep <ms>          wait before reading the next command (for scripts)
  help, quit";

struct Options {
    test: bool,
    servers: i32,
    seed: Option<u64>,
    only: Option<usize>,
    module: String,
}

fn parse_options() -> Result<Options> {
    let mut args = std::env::args().skip(1).peekable();
    let mut options = Options {
        test: args.next_if(|arg| arg == "test").is_some(),
        servers: 3,
        seed: None,
        only: None,
        module: DEFAULT_MODULE.to_string(),
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| anyhow!("{} needs a value\n\n{}", name, USAGE));
        match arg.as_str() {
            "--servers" => options.servers = value("--servers")?.parse()?,
            "--seed" => options.seed = Some(value("--seed")?.parse()?),
            "--only" => options.only = Some(value("--only")?.parse()?),
            "-h" | "--help" => {
                println!("{}\n\n{}", USAGE, HELP);
                std::process::exit(0);
            }
            _ if arg.starts_with('-') => bail!("unknown option {}\n\n{}", arg, USAGE),
            _ => options.module = arg,
        }
    }
    if options.servers < 1 {
        bail!("--servers must be at least 1");
    }
    Ok(options)
}

fn main() -> Result<()> {
    let options = parse_options()?;
    let loaded = Loaded::new(&options.module)
        .with_context(|| format!("couldn't load {}", options.module))?;
    if options.test {
        let seed = options.seed.unwrap_or(1);
        let all_passed = scenarios::run(&loaded, &options.module, options.servers, seed, options.only);
        std::process::exit(if all_passed { 0 } else { 1 });
    }
    let seed = options.seed.unwrap_or_else(|| {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
    });
    interactive(&loaded, options.servers, seed)
}

enum Command {
    Enqueue(i32),
    Dequeue,
    // None kills whichever server is leading right now.
    Kill(Option<i32>),
    // None revives every server that is down.
    Revive(Option<i32>),
    Status,
    Verbose(bool),
    Delay(u64, u64),
    Loss(u32),
    Help,
    Quit,
}

/// Runs the cluster in real time, taking commands from stdin.
fn interactive(loaded: &Loaded, servers: i32, seed: u64) -> Result<()> {
    println!("{}\n", HELP);
    let mut cluster = Cluster::new(loaded, servers, seed, true)?;
    cluster.print("host", &format!("{} servers, seed {}", servers, seed));
    let commands = spawn_command_reader();
    let started = Instant::now();
    loop {
        while let Ok((line, command)) = commands.try_recv() {
            cluster.print("host", &format!("> {}", line));
            match handle_command(&mut cluster, command) {
                Ok(true) => {}
                Ok(false) => {
                    let summary = cluster.summary();
                    cluster.print("host", &summary);
                    return Ok(());
                }
                Err(problem) => cluster.print("host", &problem),
            }
        }
        cluster.advance_to(started.elapsed().as_millis() as u64);
        thread::sleep(Duration::from_millis(5));
    }
}

/// Returns false to quit, or a message to show if the command failed.
fn handle_command(cluster: &mut Cluster, command: Command) -> Result<bool, String> {
    match command {
        Command::Enqueue(value) => cluster.submit(Op::Enqueue(value)),
        Command::Dequeue => cluster.submit(Op::Dequeue),
        Command::Kill(target) => {
            let id = target.or_else(|| cluster.leader()).ok_or("no live leader known right now")?;
            cluster.kill(id)?;
        }
        Command::Revive(Some(id)) => cluster.revive(id)?,
        Command::Revive(None) => {
            let down = cluster.down_servers();
            if down.is_empty() {
                return Err("every server is already running".to_string());
            }
            for id in down {
                cluster.revive(id)?;
            }
        }
        Command::Status => cluster.status(),
        Command::Verbose(on) => cluster.log.verbose = on,
        Command::Delay(min_delay, max_delay) => {
            if min_delay > max_delay {
                return Err("delay needs min <= max".to_string());
            }
            cluster.network.min_delay = min_delay;
            cluster.network.max_delay = max_delay;
        }
        Command::Loss(percent) => {
            if percent > 100 {
                return Err("loss is a percentage, 0-100".to_string());
            }
            cluster.network.loss_percent = percent;
        }
        Command::Help => println!("{}", HELP),
        Command::Quit => return Ok(false),
    }
    Ok(true)
}

fn parse_number<T: std::str::FromStr>(word: &str) -> Result<T, String> {
    word.parse().map_err(|_| format!("`{}` is not a number", word))
}

fn parse_command(line: &str) -> Result<Command, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["enq" | "enqueue", value] => Ok(Command::Enqueue(parse_number(value)?)),
        ["deq" | "dequeue"] => Ok(Command::Dequeue),
        ["kill", "leader"] => Ok(Command::Kill(None)),
        ["kill", id] => Ok(Command::Kill(Some(parse_number(id)?))),
        ["revive", "all"] => Ok(Command::Revive(None)),
        ["revive", id] => Ok(Command::Revive(Some(parse_number(id)?))),
        ["status"] => Ok(Command::Status),
        ["verbose", "on"] => Ok(Command::Verbose(true)),
        ["verbose", "off"] => Ok(Command::Verbose(false)),
        ["delay", min_delay, max_delay] => Ok(Command::Delay(parse_number(min_delay)?, parse_number(max_delay)?)),
        ["loss", percent] => Ok(Command::Loss(parse_number(percent)?)),
        ["help"] => Ok(Command::Help),
        ["quit" | "exit"] => Ok(Command::Quit),
        _ => Err(format!("unknown command `{}`, try `help`", line)),
    }
}

/// Reads commands from stdin, typed or piped in from a script.
fn spawn_command_reader() -> Receiver<(String, Command)> {
    let (sender, receiver) = channel();
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Handled here so it pauses the script, not the cluster.
            if let Some(ms) = line.strip_prefix("sleep ") {
                match ms.trim().parse() {
                    Ok(ms) => thread::sleep(Duration::from_millis(ms)),
                    Err(_) => println!("usage: sleep <ms>"),
                }
                continue;
            }
            match parse_command(line) {
                Ok(command) => {
                    if sender.send((line.to_string(), command)).is_err() {
                        return;
                    }
                }
                Err(message) => println!("{}", message),
            }
        }
        // End of input (Ctrl-D, or the end of a script) stops the cluster.
        let _ = sender.send(("quit".to_string(), Command::Quit));
    });
    receiver
}
