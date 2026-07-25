use super::*;

use crate::setup::detect::{codex_hook_handler_is_ours, notify_is_ours};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use toml_edit::{Array, DocumentMut};

#[derive(Debug)]
pub enum Outcome {
    Changed(String),
    Unchanged,
    Conflict,
}

fn our_array() -> Array {
    let mut a = Array::new();
    for m in CODEX_NOTIFY_MARKER {
        a.push(m);
    }
    a
}

/// Pure editor. `install=true` adds/keeps our notify; `install=false` uninstalls.
/// Never clobbers a foreign notify unless `force`. Errors on unparseable TOML.
pub fn edit_codex(existing: &str, install: bool, force: bool) -> Result<Outcome, String> {
    let mut doc = existing
        .parse::<DocumentMut>()
        .map_err(|e| format!("config.toml is not valid TOML: {e}"))?;
    let present = doc.get("notify").is_some();
    let ours = notify_is_ours(doc.get("notify"));

    if install {
        if ours {
            return Ok(Outcome::Unchanged);
        }
        if present && !force {
            return Ok(Outcome::Conflict);
        }
        if present {
            // force overwrite of a foreign notify — in place, position preserved.
            doc["notify"] = toml_edit::value(our_array());
            return Ok(Outcome::Changed(doc.to_string()));
        }
        // Absent: prepend at byte 0 so the key stays top-level (a key appended
        // after an existing [table] would bind to that table). Preserves the
        // rest verbatim.
        let line = format!(
            "notify = [\"{}\", \"{}\", \"{}\"]\n",
            CODEX_NOTIFY_MARKER[0], CODEX_NOTIFY_MARKER[1], CODEX_NOTIFY_MARKER[2]
        );
        return Ok(Outcome::Changed(format!("{line}{existing}")));
    }

    // Uninstall: remove only if it's ours; leave a foreign/absent notify alone.
    if ours {
        doc.as_table_mut().remove("notify");
        Ok(Outcome::Changed(doc.to_string()))
    } else {
        Ok(Outcome::Unchanged)
    }
}

/// Pure editor for Codex `hooks.json`. It strips only marker-owned Radar
/// command hooks, then re-adds the current hook set when installing.
pub fn edit_codex_hooks(existing: &str, install: bool) -> Result<Outcome, String> {
    let mut file = parse_hooks_file(existing)?;
    strip_codex_hooks(&mut file);

    if install {
        add_codex_hooks(&mut file);
    }

    let new = json_pretty(&file)?;
    if normalized_hooks_text(existing) == new {
        Ok(Outcome::Unchanged)
    } else {
        Ok(Outcome::Changed(new))
    }
}

/// Typed view of a Codex `hooks.json`. Deserialization *is* the shape check:
/// the `hooks` map, its event arrays, and each group's optional handler array
/// must have these types or `serde_json` rejects the file — so there is no
/// separate hand-written validator. Foreign keys at every level are preserved
/// verbatim through the flattened `rest`/`meta` maps, and `handlers` stay as raw
/// `Value`s so unknown handler fields round-trip untouched.
///
/// `handlers` is `Option` (not a defaulted `Vec`) so an *absent* `hooks` key and
/// an explicit empty `hooks: []` stay distinct across a round-trip — the strip
/// logic and a preexisting-empty-group must tell them apart.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct HooksFile {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) hooks: BTreeMap<String, Vec<HookGroup>>,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct HookGroup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hooks: Option<Vec<Value>>,
    #[serde(flatten)]
    meta: Map<String, Value>,
}

pub(crate) fn parse_hooks_file(existing: &str) -> Result<HooksFile, String> {
    if existing.trim().is_empty() {
        return Ok(HooksFile::default());
    }
    serde_json::from_str(existing).map_err(|e| format!("hooks.json is not valid JSON: {e}"))
}

/// Remove only Radar-owned handlers, then collapse any group/event we emptied.
/// A group is dropped only when *we* emptied it (it held our handlers and now
/// holds none) — a preexisting empty `hooks: []` or a group with no handler
/// array is left untouched.
fn strip_codex_hooks(file: &mut HooksFile) {
    for groups in file.hooks.values_mut() {
        groups.retain_mut(|group| {
            let Some(handlers) = group.hooks.as_mut() else {
                return true; // no handler array — not ours to touch
            };
            let before = handlers.len();
            handlers.retain(|handler| !codex_hook_handler_is_ours(handler));
            // Drop the group only if removing our handlers emptied it.
            !(handlers.len() != before && handlers.is_empty())
        });
    }
    // Drop events whose groups are all gone; an empty `hooks` map serializes away.
    file.hooks.retain(|_, groups| !groups.is_empty());
}

fn add_codex_hooks(file: &mut HooksFile) {
    for event in CODEX_HOOK_EVENTS {
        file.hooks
            .entry(event.to_string())
            .or_default()
            .push(codex_hook_group());
    }
}

fn codex_hook_group() -> HookGroup {
    HookGroup {
        hooks: Some(vec![json!({
            "type": "command",
            "command": CODEX_HOOK_COMMAND,
            "commandWindows": CODEX_HOOK_COMMAND_WINDOWS,
            "timeout": 5
        })]),
        meta: Map::new(),
    }
}

fn normalized_hooks_text(existing: &str) -> String {
    parse_hooks_file(existing)
        .and_then(|f| json_pretty(&f))
        .unwrap_or_else(|_| existing.to_string())
}

fn json_pretty<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value)
        .map(|mut s| {
            s.push('\n');
            s
        })
        .map_err(|e| format!("hooks.json serialization failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_top_level_notify_is_ours(toml: &str) {
        let doc = toml.parse::<toml_edit::DocumentMut>().expect("valid toml");
        assert!(
            notify_is_ours(doc.get("notify")),
            "notify must be top-level and ours:\n{toml}"
        );
    }

    #[test]
    fn fresh_file_installs_our_notify() {
        let out = edit_codex("", true, false).unwrap();
        match out {
            Outcome::Changed(s) => assert_top_level_notify_is_ours(&s),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn installs_above_existing_tables_stays_top_level() {
        let existing = "[marketplaces.x]\nsource = \"local\"\n";
        let out = edit_codex(existing, true, false).unwrap();
        match out {
            Outcome::Changed(s) => {
                assert_top_level_notify_is_ours(&s);
                assert!(
                    s.contains("[marketplaces.x]"),
                    "must preserve the user's table"
                );
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn idempotent_when_already_ours() {
        let existing = "notify = [\"zj-radar\", \"notify\", \"codex\"]\n";
        assert!(matches!(
            edit_codex(existing, true, false).unwrap(),
            Outcome::Unchanged
        ));
    }

    #[test]
    fn foreign_notify_refuses_without_force() {
        let existing = "notify = [\"/some/other/notifier\", \"turn-ended\"]\n";
        assert!(matches!(
            edit_codex(existing, true, false).unwrap(),
            Outcome::Conflict
        ));
    }

    #[test]
    fn foreign_notify_overwritten_with_force_preserves_rest() {
        let existing = "model = \"gpt-5.5\"\nnotify = [\"/other\", \"turn-ended\"]\n";
        match edit_codex(existing, true, true).unwrap() {
            Outcome::Changed(s) => {
                assert_top_level_notify_is_ours(&s);
                assert!(
                    s.contains("model = \"gpt-5.5\""),
                    "must preserve other keys"
                );
                assert!(!s.contains("/other"), "foreign notifier must be gone");
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let ours = "notify = [\"zj-radar\", \"notify\", \"codex\"]\nmodel = \"x\"\n";
        match edit_codex(ours, false, false).unwrap() {
            Outcome::Changed(s) => {
                assert!(!s.contains("notify"));
                assert!(s.contains("model = \"x\""));
            }
            o => panic!("{o:?}"),
        }
        let foreign = "notify = [\"/other\", \"turn-ended\"]\n";
        assert!(matches!(
            edit_codex(foreign, false, false).unwrap(),
            Outcome::Unchanged
        ));
    }

    #[test]
    fn malformed_toml_is_refused() {
        assert!(edit_codex("this = = not toml", true, false).is_err());
    }

    fn hooks_value(json_text: &str) -> serde_json::Value {
        serde_json::from_str(json_text).unwrap()
    }

    fn hook_handler_count(json_text: &str) -> usize {
        let v = hooks_value(json_text);
        v.get("hooks")
            .and_then(Value::as_object)
            .map(|events| {
                events
                    .values()
                    .filter_map(Value::as_array)
                    .flat_map(|groups| groups.iter())
                    .filter_map(|group| group.get("hooks").and_then(Value::as_array))
                    .map(Vec::len)
                    .sum()
            })
            .unwrap_or(0)
    }

    #[test]
    fn codex_hooks_fresh_file_installs_all_events() {
        match edit_codex_hooks("", true).unwrap() {
            Outcome::Changed(s) => {
                let v = hooks_value(&s);
                for event in CODEX_HOOK_EVENTS {
                    assert!(
                        v.pointer(&format!("/hooks/{event}/0/hooks/0/command"))
                            .and_then(Value::as_str)
                            .is_some_and(|command| command.contains(CODEX_HOOK_MARKER)),
                        "missing owned hook for {event}:\n{s}"
                    );
                }
                assert_eq!(hook_handler_count(&s), CODEX_HOOK_EVENTS.len());
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn codex_hooks_are_idempotent_after_pretty_install() {
        let once = match edit_codex_hooks("", true).unwrap() {
            Outcome::Changed(s) => s,
            o => panic!("{o:?}"),
        };
        assert!(matches!(
            edit_codex_hooks(&once, true).unwrap(),
            Outcome::Unchanged
        ));
    }

    #[test]
    fn codex_hooks_preserve_foreign_hooks_and_replaces_ours() {
        let existing = r#"{
          "hooks": {
            "PreToolUse": [
              {
                "matcher": "Bash",
                "hooks": [
                  {
                    "type": "command",
                    "command": "echo foreign"
                  },
                  {
                    "type": "command",
                    "command": "ZJ_RADAR_CODEX_HOOK=v1 old-zj-radar notify codex"
                  }
                ]
              }
            ]
          }
        }"#;
        match edit_codex_hooks(existing, true).unwrap() {
            Outcome::Changed(s) => {
                assert!(s.contains("echo foreign"));
                assert!(!s.contains("old-zj-radar"));
                assert_eq!(hook_handler_count(&s), CODEX_HOOK_EVENTS.len() + 1);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn codex_hooks_uninstall_removes_only_ours() {
        let installed = match edit_codex_hooks(
            r#"{
              "hooks": {
                "Stop": [
                  {
                    "hooks": [
                      {
                        "type": "command",
                        "command": "echo foreign"
                      },
                      {
                        "type": "command",
                        "command": "ZJ_RADAR_CODEX_HOOK=v1 zj-radar notify codex"
                      }
                    ]
                  }
                ]
              }
            }"#,
            false,
        )
        .unwrap()
        {
            Outcome::Changed(s) => s,
            o => panic!("{o:?}"),
        };
        assert!(installed.contains("echo foreign"));
        assert!(!installed.contains(CODEX_HOOK_MARKER));
    }

    #[test]
    fn codex_hooks_uninstall_only_ours_collapses_empty_container() {
        let installed = match edit_codex_hooks("", true).unwrap() {
            Outcome::Changed(s) => s,
            o => panic!("{o:?}"),
        };
        match edit_codex_hooks(&installed, false).unwrap() {
            Outcome::Changed(s) => assert_eq!(hooks_value(&s), json!({})),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn codex_hooks_preserve_preexisting_empty_groups() {
        let existing = r#"{
          "hooks": {
            "Stop": [
              {
                "matcher": "Bash",
                "hooks": []
              }
            ]
          }
        }"#;
        match edit_codex_hooks(existing, false).unwrap() {
            Outcome::Unchanged => {}
            o => panic!("{o:?}"),
        }
        match edit_codex_hooks(existing, true).unwrap() {
            Outcome::Changed(s) => {
                let empty = hooks_value(&s)
                    .pointer("/hooks/Stop/0/hooks")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty);
                assert!(empty, "preexisting empty group should remain:\n{s}");
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn codex_hooks_preserve_foreign_top_level_and_group_keys() {
        // Foreign top-level keys and group-level metadata (e.g. `matcher`) must
        // survive a round-trip — they flow through the flattened `rest`/`meta`.
        let existing = r#"{
          "model": "gpt-5",
          "hooks": {
            "Stop": [
              {
                "matcher": "Bash",
                "hooks": [
                  { "type": "command", "command": "echo foreign" }
                ]
              }
            ]
          }
        }"#;
        let s = match edit_codex_hooks(existing, true).unwrap() {
            Outcome::Changed(s) => s,
            o => panic!("{o:?}"),
        };
        let v = hooks_value(&s);
        assert_eq!(v.pointer("/model").and_then(Value::as_str), Some("gpt-5"));
        assert_eq!(
            v.pointer("/hooks/Stop/0/matcher").and_then(Value::as_str),
            Some("Bash"),
            "foreign group metadata must be preserved:\n{s}"
        );
        assert!(s.contains("echo foreign"), "foreign handler must survive:\n{s}");
        assert!(s.contains(CODEX_HOOK_MARKER), "our hook must be added:\n{s}");
    }

    #[test]
    fn codex_hooks_reject_malformed_json_and_bad_shapes() {
        assert!(edit_codex_hooks("not json", true).is_err());
        assert!(edit_codex_hooks("[]", true).is_err());
        assert!(edit_codex_hooks(r#"{"hooks":[]}"#, true).is_err());
        assert!(edit_codex_hooks(r#"{"hooks":{"Stop":{}}}"#, true).is_err());
        assert!(edit_codex_hooks(r#"{"hooks":{"Stop":[{"hooks":{}}]}}"#, true).is_err());
    }
}
