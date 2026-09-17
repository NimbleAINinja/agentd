//! Drives the opencode plugin template in Bun with a recording stand-in for
//! the Agentd executable. Skipped (with a notice) when `bun` is not on PATH.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const TEMPLATE: &str = include_str!("../src/opencode_plugin.js");

const DRIVER: &str = r#"
const { AgentdIntegration } = await import(process.argv[2]);
const hooks = await AgentdIntegration({});
const pause = () => new Promise((resolve) => setTimeout(resolve, 150));
const event = (type, properties = {}) => hooks.event({ event: { type, properties } });

await hooks["chat.message"]({ sessionID: "s1" }, {}); await pause();
await hooks["tool.execute.before"]({}, {}); await pause();
await event("session.status", { sessionID: "s1", status: { type: "busy" } }); await pause();
await event("session.status", { sessionID: "s2", status: { type: "busy" } }); await pause();
await event("session.idle", { sessionID: "s2" }); await pause();
await event("permission.updated", { id: "p1" }); await pause();
await event("permission.replied", { permissionID: "p1" }); await pause();
await event("session.status", { sessionID: "s1", status: { type: "busy" } }); await pause();
await event("session.status", { sessionID: "s1", status: { type: "idle" } }); await pause();
await event("session.idle", { sessionID: "s1" }); await pause();
await event("question.asked", { id: "q1" }); await pause();
await event("message.part.updated", {}); await pause();
await event("session.idle"); await pause();
"#;

#[test]
fn plugin_translates_opencode_events_into_hook_calls() {
    let Some(bun) = find_bun() else {
        eprintln!("skipping: bun is not on PATH");
        return;
    };
    let directory = env::temp_dir().join(format!("agentd-opencode-plugin-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir(&directory).unwrap();
    let log = directory.join("calls.log");
    let recorder = directory.join("agentd");
    fs::write(
        &recorder,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display()),
    )
    .unwrap();
    fs::set_permissions(&recorder, fs::Permissions::from_mode(0o755)).unwrap();
    let plugin = directory.join("agentd.js");
    fs::write(
        &plugin,
        TEMPLATE.replace(
            "__AGENTD_EXECUTABLE__",
            &serde_json::to_string(recorder.to_str().unwrap()).unwrap(),
        ),
    )
    .unwrap();
    let driver = directory.join("driver.mjs");
    fs::write(&driver, DRIVER).unwrap();

    let status = Command::new(bun)
        .arg(&driver)
        .arg(&plugin)
        .status()
        .unwrap();
    assert!(status.success());

    let calls = fs::read_to_string(&log).unwrap();
    let prefix = "hook --integration agentd-v1.1 --harness opencode --event ";
    let events: Vec<&str> = calls
        .lines()
        .map(|line| line.strip_prefix(prefix).expect(line))
        .collect();
    assert_eq!(
        events,
        [
            "PromptSubmit",
            "PermissionAsked",
            "Replied",
            "Busy",
            "Idle",
            "QuestionAsked",
            "Idle",
        ]
    );
    fs::remove_dir_all(&directory).unwrap();
}

fn find_bun() -> Option<std::path::PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join("bun"))
        .find(|candidate| candidate.is_file())
}
