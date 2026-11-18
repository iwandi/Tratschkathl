//! tk-ctl: tiny debugging client for the bus.
//!
//!   tk-ctl call <role> <method> [json-params]
//!   tk-ctl tool <name> [json-args]          (same path an agent's tool call takes)
//!   tk-ctl watch [topic-prefix]             (print events)

use std::time::Duration;

use serde_json::{json, Value};
use tk_bus::Bus;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--self-test") {
        println!("{}", tk_bus::self_test_json("helpers"));
        return;
    }
    let parse = |s: Option<&String>| -> Value {
        s.map(|s| serde_json::from_str(s).unwrap_or_else(|e| {
            eprintln!("bad JSON: {e}");
            std::process::exit(2)
        }))
        .unwrap_or(json!({}))
    };
    let usage = || {
        eprintln!("usage: tk-ctl call <role> <method> [json] | tool <name> [json] | emit <topic> [json] | watch [prefix]");
        std::process::exit(2)
    };
    match args.first().map(String::as_str) {
        Some("call") if args.len() >= 3 => {
            let (bus, _) = Bus::connect(format!("ctl:{}", std::process::id()), "ctl", vec![], None);
            let r = bus.request_timeout(&args[1], &args[2], parse(args.get(3)), Duration::from_secs(1800)).await;
            print_result(r);
        }
        Some("tool") if args.len() >= 2 => {
            let (bus, _) = Bus::connect(format!("ctl:{}", std::process::id()), "ctl", vec![], None);
            let req = json!({ "session": "ctl", "name": args[1], "args": parse(args.get(2)), "idem": format!("ctl-{}", tk_bus::model::now_ms()) });
            let r = bus.request_timeout("core", "tool.call", req, Duration::from_secs(1800)).await;
            print_result(r);
        }
        Some("emit") if args.len() >= 2 => {
            let (bus, _) = Bus::connect(format!("ctl:{}", std::process::id()), "ctl", vec![], None);
            bus.wait_connected().await;
            bus.emit(&args[1], parse(args.get(2)));
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Some("watch") => {
            let prefix = args.get(1).cloned().unwrap_or_default();
            let (_bus, mut ev) = Bus::connect(format!("ctl:{}", std::process::id()), "ctl", vec![prefix], None);
            while let Some((topic, data)) = ev.recv().await {
                println!("{topic} {data}");
            }
        }
        _ => usage(),
    }
}

fn print_result(r: Result<Value, String>) {
    match r {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
