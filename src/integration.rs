use crate::model::ActivityState;
use crate::protocol::parse_json_value;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const INTEGRATION_MARKER: &str = "agentd-v1.1";
const VERIFIED_CODEX_VERSION: &str = "codex-cli 0.149.1";
const VERIFIED_OPENCODE_VERSION: &str = "1.18.31";
const OPENCODE_PLUGIN_TEMPLATE: &str = include_str!("opencode_plugin.js");
const OPENCODE_PLUGIN_HEADER: &str = "// agentd-integration agentd-v1.1 harness=opencode\n";
const OPENCODE_EXECUTABLE_PLACEHOLDER: &str = "__AGENTD_EXECUTABLE__";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Install,
    Uninstall,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IntegrationHarness {
    Claude,
    Codex,
    Opencode,
}

impl IntegrationHarness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }

    fn events(self) -> &'static [(&'static str, ActivityState)] {
        crate::hook::mappings_for(match self {
            Self::Claude => crate::model::Harness::Claude,
            Self::Codex => crate::model::Harness::Codex,
            Self::Opencode => crate::model::Harness::Opencode,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ConfigurationRead {
    Absent,
    Present {
        bytes: Vec<u8>,
        mode: u32,
        owner_uid: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NotRemoved {
    path: String,
    event: String,
}

#[derive(Debug)]
struct MutationResult {
    changed: bool,
    not_removed: Vec<NotRemoved>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OwnedDeclaration {
    harness: IntegrationHarness,
    event: String,
}

pub fn run(action: &str, harness: &str) -> Result<(), String> {
    let action = match action {
        "install" => Action::Install,
        "uninstall" => Action::Uninstall,
        _ => return Err("agentd integrate: invalid_action".to_owned()),
    };
    let harness = match harness {
        "claude" => IntegrationHarness::Claude,
        "codex" => IntegrationHarness::Codex,
        "opencode" => IntegrationHarness::Opencode,
        _ => return Err("agentd integrate: invalid_harness".to_owned()),
    };
    let executable = env::current_exe()
        .and_then(fs::canonicalize)
        .ok()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "agentd integrate: unresolved_agentd_executable".to_owned())?;
    if executable.to_str().is_none() {
        return Err("agentd integrate: unresolved_agentd_executable".to_owned());
    }
    if harness == IntegrationHarness::Opencode {
        return run_opencode(action, &executable);
    }
    let target = target_path(harness)?;
    validate_configuration_directory(&target)?;

    let codex_version = if action == Action::Install && harness == IntegrationHarness::Codex {
        Some(check_codex_hooks()?)
    } else {
        None
    };

    let result = mutate_configuration(&target, |root| merge(root, action, harness, &executable))?;
    print_result(action, harness, &target, &result, codex_version.as_deref());
    Ok(())
}

fn target_path(harness: IntegrationHarness) -> Result<PathBuf, String> {
    let (variable, default_suffix, file_name) = match harness {
        IntegrationHarness::Claude => ("CLAUDE_CONFIG_DIR", ".claude", "settings.json"),
        IntegrationHarness::Codex => ("CODEX_HOME", ".codex", "hooks.json"),
        IntegrationHarness::Opencode => {
            unreachable!("opencode is integrated through a plugin file")
        }
    };
    let directory = match env::var_os(variable) {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(format!(
                    "agentd integrate: relative_configuration_directory variable={variable}"
                ));
            }
            path
        }
        None => {
            let home = env::var_os("HOME")
                .ok_or_else(|| "agentd integrate: home_directory_unavailable".to_owned())?;
            PathBuf::from(home).join(default_suffix)
        }
    };
    Ok(directory.join(file_name))
}

fn validate_configuration_directory(target: &Path) -> Result<(), String> {
    let directory = target
        .parent()
        .ok_or_else(|| "agentd integrate: invalid_configuration_directory".to_owned())?;
    let metadata = fs::metadata(directory).map_err(|error| {
        format!(
            "agentd integrate: invalid_configuration_directory path={} cause={error}",
            directory.display()
        )
    })?;
    let effective_uid = unsafe { libc::geteuid() as u32 };
    if !metadata.is_dir() || metadata.uid() != effective_uid {
        return Err(format!(
            "agentd integrate: invalid_configuration_directory path={}",
            directory.display()
        ));
    }
    Ok(())
}

fn check_codex_hooks() -> Result<String, String> {
    let version = Command::new("codex")
        .arg("--version")
        .output()
        .map_err(|_| "agentd integrate: unsupported_codex_hooks".to_owned())?;
    if !version.status.success() {
        return Err("agentd integrate: unsupported_codex_hooks".to_owned());
    }
    let version = String::from_utf8(version.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "agentd integrate: unsupported_codex_hooks".to_owned())?;
    let features = Command::new("codex")
        .args(["features", "list"])
        .output()
        .map_err(|_| "agentd integrate: unsupported_codex_hooks".to_owned())?;
    if !features.status.success() {
        return Err("agentd integrate: unsupported_codex_hooks".to_owned());
    }
    let features = String::from_utf8(features.stdout)
        .map_err(|_| "agentd integrate: unsupported_codex_hooks".to_owned())?;
    let enabled = features.lines().any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        fields.len() == 3 && fields[0] == "hooks" && fields[1] == "stable" && fields[2] == "true"
    });
    if !enabled {
        return Err("agentd integrate: unsupported_codex_hooks".to_owned());
    }
    Ok(version)
}

fn mutate_configuration<F>(target: &Path, merge_value: F) -> Result<MutationResult, String>
where
    F: Fn(&mut Value) -> Result<Vec<NotRemoved>, String>,
{
    mutate_configuration_observed(target, merge_value, |_, _| {})
}

fn mutate_configuration_observed<F, O>(
    target: &Path,
    merge_value: F,
    mut before_reread: O,
) -> Result<MutationResult, String>
where
    F: Fn(&mut Value) -> Result<Vec<NotRemoved>, String>,
    O: FnMut(usize, &Path),
{
    let effective_uid = unsafe { libc::geteuid() as u32 };
    for attempt in 0..=1 {
        let baseline = read_configuration(target, effective_uid)?;
        let mut value = configuration_value(target, &baseline)?;
        let original = value.clone();
        let not_removed = merge_value(&mut value)?;
        if value == original {
            return Ok(MutationResult {
                changed: false,
                not_removed,
            });
        }

        let mut serialized = serde_json::to_vec_pretty(&value)
            .map_err(|error| format!("agentd integrate: serialize_configuration cause={error}"))?;
        serialized.push(b'\n');
        let mut candidate = TemporaryCandidate::create(target)?;
        candidate.write_and_flush(&serialized)?;
        before_reread(attempt, target);
        let current = read_configuration(target, effective_uid)?;
        if current != baseline {
            drop(candidate);
            if attempt == 0 {
                continue;
            }
            return Err(format!(
                "agentd integrate: configuration_changed path={}",
                target.display()
            ));
        }
        candidate.preserve_target_mode(&baseline)?;
        candidate.commit(target)?;
        return Ok(MutationResult {
            changed: true,
            not_removed,
        });
    }
    unreachable!("the bounded mutation loop returns on its second attempt")
}

fn read_configuration(target: &Path, effective_uid: u32) -> Result<ConfigurationRead, String> {
    let entry = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ConfigurationRead::Absent);
        }
        Err(error) => {
            return Err(format!(
                "agentd integrate: configuration_read path={} cause={error}",
                target.display()
            ));
        }
    };
    if !entry.file_type().is_file()
        || entry.file_type().is_symlink()
        || entry.uid() != effective_uid
    {
        return Err(format!(
            "agentd integrate: unsupported_configuration_target path={}",
            target.display()
        ));
    }

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(target)
        .map_err(|error| {
            if matches!(error.raw_os_error(), Some(libc::ELOOP)) {
                format!(
                    "agentd integrate: unsupported_configuration_target path={}",
                    target.display()
                )
            } else {
                format!(
                    "agentd integrate: configuration_read path={} cause={error}",
                    target.display()
                )
            }
        })?;
    let opened = file.metadata().map_err(|error| {
        format!(
            "agentd integrate: configuration_read path={} cause={error}",
            target.display()
        )
    })?;
    if !opened.file_type().is_file()
        || opened.uid() != effective_uid
        || opened.dev() != entry.dev()
        || opened.ino() != entry.ino()
    {
        return Err(format!(
            "agentd integrate: configuration_changed path={}",
            target.display()
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        format!(
            "agentd integrate: configuration_read path={} cause={error}",
            target.display()
        )
    })?;
    Ok(ConfigurationRead::Present {
        bytes,
        mode: entry.mode() & 0o7777,
        owner_uid: entry.uid(),
    })
}

fn configuration_value(target: &Path, read: &ConfigurationRead) -> Result<Value, String> {
    match read {
        ConfigurationRead::Absent => Ok(Value::Object(Map::new())),
        ConfigurationRead::Present { bytes, .. } => {
            let value = parse_json_value(bytes).map_err(|_| {
                format!(
                    "agentd integrate: malformed_configuration path={}",
                    target.display()
                )
            })?;
            if value.is_object() {
                Ok(value)
            } else {
                Err(format!(
                    "agentd integrate: invalid_configuration_shape path={}",
                    target.display()
                ))
            }
        }
    }
}

fn merge(
    root: &mut Value,
    action: Action,
    harness: IntegrationHarness,
    executable: &Path,
) -> Result<Vec<NotRemoved>, String> {
    validate_hooks_shape(root)?;
    match action {
        Action::Install => install_entries(root, harness, executable),
        Action::Uninstall => uninstall_entries(root, harness),
    }
}

fn validate_hooks_shape(root: &Value) -> Result<(), String> {
    let root = root
        .as_object()
        .ok_or_else(|| "agentd integrate: invalid_configuration_shape member=root".to_owned())?;
    let Some(hooks) = root.get("hooks") else {
        return Ok(());
    };
    let hooks = hooks
        .as_object()
        .ok_or_else(|| "agentd integrate: invalid_configuration_shape member=hooks".to_owned())?;
    for (event, groups) in hooks {
        let groups = groups.as_array().ok_or_else(|| {
            format!("agentd integrate: invalid_configuration_shape member=hooks.{event}")
        })?;
        for group in groups {
            let group = group.as_object().ok_or_else(|| {
                format!("agentd integrate: invalid_configuration_shape member=hooks.{event}[]")
            })?;
            let handlers = group.get("hooks").ok_or_else(|| {
                format!(
                    "agentd integrate: invalid_configuration_shape member=hooks.{event}[].hooks"
                )
            })?;
            let handlers = handlers.as_array().ok_or_else(|| {
                format!(
                    "agentd integrate: invalid_configuration_shape member=hooks.{event}[].hooks"
                )
            })?;
            if handlers.iter().any(|handler| !handler.is_object()) {
                return Err(format!(
                    "agentd integrate: invalid_configuration_shape member=hooks.{event}[].hooks[]"
                ));
            }
        }
    }
    Ok(())
}

fn install_entries(
    root: &mut Value,
    harness: IntegrationHarness,
    executable: &Path,
) -> Result<Vec<NotRemoved>, String> {
    let hooks = hooks_object(root);
    let mut retained_events = HashSet::new();
    let existing_events: Vec<String> = hooks.keys().cloned().collect();
    for outer_event in existing_events {
        let groups = hooks
            .get_mut(&outer_event)
            .and_then(Value::as_array_mut)
            .expect("validated hook event must remain an array");
        let mut group_index = 0;
        while group_index < groups.len() {
            let group = groups[group_index]
                .as_object_mut()
                .expect("validated matcher group must remain an object");
            let is_plain_group = group.len() == 1;
            let handlers = group
                .get_mut("hooks")
                .and_then(Value::as_array_mut)
                .expect("validated handlers must remain an array");
            let sole_owned = (is_plain_group && handlers.len() == 1)
                .then(|| handler_command(&handlers[0]).and_then(owned_declaration))
                .flatten();
            if let Some(owned) = sole_owned
                && owned.harness == harness
                && owned.event == outer_event
                && retained_events.insert(outer_event.clone())
            {
                handlers[0] = handler(harness, &outer_event, executable);
                group_index += 1;
                continue;
            }

            handlers.retain(|handler| {
                !handler_command(handler)
                    .and_then(owned_declaration)
                    .is_some_and(|owned| owned.harness == harness)
            });
            if handlers.is_empty() && is_plain_group {
                groups.remove(group_index);
                continue;
            }
            group_index += 1;
        }
        if groups.is_empty() {
            hooks.remove(&outer_event);
        }
    }

    for (event, _) in harness.events() {
        if retained_events.contains(*event) {
            continue;
        }
        hooks
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("validated hook event must remain an array")
            .push(json!({"hooks": [handler(harness, event, executable)]}));
    }
    Ok(collect_not_removed(root, harness))
}

fn uninstall_entries(
    root: &mut Value,
    harness: IntegrationHarness,
) -> Result<Vec<NotRemoved>, String> {
    let not_removed = collect_not_removed(root, harness);
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(not_removed);
    };
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let groups = hooks
            .get_mut(&event)
            .and_then(Value::as_array_mut)
            .expect("validated hook event must remain an array");
        let mut group_index = 0;
        while group_index < groups.len() {
            let group = groups[group_index]
                .as_object_mut()
                .expect("validated matcher group must remain an object");
            let handlers = group
                .get_mut("hooks")
                .and_then(Value::as_array_mut)
                .expect("validated handlers must remain an array");
            handlers.retain(|handler| {
                !handler_command(handler)
                    .and_then(owned_declaration)
                    .is_some_and(|owned| owned.harness == harness)
            });
            if handlers.is_empty() && group.len() == 1 {
                groups.remove(group_index);
                continue;
            }
            group_index += 1;
        }
        if groups.is_empty() {
            hooks.remove(&event);
        }
    }
    Ok(not_removed)
}

fn hooks_object(root: &mut Value) -> &mut Map<String, Value> {
    root.as_object_mut()
        .expect("configuration root was validated")
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("hooks value was validated")
}

fn handler(harness: IntegrationHarness, event: &str, executable: &Path) -> Value {
    let command = command_line(executable, harness, event);
    match harness {
        IntegrationHarness::Claude => {
            json!({"type": "command", "command": command, "timeout": 1})
        }
        IntegrationHarness::Codex => {
            json!({"type": "command", "command": command, "timeout": 1, "async": false})
        }
        IntegrationHarness::Opencode => {
            unreachable!("opencode is integrated through a plugin file")
        }
    }
}

fn command_line(executable: &Path, harness: IntegrationHarness, event: &str) -> String {
    format!(
        "{} hook --integration {INTEGRATION_MARKER} --harness {} --event {event}",
        shell_words::quote(
            executable
                .to_str()
                .expect("the executable path was validated as UTF-8"),
        ),
        harness.as_str()
    )
}

fn handler_command(handler: &Value) -> Option<&str> {
    let handler = handler.as_object()?;
    (handler.get("type").and_then(Value::as_str) == Some("command"))
        .then(|| handler.get("command").and_then(Value::as_str))
        .flatten()
}

fn owned_declaration(command: &str) -> Option<OwnedDeclaration> {
    if contains_shell_operator_or_expansion(command) {
        return None;
    }
    let words = shell_words::split(command).ok()?;
    if words.len() != 8
        || !Path::new(&words[0]).is_absolute()
        || words[1] != "hook"
        || words[2] != "--integration"
        || words[3] != INTEGRATION_MARKER
        || words[4] != "--harness"
        || words[6] != "--event"
    {
        return None;
    }
    let harness = match words[5].as_str() {
        "claude" => IntegrationHarness::Claude,
        "codex" => IntegrationHarness::Codex,
        _ => return None,
    };
    let event = words[7].clone();
    if harness
        .events()
        .iter()
        .any(|(allowed, _)| *allowed == event)
    {
        Some(OwnedDeclaration { harness, event })
    } else {
        None
    }
}

fn contains_shell_operator_or_expansion(command: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Quote {
        Unquoted,
        Single,
        Double,
    }
    let mut quote = Quote::Unquoted;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Quote::Unquoted => match character {
                '\\' => escaped = true,
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                ';' | '|' | '&' | '<' | '>' | '$' | '`' | '*' | '?' | '[' | '\n' | '\r' => {
                    return true;
                }
                _ => {}
            },
            Quote::Single => {
                if character == '\'' {
                    quote = Quote::Unquoted;
                }
            }
            Quote::Double => match character {
                '\\' => escaped = true,
                '"' => quote = Quote::Unquoted,
                '$' | '`' | '\n' | '\r' => return true,
                _ => {}
            },
        }
    }
    escaped || !matches!(quote, Quote::Unquoted)
}

fn collect_not_removed(root: &Value, harness: IntegrationHarness) -> Vec<NotRemoved> {
    let mut result = Vec::new();
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return result;
    };
    for (event, groups) in hooks {
        for group in groups.as_array().expect("validated event array") {
            for handler in group
                .get("hooks")
                .and_then(Value::as_array)
                .expect("validated handler array")
            {
                let Some(command) = handler_command(handler) else {
                    continue;
                };
                if owned_declaration(command).is_none()
                    && command.contains(INTEGRATION_MARKER)
                    && command.contains(&format!("--harness {}", harness.as_str()))
                {
                    let path = shell_words::split(command)
                        .ok()
                        .and_then(|words| words.first().cloned())
                        .unwrap_or_else(|| "unknown".to_owned());
                    result.push(NotRemoved {
                        path,
                        event: event.clone(),
                    });
                }
            }
        }
    }
    result
}

fn print_result(
    action: Action,
    harness: IntegrationHarness,
    target: &Path,
    result: &MutationResult,
    version: Option<&str>,
) {
    let result_name = if result.changed {
        "changed"
    } else {
        "unchanged"
    };
    let not_removed = serde_json::to_string(
        &result
            .not_removed
            .iter()
            .map(|entry| json!({"path": entry.path, "event": entry.event}))
            .collect::<Vec<_>>(),
    )
    .expect("not-removed report serialization cannot fail");
    let mut line = format!(
        "agentd integrate: agentd_version={} harness={} action={} result={result_name} target={} not_removed={not_removed}",
        env!("CARGO_PKG_VERSION"),
        harness.as_str(),
        action.as_str(),
        target.display()
    );
    if action == Action::Install {
        match harness {
            IntegrationHarness::Claude => line.push_str(
                " existing_process=kept_by_procfs activity=unchanged next_activity=accepted_mapped_hook_event activation=restart_only resume=\"claude --continue|claude --resume\"",
            ),
            IntegrationHarness::Codex => {
                line.push_str(
                    " existing_process=kept_by_procfs activity=unchanged next_activity=accepted_mapped_hook_event activation=restart_only resume=\"codex resume\" trust=next_interactive_startup_review",
                );
                if let Some(version) = version
                    && version != VERIFIED_CODEX_VERSION
                {
                    line.push_str(&format!(
                        " warning=unverified_codex_version version={version}"
                    ));
                }
            }
            IntegrationHarness::Opencode => {
                line.push_str(
                    " existing_process=kept_by_procfs activity=unchanged next_activity=accepted_mapped_plugin_event activation=restart_only resume=\"opencode --continue\"",
                );
                if let Some(version) = version
                    && version != VERIFIED_OPENCODE_VERSION
                {
                    line.push_str(&format!(
                        " warning=unverified_opencode_version version={version}"
                    ));
                }
            }
        }
    }
    println!("{line}");
}

fn run_opencode(action: Action, executable: &Path) -> Result<(), String> {
    let plugins = opencode_config_directory()?.join("plugins");
    // Validates the opencode configuration directory itself.
    validate_configuration_directory(&plugins)?;
    let target = plugins.join("agentd.js");
    let version = if action == Action::Install {
        let version = check_opencode()?;
        match fs::create_dir(&plugins) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "agentd integrate: invalid_configuration_directory path={} cause={error}",
                    plugins.display()
                ));
            }
        }
        Some(version)
    } else {
        None
    };
    let result = if action == Action::Uninstall && !plugins.exists() {
        MutationResult {
            changed: false,
            not_removed: Vec::new(),
        }
    } else {
        validate_configuration_directory(&target)?;
        let desired = match action {
            Action::Install => Some(opencode_plugin(executable)),
            Action::Uninstall => None,
        };
        mutate_opencode_plugin(&target, desired.as_deref(), |_, _| {})?
    };
    print_result(
        action,
        IntegrationHarness::Opencode,
        &target,
        &result,
        version.as_deref(),
    );
    Ok(())
}

fn opencode_config_directory() -> Result<PathBuf, String> {
    for (variable, suffix) in [("OPENCODE_CONFIG_DIR", ""), ("XDG_CONFIG_HOME", "opencode")] {
        if let Some(value) = env::var_os(variable) {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(format!(
                    "agentd integrate: relative_configuration_directory variable={variable}"
                ));
            }
            return Ok(path.join(suffix));
        }
    }
    let home = env::var_os("HOME")
        .ok_or_else(|| "agentd integrate: home_directory_unavailable".to_owned())?;
    Ok(PathBuf::from(home).join(".config").join("opencode"))
}

fn check_opencode() -> Result<String, String> {
    let output = Command::new("opencode")
        .arg("--version")
        .output()
        .map_err(|_| "agentd integrate: unsupported_opencode".to_owned())?;
    if !output.status.success() {
        return Err("agentd integrate: unsupported_opencode".to_owned());
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "agentd integrate: unsupported_opencode".to_owned())
}

fn opencode_plugin(executable: &Path) -> Vec<u8> {
    let executable = serde_json::to_string(
        executable
            .to_str()
            .expect("the executable path was validated as UTF-8"),
    )
    .expect("string serialization cannot fail");
    OPENCODE_PLUGIN_TEMPLATE
        .replace(OPENCODE_EXECUTABLE_PLACEHOLDER, &executable)
        .into_bytes()
}

/// Installs (`desired` is `Some`) or removes the Agentd-owned opencode
/// plugin. A file without the Agentd header belongs to someone else and is
/// never replaced or removed.
fn mutate_opencode_plugin<O>(
    target: &Path,
    desired: Option<&[u8]>,
    mut before_reread: O,
) -> Result<MutationResult, String>
where
    O: FnMut(usize, &Path),
{
    let effective_uid = unsafe { libc::geteuid() as u32 };
    for attempt in 0..=1 {
        let baseline = read_configuration(target, effective_uid)?;
        let unchanged = |not_removed| {
            Ok(MutationResult {
                changed: false,
                not_removed,
            })
        };
        match (&baseline, desired) {
            (ConfigurationRead::Absent, None) => return unchanged(Vec::new()),
            (ConfigurationRead::Present { bytes, .. }, Some(desired)) if bytes == desired => {
                return unchanged(Vec::new());
            }
            (ConfigurationRead::Present { bytes, .. }, _)
                if !bytes.starts_with(OPENCODE_PLUGIN_HEADER.as_bytes()) =>
            {
                if desired.is_some() {
                    return Err(format!(
                        "agentd integrate: unowned_configuration_target path={}",
                        target.display()
                    ));
                }
                return unchanged(vec![NotRemoved {
                    path: target.display().to_string(),
                    event: "plugin".to_owned(),
                }]);
            }
            _ => {}
        }

        let candidate = match desired {
            Some(desired) => {
                let mut candidate = TemporaryCandidate::create(target)?;
                candidate.write_and_flush(desired)?;
                Some(candidate)
            }
            None => None,
        };
        before_reread(attempt, target);
        if read_configuration(target, effective_uid)? != baseline {
            drop(candidate);
            if attempt == 0 {
                continue;
            }
            return Err(format!(
                "agentd integrate: configuration_changed path={}",
                target.display()
            ));
        }
        match candidate {
            Some(mut candidate) => {
                candidate.preserve_target_mode(&baseline)?;
                candidate.commit(target)?;
            }
            None => fs::remove_file(target).map_err(|error| {
                format!(
                    "agentd integrate: remove_plugin path={} cause={error}",
                    target.display()
                )
            })?,
        }
        return Ok(MutationResult {
            changed: true,
            not_removed: Vec::new(),
        });
    }
    unreachable!("the bounded mutation loop returns on its second attempt")
}

struct TemporaryCandidate {
    path: PathBuf,
    file: Option<File>,
}

impl TemporaryCandidate {
    fn create(target: &Path) -> Result<Self, String> {
        let directory = target
            .parent()
            .ok_or_else(|| "agentd integrate: invalid_configuration_directory".to_owned())?;
        let target_name = target
            .file_name()
            .ok_or_else(|| "agentd integrate: invalid_configuration_target".to_owned())?;
        for _ in 0..100 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut name = OsString::from(".");
            name.push(target_name);
            name.push(format!(".agentd-{}-{sequence}.tmp", std::process::id()));
            let path = directory.join(name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => {
                    if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                        drop(file);
                        let _ = fs::remove_file(&path);
                        return Err(format!(
                            "agentd integrate: candidate_mode path={} cause={error}",
                            path.display()
                        ));
                    }
                    return Ok(Self {
                        path,
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(format!(
                        "agentd integrate: create_candidate path={} cause={error}",
                        path.display()
                    ));
                }
            }
        }
        Err("agentd integrate: create_candidate unique_name_exhausted".to_owned())
    }

    fn write_and_flush(&mut self, bytes: &[u8]) -> Result<(), String> {
        let file = self.file.as_mut().expect("candidate file is open");
        file.write_all(bytes).map_err(|error| {
            format!(
                "agentd integrate: write_candidate path={} cause={error}",
                self.path.display()
            )
        })?;
        file.sync_all().map_err(|error| {
            format!(
                "agentd integrate: flush_candidate path={} cause={error}",
                self.path.display()
            )
        })
    }

    fn preserve_target_mode(&mut self, baseline: &ConfigurationRead) -> Result<(), String> {
        if let ConfigurationRead::Present { mode, .. } = baseline {
            self.file
                .as_ref()
                .expect("candidate file is open")
                .set_permissions(fs::Permissions::from_mode(*mode))
                .map_err(|error| {
                    format!(
                        "agentd integrate: candidate_mode path={} cause={error}",
                        self.path.display()
                    )
                })?;
        }
        Ok(())
    }

    fn commit(&mut self, target: &Path) -> Result<(), String> {
        drop(self.file.take());
        fs::rename(&self.path, target).map_err(|error| {
            format!(
                "agentd integrate: atomic_rename path={} cause={error}",
                target.display()
            )
        })?;
        self.path = PathBuf::new();
        Ok(())
    }
}

impl Drop for TemporaryCandidate {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "agentd-integration-{label}-{}-{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn claude_install_and_uninstall_are_byte_idempotent_and_preserve_order() {
        exercise_idempotence(IntegrationHarness::Claude, "settings.json");
    }

    #[test]
    fn codex_install_and_uninstall_are_byte_idempotent_and_preserve_trust_file() {
        exercise_idempotence(IntegrationHarness::Codex, "hooks.json");
    }

    #[test]
    fn no_follow_read_refuses_a_symlink_without_changing_its_referent() {
        let directory = TestDir::new("symlink");
        let referent = directory.0.join("referent.json");
        let target = directory.0.join("settings.json");
        fs::write(&referent, b"{\"keep\":true}\n").unwrap();
        symlink(&referent, &target).unwrap();
        let before = fs::read(&referent).unwrap();
        let error = mutate_configuration(&target, |root| {
            merge(
                root,
                Action::Install,
                IntegrationHarness::Claude,
                Path::new("/opt/agentd"),
            )
        })
        .unwrap_err();
        assert!(error.contains("unsupported_configuration_target"));
        assert_eq!(fs::read(&referent).unwrap(), before);
        assert!(
            fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn concurrent_change_is_remerged_once_and_a_second_change_is_refused() {
        let directory = TestDir::new("concurrent");
        let target = directory.0.join("settings.json");
        fs::write(&target, b"{\"generation\":0}\n").unwrap();
        let result = mutate_configuration_observed(
            &target,
            |root| {
                merge(
                    root,
                    Action::Install,
                    IntegrationHarness::Claude,
                    Path::new("/opt/agentd"),
                )
            },
            |attempt, path| {
                if attempt == 0 {
                    fs::write(path, b"{\"generation\":1}\n").unwrap();
                }
            },
        )
        .unwrap();
        assert!(result.changed);
        let merged: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
        assert_eq!(merged["generation"], 1);
        assert_eq!(count_owned(&target, IntegrationHarness::Claude), 4);

        fs::write(&target, b"{\"generation\":0}\n").unwrap();
        let error = mutate_configuration_observed(
            &target,
            |root| {
                merge(
                    root,
                    Action::Install,
                    IntegrationHarness::Claude,
                    Path::new("/opt/agentd"),
                )
            },
            |attempt, path| {
                fs::write(path, format!("{{\"generation\":{}}}\n", attempt + 1)).unwrap();
            },
        )
        .unwrap_err();
        assert!(error.contains("configuration_changed"));
        assert_eq!(fs::read(&target).unwrap(), b"{\"generation\":2}\n");
        assert_eq!(
            fs::read_dir(&directory.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            vec![OsString::from("settings.json")]
        );
    }

    #[test]
    fn owned_parser_rejects_expansion_and_extra_arguments() {
        assert!(
            owned_declaration(
                "'/opt/agent d' hook --integration agentd-v1.1 --harness claude --event Stop"
            )
            .is_some()
        );
        assert!(
            owned_declaration(
                "$AGENTD hook --integration agentd-v1.1 --harness claude --event Stop"
            )
            .is_none()
        );
        assert!(
            owned_declaration(
                "/opt/agentd hook --integration agentd-v1.1 --harness claude --event Stop --extra"
            )
            .is_none()
        );
    }

    #[test]
    fn opencode_plugin_install_and_uninstall_are_idempotent() {
        let directory = TestDir::new("opencode");
        let target = directory.0.join("agentd.js");
        let plugin = opencode_plugin(Path::new("/opt/agent d/agentd"));
        let text = String::from_utf8(plugin.clone()).unwrap();
        assert!(text.starts_with(OPENCODE_PLUGIN_HEADER));
        assert!(text.contains(r#"const AGENTD = "/opt/agent d/agentd";"#));
        assert!(!text.contains(OPENCODE_EXECUTABLE_PLACEHOLDER));
        for (event, _) in IntegrationHarness::Opencode.events() {
            assert!(
                text.contains(&format!("\"{event}\"")),
                "plugin never fires {event}"
            );
        }

        let install = |bytes: &[u8]| mutate_opencode_plugin(&target, Some(bytes), |_, _| {});
        assert!(install(&plugin).unwrap().changed);
        assert_eq!(fs::read(&target).unwrap(), plugin);
        assert!(!install(&plugin).unwrap().changed);

        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let moved = opencode_plugin(Path::new("/new/agentd"));
        assert!(install(&moved).unwrap().changed);
        assert_eq!(fs::read(&target).unwrap(), moved);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            0o640
        );

        let uninstall = || mutate_opencode_plugin(&target, None, |_, _| {});
        let removed = uninstall().unwrap();
        assert!(removed.changed);
        assert!(removed.not_removed.is_empty());
        assert!(!target.exists());
        assert!(!uninstall().unwrap().changed);
        assert_eq!(
            fs::read_dir(&directory.0).unwrap().count(),
            0,
            "no candidate files are left behind"
        );
    }

    #[test]
    fn opencode_plugin_never_replaces_or_removes_an_unowned_file() {
        let directory = TestDir::new("opencode-unowned");
        let target = directory.0.join("agentd.js");
        let foreign = b"export const Mine = async () => ({})\n";
        fs::write(&target, foreign).unwrap();

        let error = mutate_opencode_plugin(
            &target,
            Some(&opencode_plugin(Path::new("/opt/agentd"))),
            |_, _| {},
        )
        .unwrap_err();
        assert!(error.contains("unowned_configuration_target"));
        let kept = mutate_opencode_plugin(&target, None, |_, _| {}).unwrap();
        assert!(!kept.changed);
        assert_eq!(kept.not_removed.len(), 1);
        assert_eq!(fs::read(&target).unwrap(), foreign);
    }

    #[test]
    fn opencode_plugin_refuses_symlinks_and_rechecks_concurrent_changes() {
        let directory = TestDir::new("opencode-race");
        let target = directory.0.join("agentd.js");
        let referent = directory.0.join("elsewhere.js");
        fs::write(&referent, b"keep").unwrap();
        symlink(&referent, &target).unwrap();
        let plugin = opencode_plugin(Path::new("/opt/agentd"));
        let error = mutate_opencode_plugin(&target, Some(&plugin), |_, _| {}).unwrap_err();
        assert!(error.contains("unsupported_configuration_target"));
        assert_eq!(fs::read(&referent).unwrap(), b"keep");
        fs::remove_file(&target).unwrap();

        // A foreign file appearing mid-install is noticed and left alone.
        let error = mutate_opencode_plugin(&target, Some(&plugin), |attempt, path| {
            if attempt == 0 {
                fs::write(path, b"foreign").unwrap();
            }
        })
        .unwrap_err();
        assert!(error.contains("unowned_configuration_target"));
        assert_eq!(fs::read(&target).unwrap(), b"foreign");

        // An owned file rewritten mid-uninstall twice is refused, not removed.
        fs::write(&target, &plugin).unwrap();
        let error = mutate_opencode_plugin(&target, None, |attempt, path| {
            let mut bytes = plugin.clone();
            bytes.extend_from_slice(format!("// {attempt}\n").as_bytes());
            fs::write(path, bytes).unwrap();
        })
        .unwrap_err();
        assert!(error.contains("configuration_changed"));
        assert!(target.exists());
    }

    fn exercise_idempotence(harness: IntegrationHarness, file_name: &str) {
        let directory = TestDir::new(harness.as_str());
        let target = directory.0.join(file_name);
        let trust = directory.0.join("config.toml");
        let trust_bytes = b"[hooks.state]\ntrusted_hash = \"unchanged\"\n";
        fs::write(&trust, trust_bytes).unwrap();
        let fixture = json!({
            "unrelatedRoot": {"keep": true},
            "hooks": {
                "UserPromptSubmit": [
                    {
                        "matcher": "first",
                        "hooks": [
                            {"type": "command", "command": "/bin/first", "timeout": 7},
                            {"type": "command", "command": "echo agentd is unrelated"},
                            {
                                "type": "command",
                                "command": format!(
                                    "/mixed/agentd hook --integration agentd-v1.1 --harness {} --event Stop",
                                    harness.as_str()
                                )
                            },
                            {
                                "type": "command",
                                "command": format!(
                                    "/unowned/agentd hook --integration agentd-v1.1 --harness {} --event UserPromptSubmit --extra",
                                    harness.as_str()
                                )
                            }
                        ]
                    },
                    {
                        "hooks": [{
                            "type": "command",
                            "command": format!(
                                "/old/agentd hook --integration agentd-v1.1 --harness {} --event UserPromptSubmit",
                                harness.as_str()
                            ),
                            "timeout": 1
                        }]
                    }
                ],
                "OtherEvent": [{
                    "hooks": [
                        {"type": "command", "command": "/bin/before"},
                        {"type": "command", "command": "/bin/after"}
                    ]
                }]
            }
        });
        fs::write(&target, serde_json::to_vec_pretty(&fixture).unwrap()).unwrap();
        let original_mode = fs::metadata(&target).unwrap().permissions().mode() & 0o7777;

        let install = || {
            mutate_configuration(&target, |root| {
                merge(root, Action::Install, harness, Path::new("/new/agentd"))
            })
        };
        let first_install = install().unwrap();
        assert!(first_install.changed);
        assert_eq!(first_install.not_removed.len(), 1);
        let installed = fs::read(&target).unwrap();
        assert!(!install().unwrap().changed);
        assert_eq!(fs::read(&target).unwrap(), installed);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            original_mode
        );
        assert_eq!(count_owned(&target, harness), harness.events().len());
        assert_eq!(fs::read(&trust).unwrap(), trust_bytes);

        let uninstall = || {
            mutate_configuration(&target, |root| {
                merge(root, Action::Uninstall, harness, Path::new("/moved/agentd"))
            })
        };
        let first_uninstall = uninstall().unwrap();
        assert!(first_uninstall.changed);
        assert_eq!(first_uninstall.not_removed.len(), 1);
        let uninstalled = fs::read(&target).unwrap();
        assert!(!uninstall().unwrap().changed);
        assert_eq!(fs::read(&target).unwrap(), uninstalled);
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o7777,
            original_mode
        );
        assert_eq!(count_owned(&target, harness), 0);
        assert_eq!(fs::read(&trust).unwrap(), trust_bytes);

        let value: Value = serde_json::from_slice(&uninstalled).unwrap();
        assert_eq!(value["unrelatedRoot"], fixture["unrelatedRoot"]);
        assert!(value["hooks"].is_object());
        assert_eq!(value["hooks"]["UserPromptSubmit"][0]["matcher"], "first");
        assert_eq!(
            value["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "/bin/first"
        );
        assert_eq!(
            value["hooks"]["UserPromptSubmit"][0]["hooks"][1]["command"],
            "echo agentd is unrelated"
        );
        assert!(
            value["hooks"]["UserPromptSubmit"][0]["hooks"][2]["command"]
                .as_str()
                .unwrap()
                .ends_with("--extra")
        );
        assert_eq!(
            value["hooks"]["OtherEvent"][0]["hooks"][0]["command"],
            "/bin/before"
        );
        assert_eq!(
            value["hooks"]["OtherEvent"][0]["hooks"][1]["command"],
            "/bin/after"
        );
    }

    fn count_owned(target: &Path, harness: IntegrationHarness) -> usize {
        let value: Value = serde_json::from_slice(&fs::read(target).unwrap()).unwrap();
        value["hooks"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|groups| groups.as_array().unwrap())
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .filter_map(handler_command)
            .filter_map(owned_declaration)
            .filter(|owned| owned.harness == harness)
            .count()
    }
}
