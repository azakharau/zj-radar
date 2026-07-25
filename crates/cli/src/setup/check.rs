use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CheckLevel {
    Ok,
    Warn,
    Missing,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CheckItem {
    level: CheckLevel,
    name: &'static str,
    detail: String,
}

impl CheckItem {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level: CheckLevel::Ok,
            name,
            detail: detail.into(),
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level: CheckLevel::Warn,
            name,
            detail: detail.into(),
        }
    }

    fn missing(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level: CheckLevel::Missing,
            name,
            detail: detail.into(),
        }
    }
}

/// Returns true when any item is `Missing` — the doctor's contribution to the
/// process exit code, so `zj-radar setup --check && zj-radar run` can gate.
/// Warns don't fail: they're advice, not a broken install.
pub(crate) fn check_codex(legacy_notify: bool) -> bool {
    let env = CodexEnv {
        codex_on_path:    which("codex"),
        zj_radar_on_path: which("zj-radar"),
        config_text:      codex_config_path().and_then(|p| std::fs::read_to_string(p).ok()),
        hooks_text:       codex_hooks_path().and_then(|p| std::fs::read_to_string(p).ok()),
    };
    let items = codex_check_items(&analyze_codex(&env), legacy_notify);
    println!("codex:");
    print_check_items(&items)
}



/// Returns true when any item is `Missing` — see [`check_codex`]. The claude
/// producer is wired through Claude Code's plugin marketplace, so the doctor
/// has exactly two facts to verify: the binary and the installed plugin.
pub(crate) fn check_claude() -> bool {
    let wired = claude_producer_wired(claude_installed_plugins_text().as_deref());
    let items = claude_check_items(which("claude"), wired);
    println!("claude:");
    print_check_items(&items)
}

pub(crate) fn claude_check_items(on_path: bool, wired: bool) -> Vec<CheckItem> {
    vec![
        if on_path {
            CheckItem::ok("claude binary", "found on PATH")
        } else {
            CheckItem::missing("claude binary", "not found on PATH")
        },
        if wired {
            CheckItem::ok("plugin", "zj-radar-claude plugin installed")
        } else {
            CheckItem::missing("plugin", "zj-radar-claude not installed — run `zj-radar setup claude`")
        },
    ]
}


/// Print the items; report whether any is `Missing`.
fn print_check_items(items: &[CheckItem]) -> bool {
    for item in items {
        let status = match item.level {
            CheckLevel::Ok => "ok",
            CheckLevel::Warn => "warn",
            CheckLevel::Missing => "missing",
        };
        println!("  {status} {}: {}", item.name, item.detail);
    }
    items.iter().any(|i| i.level == CheckLevel::Missing)
}

pub(crate) fn codex_check_items(f: &CodexFacts, legacy_notify: bool) -> Vec<CheckItem> {
    let mut items = Vec::new();
    items.push(if f.codex_on_path {
        CheckItem::ok("codex binary", "found on PATH")
    } else {
        CheckItem::missing("codex binary", "not found on PATH")
    });
    items.push(if f.zj_radar_on_path {
        CheckItem::ok("zj-radar binary", "found on PATH")
    } else {
        CheckItem::missing("zj-radar binary", "not found on PATH")
    });

    items.push(match &f.hooks_feature {
        CodexHooksFeature::Disabled => {
            CheckItem::warn("hooks feature", "`[features].hooks = false` disables Codex hooks")
        }
        CodexHooksFeature::EnabledOrUnset => {
            CheckItem::ok("hooks feature", "enabled or unset in config.toml")
        }
        CodexHooksFeature::ConfigError(e) => CheckItem::warn("config.toml", e.clone()),
    });

    if legacy_notify {
        items.push(match &f.notify {
            CodexNotifyState::ConfigAbsent => {
                CheckItem::missing("legacy notify", "config.toml not found")
            }
            CodexNotifyState::Ours => CheckItem::ok("legacy notify", "zj-radar owns Codex notify"),
            CodexNotifyState::Foreign => {
                CheckItem::warn("legacy notify", "another command owns Codex notify")
            }
            CodexNotifyState::NotInstalled => {
                CheckItem::missing("legacy notify", "Codex notify is not installed")
            }
            CodexNotifyState::ConfigError(e) => CheckItem::warn(
                "config.toml",
                format!("config.toml is not valid TOML: {e}"),
            ),
        });
    } else {
        items.push(match &f.owned_hook_events {
            None => CheckItem::missing("hooks.json", "zj-radar Codex hooks are not installed"),
            Some(Ok(count)) if *count == CODEX_HOOK_EVENTS.len() => {
                CheckItem::ok("hooks.json", "all zj-radar Codex hooks installed")
            }
            Some(Ok(count)) if *count > 0 => CheckItem::warn(
                "hooks.json",
                format!("partial zj-radar hook install ({count}/{})", CODEX_HOOK_EVENTS.len()),
            ),
            Some(Ok(_)) => {
                CheckItem::missing("hooks.json", "zj-radar Codex hooks are not installed")
            }
            Some(Err(e)) => CheckItem::warn("hooks.json", e.clone()),
        });
        if matches!(f.notify, CodexNotifyState::Foreign) {
            items.push(CheckItem::ok(
                "legacy notify",
                "foreign notify is preserved; hooks do not use the notify slot",
            ));
        }
    }

    if !legacy_notify {
        items.push(CheckItem::warn(
            "hook trust",
            "run `/hooks` in Codex after install or hook changes",
        ));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_check_reports_hook_setup_ready_with_trust_reminder() {
        let hooks = match edit_codex_hooks("", true).unwrap() {
            Outcome::Changed(s) => s,
            o => panic!("{o:?}"),
        };
        let facts = analyze_codex(&CodexEnv {
            codex_on_path:    true,
            zj_radar_on_path: true,
            config_text:      Some("model = \"x\"\n".to_string()),
            hooks_text:       Some(hooks.to_string()),
        });
        let items = codex_check_items(&facts, false);
        assert!(items.contains(&CheckItem::ok("codex binary", "found on PATH")));
        assert!(items.contains(&CheckItem::ok("zj-radar binary", "found on PATH")));
        assert!(items.contains(&CheckItem::ok(
            "hooks feature",
            "enabled or unset in config.toml"
        )));
        assert!(items.contains(&CheckItem::ok(
            "hooks.json",
            "all zj-radar Codex hooks installed"
        )));
        assert!(items.iter().any(|item| item.name == "hook trust"
            && item.level == CheckLevel::Warn
            && item.detail.contains("/hooks")));
    }

    #[test]
    fn codex_check_warns_when_hooks_feature_is_disabled() {
        let hooks = match edit_codex_hooks("", true).unwrap() {
            Outcome::Changed(s) => s,
            o => panic!("{o:?}"),
        };
        let facts = analyze_codex(&CodexEnv {
            codex_on_path:    true,
            zj_radar_on_path: true,
            config_text:      Some("[features]\nhooks = false\n".to_string()),
            hooks_text:       Some(hooks.to_string()),
        });
        let items = codex_check_items(&facts, false);
        assert!(items.iter().any(|item| item.name == "hooks feature"
            && item.level == CheckLevel::Warn
            && item.detail.contains("hooks = false")));
    }

    #[test]
    fn codex_check_reports_partial_or_malformed_hooks() {
        let partial = r#"{
          "hooks": {
            "Stop": [
              {
                "hooks": [
                  {
                    "type": "command",
                    "command": "ZJ_RADAR_CODEX_HOOK=v1 zj-radar notify codex"
                  }
                ]
              }
            ]
          }
        }"#;
        let facts = analyze_codex(&CodexEnv {
            codex_on_path:    true,
            zj_radar_on_path: true,
            config_text:      None,
            hooks_text:       Some(partial.to_string()),
        });
        let items = codex_check_items(&facts, false);
        assert!(items.iter().any(|item| item.name == "hooks.json"
            && item.level == CheckLevel::Warn
            && item.detail.contains("partial")));

        let facts = analyze_codex(&CodexEnv {
            codex_on_path:    true,
            zj_radar_on_path: true,
            config_text:      None,
            hooks_text:       Some("not json".to_string()),
        });
        let items = codex_check_items(&facts, false);
        assert!(items.iter().any(|item| item.name == "hooks.json"
            && item.level == CheckLevel::Warn
            && item.detail.contains("not valid JSON")));
    }

    #[test]
    fn codex_check_notes_foreign_notify_is_preserved_for_hooks() {
        let hooks = match edit_codex_hooks("", true).unwrap() {
            Outcome::Changed(s) => s,
            o => panic!("{o:?}"),
        };
        let config = "notify = [\"/other\", \"turn-ended\"]\n";
        let facts = analyze_codex(&CodexEnv {
            codex_on_path:    true,
            zj_radar_on_path: true,
            config_text:      Some(config.to_string()),
            hooks_text:       Some(hooks.to_string()),
        });
        let items = codex_check_items(&facts, false);
        assert!(items.iter().any(|item| item.name == "legacy notify"
            && item.level == CheckLevel::Ok
            && item.detail.contains("preserved")));
    }

    #[test]
    fn codex_check_legacy_notify_mode_reports_notify_slot() {
        let facts = analyze_codex(&CodexEnv {
            codex_on_path:    true,
            zj_radar_on_path: true,
            config_text:      Some("notify = [\"zj-radar\", \"notify\", \"codex\"]\n".to_string()),
            hooks_text:       None,
        });
        let items = codex_check_items(&facts, true);
        assert!(items.contains(&CheckItem::ok(
            "legacy notify",
            "zj-radar owns Codex notify"
        )));

        let facts = analyze_codex(&CodexEnv {
            codex_on_path:    true,
            zj_radar_on_path: true,
            config_text:      Some("notify = [\"/other\"]\n".to_string()),
            hooks_text:       None,
        });
        let items = codex_check_items(&facts, true);
        assert!(items.iter().any(|item| item.name == "legacy notify"
            && item.level == CheckLevel::Warn
            && item.detail.contains("another command")));
        assert!(
            !items.iter().any(|item| item.name == "hook trust"),
            "legacy notify mode should not ask users to trust hooks"
        );
    }
}
