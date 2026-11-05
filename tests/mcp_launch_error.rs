//! Launch failures must remain valid tool results in both MCP dialects.
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn launch_errors_preserve_dialect_and_notification_silence() {
    let root = std::env::temp_dir().join(format!("blade-mcp-launch-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for version in [Some("2026-07-28"), None] {
        let mut params = json!({"name":"see","arguments":{"mode":"content"}});
        if let Some(version) = version {
            params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":version});
        }
        let requests = [
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":params}),
            json!({"jsonrpc":"2.0","method":"tools/call","params":params}),
            json!({"jsonrpc":"2.0","id":2,"method":"ping","params":{}}),
        ];
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bladebro"))
            .arg("mcp")
            .env(
                "BLADE_HOME",
                root.join(if version.is_some() { "new" } else { "legacy" }),
            )
            .env("BLADE_LANE", "agent")
            .env("BLADE_NO_WARMING", "1")
            .env("BLADE_NO_UPDATE_CHECK", "1")
            .env("CHROME_PATH", root.join("missing-chrome"))
            .env_remove("BLADE_TRANSPORT")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        for request in requests {
            input
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
        }
        drop(input);
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(15), child.wait_with_output())
                .await
                .expect("invalid explicit Chrome path must fail promptly")
                .unwrap();
        assert!(output.status.success(), "{:?}", output);
        let responses: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let result = &responses[0]["result"];
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("CHROME_PATH"));
        if version.is_some() {
            assert_eq!(result["resultType"], "complete", "{responses:?}");
            assert_eq!(
                result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
                "bladebro"
            );
        } else {
            assert!(result.get("resultType").is_none());
        }
        assert_eq!(
            responses.len(),
            2,
            "notifications must be silent: {responses:?}"
        );
        assert_eq!(responses[1]["id"], 2, "server survives launch failure");
    }
    std::fs::remove_dir_all(root).unwrap();
}
