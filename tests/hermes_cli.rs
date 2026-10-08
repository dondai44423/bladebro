//! Native subprocess coverage of the PUBLIC Hermes CLI contract on every CI
//! platform. tools/hermes_probe additionally checks this against actual Hermes.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

fn read(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn write(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn fake(args: &[String]) -> i32 {
    let mut home = PathBuf::from(std::env::var_os("HERMES_HOME").unwrap());
    let mut args = args;
    if args.first().map(String::as_str) == Some("-p") {
        home = home.join("profiles").join(&args[1]);
        args = &args[2..];
    }
    let path = home.join("config.yaml");
    if args.iter().map(String::as_str).collect::<Vec<_>>() == ["config", "path"] {
        println!("{}", path.display());
        return 0;
    }
    if args.first().map(String::as_str) == Some("mcp") {
        return i32::from(home.join("fail-probe").exists());
    }
    let mut c = read(&path);
    let key = &args[2];
    let parts: Vec<_> = key.split('.').collect();
    let mut node = &mut c;
    for part in &parts[..parts.len() - 1] {
        if node.get(*part).is_none() {
            node[*part] = json!({});
        }
        node = &mut node[*part];
    }
    let leaf = parts[parts.len() - 1];
    match args[1].as_str() {
        "get" => {
            if home.join("bad-read").exists() {
                println!("{{}}");
                eprintln!("invalid YAML; using defaults");
            } else if let Some(v) = node.get(leaf) {
                println!("{v}");
            } else {
                eprintln!("Config key not set: {key}");
                return 1;
            }
        }
        "set" => {
            if key == "agent.disabled_toolsets" && home.join("lying-writer").exists() {
                return 0;
            }
            node[leaf] = serde_json::from_str(&args[3]).unwrap();
            write(&path, &c);
        }
        "unset" => {
            node.as_object_mut().unwrap().remove(leaf);
            write(&path, &c);
        }
        _ => panic!("unexpected public CLI operation: {args:?}"),
    }
    0
}

struct Fixture {
    root: PathBuf,
    checks: usize,
}

impl Fixture {
    fn check(&mut self, ok: bool, name: &str) {
        assert!(ok, "{name}");
        self.checks += 1;
        println!("PASS {name}");
    }

    fn run(&mut self, args: &[&str], ok: bool) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_bladebro"))
            .args(["hermes", "--json", "--hermes"])
            .arg(std::env::current_exe().unwrap())
            .args(args)
            .env("HERMES_HOME", &self.root)
            .current_dir(&self.root)
            .env("BLADE_HOME", "blade")
            .env("BLADE_NO_UPDATE_CHECK", "1")
            .env("CHROME_PATH", "explicit-chrome")
            .output()
            .unwrap();
        self.check(
            out.status.success() == ok,
            &format!(
                "{args:?} exit status; stderr={}",
                String::from_utf8_lossy(&out.stderr)
            ),
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        self.check(
            v["ok"] == ok && v["is_error"] == !ok,
            "one honest JSON result",
        );
        self.check(
            !String::from_utf8_lossy(&out.stdout).contains("private-sentinel"),
            "prior server secrets are never printed",
        );
        v
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("config" | "mcp" | "-p")
    ) {
        std::process::exit(fake(&args));
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "blade Hermes Δ space-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("config.yaml");
    let baseline = json!({"agent":{"disabled_toolsets":["tts"]}, "mcp_servers":{"keep":{"enabled":false}}, "browser":{"backend":"off"}});
    write(&path, &baseline);
    let mut f = Fixture { root, checks: 0 };
    f.run(&["on"], true);
    let installed = read(&path)["mcp_servers"]["bladebro"].clone();
    f.check(
        installed["args"] == json!(["mcp"]),
        "ordinary MCP command configured",
    );
    f.check(
        read(&path)["agent"]["disabled_toolsets"] == json!(["tts", "browser"]),
        "only browser suppression added",
    );
    f.check(
        installed["env"]["BLADE_HOME"] == f.root.join("blade").to_str().unwrap(),
        "explicit data root reaches MCP despite Hermes environment filtering",
    );
    f.check(
        installed["env"]["CHROME_PATH"] == f.root.join("explicit-chrome").to_str().unwrap(),
        "explicit browser path reaches MCP",
    );
    let recovery = f.root.join(".bladebro-browser/state.json");
    let saved = std::fs::read(&recovery).unwrap();
    f.run(&["on"], true);
    f.check(
        std::fs::read(&recovery).unwrap() == saved,
        "on retains first restore point",
    );
    let status = f.run(&["status"], true);
    f.check(
        status["text"]
            .as_str()
            .unwrap()
            .contains("Bladebro configured"),
        "status uses persisted state",
    );
    let mut edited = read(&path);
    edited["agent"]["disabled_toolsets"] = json!(["tts", "browser", "memory"]);
    write(&path, &edited);
    f.run(&["off"], true);
    f.check(
        read(&path)["agent"]["disabled_toolsets"] == json!(["tts", "memory"]),
        "restore keeps later unrelated changes",
    );
    f.check(
        read(&path)["mcp_servers"] == baseline["mcp_servers"],
        "restore preserves other MCP servers",
    );
    f.check(
        !recovery.exists(),
        "restore point consumed after verified writes",
    );
    f.run(&["off"], true);

    let mut prior = baseline.clone();
    prior["agent"]["disabled_toolsets"] = json!(["tts", "browser"]);
    prior["mcp_servers"]["bladebro"] = json!({"enabled":false,"command":"old", "env":{"TOKEN":"private-sentinel","CHROME_PATH":"configured-chrome"}});
    write(&path, &prior);
    f.run(&["on"], true);
    f.check(
        read(&path)["mcp_servers"]["bladebro"]["env"]["TOKEN"] == "private-sentinel"
            && read(&path)["mcp_servers"]["bladebro"]["env"]["CHROME_PATH"] == "configured-chrome",
        "prior server environment survives the switch",
    );
    f.run(&["off"], true);
    f.check(
        read(&path) == prior,
        "prior custom server and prior suppression restored exactly",
    );

    let mut oversized = baseline.clone();
    oversized["mcp_servers"]["bladebro"] =
        json!({"enabled":false,"env":{"LARGE":"x".repeat(140 * 1024)}});
    write(&path, &oversized);
    f.run(&["on"], false);
    f.check(
        read(&path) == oversized && !recovery.exists(),
        "oversized recovery refuses before any settings or restore point write",
    );

    write(&path, &baseline);
    std::fs::write(f.root.join("fail-probe"), "").unwrap();
    f.run(&["on"], false);
    f.check(
        read(&path)["agent"] == baseline["agent"],
        "failed MCP probe leaves native browser enabled",
    );
    std::fs::remove_file(f.root.join("fail-probe")).unwrap();
    f.run(&["on"], true);
    f.run(&["off"], true);
    f.check(read(&path) == baseline, "failed setup resumes and restores");

    std::fs::write(f.root.join("lying-writer"), "").unwrap();
    f.run(&["on"], false);
    f.check(
        read(&path)["agent"] == baseline["agent"],
        "false-success writer detected by readback",
    );
    std::fs::remove_file(f.root.join("lying-writer")).unwrap();
    f.run(&["off"], true);

    f.run(&["on"], true);
    let mut conflict = read(&path);
    conflict["mcp_servers"]["bladebro"]["command"] = json!("user-edited");
    write(&path, &conflict);
    f.run(&["off"], false);
    f.check(
        read(&path) == conflict && recovery.exists(),
        "edited MCP entry refuses restore without writes",
    );
    conflict["mcp_servers"]["bladebro"] = installed;
    write(&path, &conflict);
    f.run(&["off"], true);

    std::fs::write(f.root.join("bad-read"), "").unwrap();
    f.run(&["on"], false);
    f.check(
        read(&path) == baseline && !recovery.exists(),
        "fallback defaults never authorize writes",
    );
    std::fs::remove_file(f.root.join("bad-read")).unwrap();
    let profile = f.root.join("profiles").join("work");
    std::fs::create_dir_all(&profile).unwrap();
    write(&profile.join("config.yaml"), &baseline);
    f.run(&["on", "--profile", "work"], true);
    f.check(
        read(&path) == baseline,
        "named profile does not change default config",
    );
    f.check(
        read(&profile.join("config.yaml"))["agent"]["disabled_toolsets"]
            == json!(["tts", "browser"]),
        "named profile receives browser switch",
    );
    f.run(&["off", "--profile", "work"], true);
    f.check(
        read(&profile.join("config.yaml")) == baseline,
        "named profile restores independently",
    );
    f.run(&["--bogus"], false);
    println!("PASS Hermes native CLI contract: {} assertions", f.checks);
}
