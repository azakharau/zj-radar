use super::*;

use serde_json::Value;
use toml_edit::Item;

/// True iff `notify` exists and equals our exact marker array.
pub fn notify_is_ours(item: Option<&Item>) -> bool {
    item.and_then(|i| i.as_array())
        .map(|a| {
            a.len() == CODEX_NOTIFY_MARKER.len()
                && a.iter()
                    .zip(CODEX_NOTIFY_MARKER)
                    .all(|(v, m)| v.as_str() == Some(m))
        })
        .unwrap_or(false)
}

pub(crate) fn codex_hook_handler_is_ours(handler: &Value) -> bool {
    handler
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.contains(CODEX_HOOK_MARKER))
        || handler
            .get("commandWindows")
            .and_then(Value::as_str)
            .is_some_and(|command| command.contains(CODEX_HOOK_MARKER))
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
    fn notify_is_ours_matches_exact_marker_array() {
        let existing = "notify = [\"zj-radar\", \"notify\", \"codex\"]\n";
        assert_top_level_notify_is_ours(existing);
    }

    #[test]
    fn notify_is_ours_rejects_foreign_array() {
        let doc = "notify = [\"/other\", \"turn-ended\"]\n"
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        assert!(!notify_is_ours(doc.get("notify")));
    }

    #[test]
    fn notify_is_ours_rejects_absent() {
        let doc = "model = \"x\"\n".parse::<toml_edit::DocumentMut>().unwrap();
        assert!(!notify_is_ours(doc.get("notify")));
    }

    #[test]
    fn codex_hook_handler_is_ours_matches_command_or_windows_variant() {
        let ours = serde_json::json!({"command": format!("{CODEX_HOOK_MARKER} zj-radar notify codex")});
        assert!(codex_hook_handler_is_ours(&ours));
        let ours_windows = serde_json::json!({"commandWindows": format!("{CODEX_HOOK_MARKER} zj-radar notify codex")});
        assert!(codex_hook_handler_is_ours(&ours_windows));
        let foreign = serde_json::json!({"command": "echo foreign"});
        assert!(!codex_hook_handler_is_ours(&foreign));
    }
}
