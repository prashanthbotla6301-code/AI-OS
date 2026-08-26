//! agentos-ctl — operator CLI for AgentOS Core and friends.
//! Runs inside the OS terminal (and against dev-mode instances on a host).

use agentos_common::Paths;
use std::process::exit;

fn call(sock: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    let p = Paths::detect();
    match agentos_ipc::call(&p.socket(sock), method, params) {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "error": e.message, "code": e.code }),
    }
}

fn pretty(v: serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        help();
        exit(2);
    }
    use agentos_common::sockets as s;
    match args[0].as_str() {
        "status" => pretty(call(s::CORE, "status", serde_json::json!({}))),
        "services" => pretty(call(s::CORE, "services", serde_json::json!({}))),
        "sysinfo" => pretty(call(s::CORE, "sys.info", serde_json::json!({}))),
        "restart" if args.len() > 1 => pretty(call(
            s::CORE,
            "service.restart",
            serde_json::json!({"name": args[1]}),
        )),
        "stop-service" if args.len() > 1 => pretty(call(
            s::CORE,
            "service.stop",
            serde_json::json!({"name": args[1]}),
        )),
        "shutdown" | "poweroff" => {
            pretty(call(s::CORE, "shutdown", serde_json::json!({})));
            println!("powering off...");
        }
        "reboot" => {
            pretty(call(s::CORE, "reboot", serde_json::json!({})));
            println!("rebooting...");
        }
        "hostname" if args.len() > 1 => pretty(call(
            s::CORE,
            "config.set",
            serde_json::json!({"key":"hostname","value":args[1]}),
        )),
        "agents" => pretty(call(s::RUNTIME, "agents.list", serde_json::json!({}))),
        "agent-start" if args.len() > 1 => pretty(call(
            s::RUNTIME,
            "agents.start",
            serde_json::json!({"id": args[1]}),
        )),
        "agent-stop" if args.len() > 1 => pretty(call(
            s::RUNTIME,
            "agents.stop",
            serde_json::json!({"id": args[1]}),
        )),
        "goal" if args.len() > 1 => pretty(call(
            s::ORCHESTRATOR,
            "goals.submit",
            serde_json::json!({"text": args[1..].join(" ")}),
        )),
        "goals" => pretty(call(s::ORCHESTRATOR, "goals.list", serde_json::json!({}))),
        "tasks" => pretty(call(s::ORCHESTRATOR, "tasks.list", serde_json::json!({}))),
        "perms" => pretty(call(s::PERMISSIONS, "policy.get", serde_json::json!({}))),
        "grants" => pretty(call(s::PERMISSIONS, "grants.list", serde_json::json!({}))),
        "models" => pretty(call(s::MODELS, "models.list", serde_json::json!({}))),
        "route" if args.len() > 1 => pretty(call(
            s::MODELS,
            "models.route",
            serde_json::json!({"text": args[1..].join(" "), "capability": "text.echo"}),
        )),
        "bus" => pretty(call(s::BUS, "bus.recent", serde_json::json!({"limit": 20}))),
        "install" if args.len() > 1 => pretty(call(
            s::APM,
            "apm.install",
            serde_json::json!({"path": args[1]}),
        )),
        "packages" => pretty(call(s::APM, "apm.list", serde_json::json!({}))),
        "help" | "--help" | "-h" => help(),
        other => {
            eprintln!("unknown command: {other}");
            help();
            exit(2);
        }
    }
}

fn help() {
    println!(
        "agentos-ctl — AgentOS operator CLI (v{})",
        agentos_common::VERSION
    );
    println!(
        "\
commands:
  status                 system status (version, kernel, uptime, services)
  services               service supervision table
  sysinfo                cpu / memory info
  restart <svc>          restart a system service
  stop-service <svc>     stop a system service
  shutdown | reboot      power the machine off / reboot
  hostname <name>        set system hostname
  agents                 list installed agents and lifecycle states
  agent-start <id>       start an agent (mock executor in Phase 1)
  agent-stop <id>        stop a running agent
  goal <text>            submit a goal to the orchestrator
  goals | tasks          list goals / tasks
  perms | grants         permission policy / active grants
  models | route <text>  model providers / route a test request
  bus                    recent agent-bus traffic
  install <dir>          install a .agent package directory
  packages               installed packages"
    );
}
