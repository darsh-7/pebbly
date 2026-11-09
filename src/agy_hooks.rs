//! Antigravity's hooks for Pebbly in `~/.gemini/config/hooks.json` (the IDE, the app and the CLI
//! all read it): a `pebbly` entry next to the others (Orca's...), there while the setting is on.
//! Each event runs a small .cmd in %LOCALAPPDATA%\Pebbly\agy that hands the hook's JSON to
//! `pebbly.exe agy-hook <event>` and prints the answer Antigravity wants (`{}`, or a Stop's
//! decision that lets it stop): the JSON doesn't name its event, and a hook command has no args.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Our entry's name in hooks.json.
pub const NAME: &str = "pebbly";
/// The events Pebbly listens to.
pub const EVENTS: [&str; 3] = ["PreInvocation", "PostToolUse", "Stop"];

/// `~/.gemini/config`: there once Antigravity ran on this PC.
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".gemini").join("config"))
}

fn wrapper_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Pebbly").join("agy"))
}

/// The .cmd for `event`: Pebbly gets the JSON, Antigravity its answer, and the hook never fails.
pub fn wrapper_text(exe: &str, event: &str) -> String {
    let answer = if event == "Stop" { r#"echo {"decision":""}"# } else { "echo {}" };
    format!("@echo off\r\n\"{exe}\" agy-hook {event}\r\n{answer}\r\nexit /b 0\r\n")
}

/// `hooks` with our entry pointing at `cmds` (event → .cmd path), replacing any old one.
pub fn install(mut hooks: Value, cmds: &[(&str, String)]) -> Value {
    if !hooks.is_object() {
        hooks = json!({});
    }
    let mut ours = serde_json::Map::new();
    for (event, cmd) in cmds {
        let handler = json!({"type": "command", "command": cmd, "timeout": 10});
        // Only the tool events take a matcher; the others list their handlers directly.
        let entry = if event.ends_with("ToolUse") { json!([{"matcher": "*", "hooks": [handler]}]) } else { json!([handler]) };
        ours.insert(event.to_string(), entry);
    }
    hooks[NAME] = Value::Object(ours);
    hooks
}

/// `hooks` without our entry.
pub fn remove(mut hooks: Value) -> Value {
    if let Some(map) = hooks.as_object_mut() {
        map.remove(NAME);
    }
    hooks
}

fn write_if_changed(path: &Path, text: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(());
    }
    std::fs::write(path, text)
}

/// Puts our hooks in (on) or takes them out (off); nothing when Antigravity was never used here
/// or the file is already as asked. Whether the file changed.
pub fn follow(on: bool, exe: &str) -> std::io::Result<bool> {
    let Some(dir) = config_dir().filter(|d| d.exists()) else { return Ok(false) };
    let path = dir.join("hooks.json");
    if !on {
        if !path.exists() {
            return Ok(false);
        }
        return crate::claude_hooks::rewrite_at(&path, |v| v.get(NAME).is_some().then(|| remove(v)));
    }
    let wrappers = wrapper_dir().ok_or_else(|| std::io::Error::other("no local app data"))?;
    std::fs::create_dir_all(&wrappers)?;
    let mut cmds = Vec::new();
    for event in EVENTS {
        let cmd = wrappers.join(format!("agy-{}.cmd", event.to_lowercase()));
        write_if_changed(&cmd, &wrapper_text(exe, event))?;
        cmds.push((event, cmd.to_string_lossy().into_owned()));
    }
    crate::claude_hooks::rewrite_at(&path, |v| {
        let next = install(v.clone(), &cmds);
        (next != v).then_some(next)
    })
}

/// `follow` for the running exe and the current setting, logged.
pub fn follow_settings() {
    let s = crate::settings::current();
    let Ok(exe) = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()) else { return };
    match follow(s.enabled && s.live_enabled && s.agy_on, &exe) {
        Ok(true) => crate::platform::log::log(&format!("antigravity hooks: {}", if s.agy_on { "installed" } else { "removed" })),
        Ok(false) => {}
        Err(e) => crate::platform::log::log(&format!("antigravity hooks: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_entry_sits_next_to_others_and_comes_out_alone() {
        let orca = json!({"orca-status": {"Stop": [{"type": "command", "command": "orca.cmd"}]}});
        let cmds = [("PreInvocation", "pre.cmd".to_string()), ("PostToolUse", "post.cmd".to_string()), ("Stop", "stop.cmd".to_string())];
        let with = install(orca.clone(), &cmds);
        assert_eq!(with["orca-status"], orca["orca-status"], "theirs untouched");
        assert_eq!(with[NAME]["PreInvocation"][0]["command"], "pre.cmd");
        assert_eq!(with[NAME]["PostToolUse"][0]["matcher"], "*");
        assert_eq!(with[NAME]["PostToolUse"][0]["hooks"][0]["command"], "post.cmd");
        assert_eq!(install(with.clone(), &cmds), with, "installing twice changes nothing");
        assert_eq!(remove(with), orca);
    }

    #[test]
    fn the_wrapper_answers_stop_with_a_decision() {
        let stop = wrapper_text(r"C:\P\pebbly.exe", "Stop");
        assert!(stop.contains(r#""C:\P\pebbly.exe" agy-hook Stop"#));
        assert!(stop.contains(r#"echo {"decision":""}"#));
        assert!(wrapper_text("p.exe", "PostToolUse").contains("echo {}"));
    }
}
