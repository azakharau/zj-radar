use super::*;

use crate::setup::detect::{codex_hook_handler_is_ours, notify_is_ours};

use toml_edit::{DocumentMut, Item};

/// Derived state of Codex's `[features].hooks` switch.
pub(crate) enum CodexHooksFeature {
    EnabledOrUnset,
    Disabled,
    ConfigError(String),
}

/// Derived state of the legacy `notify` slot in Codex `config.toml`.
pub(crate) enum CodexNotifyState {
    ConfigAbsent,
    NotInstalled,
    Ours,
    Foreign,
    ConfigError(String),
}

/// Raw, already-read environment for Codex setup. The only IO layer.
pub(crate) struct CodexEnv {
    pub codex_on_path:    bool,
    pub zj_radar_on_path: bool,
    pub config_text:      Option<String>,
    pub hooks_text:       Option<String>,
}

/// Every derived fact about Codex setup state. The legacy-vs-hooks choice is a
/// flag the consumer projects on — NOT a fact — so both surfaces are observed.
pub(crate) struct CodexFacts {
    pub codex_on_path:     bool,
    pub zj_radar_on_path:  bool,
    pub hooks_feature:     CodexHooksFeature,
    pub notify:            CodexNotifyState,
    /// `None` = hooks.json absent; `Some(Ok(n))` = n marker-owned events; `Some(Err)` = parse error.
    pub owned_hook_events: Option<Result<usize, String>>,
}

/// Pure: derive every Codex setup fact from already-read inputs. No I/O.
pub(crate) fn analyze_codex(env: &CodexEnv) -> CodexFacts {
    let hooks_feature = match env.config_text.as_deref().map(codex_hooks_disabled_in_config) {
        Some(Ok(true)) => CodexHooksFeature::Disabled,
        Some(Ok(false)) | None => CodexHooksFeature::EnabledOrUnset,
        Some(Err(e)) => CodexHooksFeature::ConfigError(e),
    };
    let notify = match env.config_text.as_deref() {
        None => CodexNotifyState::ConfigAbsent,
        Some(text) => match text.parse::<DocumentMut>() {
            Ok(doc) if notify_is_ours(doc.get("notify")) => CodexNotifyState::Ours,
            Ok(doc) if doc.get("notify").is_some() => CodexNotifyState::Foreign,
            Ok(_) => CodexNotifyState::NotInstalled,
            Err(e) => CodexNotifyState::ConfigError(e.to_string()),
        },
    };
    let owned_hook_events = env.hooks_text.as_deref().map(codex_owned_hook_event_count);
    CodexFacts {
        codex_on_path: env.codex_on_path,
        zj_radar_on_path: env.zj_radar_on_path,
        hooks_feature,
        notify,
        owned_hook_events,
    }
}

fn codex_owned_hook_event_count(existing: &str) -> Result<usize, String> {
    let file = parse_hooks_file(existing)?;
    Ok(CODEX_HOOK_EVENTS
        .iter()
        .filter(|event| {
            file.hooks.get(**event).is_some_and(|groups| {
                groups
                    .iter()
                    .filter_map(|group| group.hooks.as_ref())
                    .flatten()
                    .any(codex_hook_handler_is_ours)
            })
        })
        .count())
}

fn codex_hooks_disabled_in_config(existing: &str) -> Result<bool, String> {
    let doc = existing
        .parse::<DocumentMut>()
        .map_err(|e| format!("config.toml is not valid TOML: {e}"))?;
    Ok(doc
        .get("features")
        .and_then(Item::as_table_like)
        .and_then(|features| {
            features
                .get("hooks")
                .or_else(|| features.get("codex_hooks"))
                .and_then(Item::as_bool)
        })
        == Some(false))
}

#[cfg(test)]
mod tests {
    use super::*;







    #[test]
    fn analyze_codex_classifies_notify_states() {
        let ours = "notify = [\"zj-radar\", \"notify\", \"codex\"]\n";
        let foreign = "notify = [\"other\"]\n";
        let mk = |cfg: Option<&str>| analyze_codex(&CodexEnv {
            codex_on_path: true,
            zj_radar_on_path: true,
            config_text: cfg.map(str::to_string),
            hooks_text: None,
        });
        assert!(matches!(mk(Some(ours)).notify, CodexNotifyState::Ours));
        assert!(matches!(mk(Some(foreign)).notify, CodexNotifyState::Foreign));
        assert!(matches!(mk(Some("a = 1\n")).notify, CodexNotifyState::NotInstalled));
        assert!(matches!(mk(None).notify, CodexNotifyState::ConfigAbsent));
    }

    #[test]
    fn analyze_codex_hooks_feature_and_event_count() {
        let cfg_disabled = "[features]\nhooks = false\n";
        let f = analyze_codex(&CodexEnv {
            codex_on_path: true,
            zj_radar_on_path: true,
            config_text: Some(cfg_disabled.to_string()),
            hooks_text: None,
        });
        assert!(matches!(f.hooks_feature, CodexHooksFeature::Disabled));
        assert!(f.owned_hook_events.is_none(), "no hooks.json -> None");
    }
}
