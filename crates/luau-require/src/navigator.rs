//! Port of `RequireNavigator.cpp`, `PathUtilities.cpp` and
//! `AliasCycleTracker.cpp`: the require-by-string path resolution algorithm,
//! expressed against a [`NavigationContext`] supplied by the embedder.

use std::time::Duration;

use luau_config::luau_config::{extract_luau_config, InterruptCallbacks};
use luau_config::{parse_config, AliasOptions, Config, ConfigOptions};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NavigateResult {
    Success,
    Ambiguous,
    NotFound,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigBehavior {
    GetAlias,
    GetConfig,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfigStatus {
    Absent,
    Ambiguous,
    PresentJson,
    PresentLuau,
}

/// Keeps track of where navigation currently is and answers questions about
/// that location.
pub trait NavigationContext {
    fn reset_to_requirer(&mut self) -> NavigateResult;
    fn jump_to_alias(&mut self, path: &str) -> NavigateResult;
    fn to_alias_override(&mut self, _alias_unprefixed: &str) -> NavigateResult {
        NavigateResult::NotFound
    }
    fn to_alias_fallback(&mut self, _alias_unprefixed: &str) -> NavigateResult {
        NavigateResult::NotFound
    }
    fn to_parent(&mut self) -> NavigateResult;
    fn to_child(&mut self, component: &str) -> NavigateResult;
    fn get_config_status(&self) -> ConfigStatus {
        ConfigStatus::Absent
    }
    /// Whether `get_alias` or `get_config` is used when a configuration is
    /// present.
    fn get_config_behavior(&self) -> ConfigBehavior {
        ConfigBehavior::GetAlias
    }
    fn get_alias(&mut self, _alias: &str) -> Option<String> {
        None
    }
    fn get_config(&self) -> Option<String> {
        None
    }
    /// Time limit for `.config.luau` scripts (None: unlimited).
    fn luau_config_timeout(&self) -> Option<Duration> {
        Some(Duration::from_millis(2000))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathType {
    RelativeToCurrent,
    RelativeToParent,
    Aliased,
    Unsupported,
}

pub fn get_path_type(path: &str) -> PathType {
    if path.starts_with("./") {
        PathType::RelativeToCurrent
    } else if path.starts_with("../") {
        PathType::RelativeToParent
    } else if path.starts_with('@') {
        PathType::Aliased
    } else {
        PathType::Unsupported
    }
}

/// Splits at the first '/': (component, rest).
pub fn split_path(path: &str) -> (&str, &str) {
    match path.find('/') {
        Some(pos) => (&path[..pos], &path[pos + 1..]),
        None => (path, ""),
    }
}

#[derive(Clone, Default)]
struct AliasCycleTracker {
    ordered: Vec<String>,
}

impl AliasCycleTracker {
    fn add(&mut self, alias: &str) -> Option<String> {
        if self.ordered.iter().any(|a| a == alias) {
            return Some(format!("detected alias cycle ({})", self.stringified_cycle(alias)));
        }
        self.ordered.push(alias.to_string());
        None
    }

    fn stringified_cycle(&self, repeated: &str) -> String {
        let mut result = String::new();
        let mut in_cycle = false;
        for item in &self.ordered {
            if in_cycle {
                result += " -> ";
                result += &format!("@{}", item);
            }
            if item == repeated {
                in_cycle = true;
                result += &format!("@{}", item);
            }
        }
        result += &format!(" -> @{}", repeated);
        result
    }
}

fn extract_alias(path: &str) -> String {
    // without the '@' prefix, up to the first separator
    let rest = &path[1..];
    match path.find('/') {
        Some(pos) => path[1..pos].to_string(),
        None => rest.to_string(),
    }
}

type Error = Option<String>;

/// Traverses a require path in a [`NavigationContext`].
pub struct Navigator<'a> {
    ctx: &'a mut dyn NavigationContext,
}

impl<'a> Navigator<'a> {
    pub fn new(ctx: &'a mut dyn NavigationContext) -> Self {
        Navigator { ctx }
    }

    /// Navigates to `path`; the error message on failure.
    pub fn navigate(&mut self, path: &str) -> Result<(), String> {
        let path = path.replace('\\', "/");
        match self.navigate_impl(&path) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn navigate_impl(&mut self, path: &str) -> Error {
        let path_type = get_path_type(path);
        if path_type == PathType::Unsupported {
            return Some("require path must start with a valid prefix: ./, ../, or @".to_string());
        }

        if path_type == PathType::Aliased {
            let alias = extract_alias(path).to_ascii_lowercase();
            if let Some(e) = self.reset_to_requirer() {
                return Some(e);
            }
            if alias == "self" {
                // "@self" navigates directly from the requirer's context;
                // no alias overrides are considered
                return self.navigate_through_path(path);
            }
            match self.to_alias_override(&alias) {
                (Some(e), _) => return Some(e),
                (None, true) => return self.navigate_through_path(path),
                (None, false) => {}
            }
            let mut config = Config::new();
            if let Some(e) = self.navigate_to_and_populate_config(&alias, &mut config) {
                return Some(e);
            }
            if config.aliases.contains_key(&alias) {
                if let Some(e) = self.navigate_to_alias(&alias, &config, AliasCycleTracker::default()) {
                    return Some(e);
                }
            } else if let Some(e) = self.to_alias_fallback(&alias) {
                return Some(e);
            }
            return self.navigate_through_path(path);
        }

        if let Some(e) = self.reset_to_requirer() {
            return Some(e);
        }
        if let Some(e) = self.navigate_to_parent(None) {
            return Some(e);
        }
        self.navigate_through_path(path)
    }

    fn navigate_through_path(&mut self, path: &str) -> Error {
        let mut components = split_path(path);
        if path.starts_with('@') {
            // navigation to the alias itself is handled by the caller
            components = split_path(components.1);
        }
        let mut previous: Option<String> = None;
        while !(components.0.is_empty() && components.1.is_empty()) {
            let first = components.0;
            if first == "." || first.is_empty() {
                components = split_path(components.1);
                continue;
            } else if first == ".." {
                if let Some(e) = self.navigate_to_parent(previous.clone()) {
                    return Some(e);
                }
            } else if let Some(e) = self.navigate_to_child(first) {
                return Some(e);
            }
            previous = Some(first.to_string());
            components = split_path(components.1);
        }
        None
    }

    fn navigate_to_alias(&mut self, alias: &str, config: &Config, mut cycle_tracker: AliasCycleTracker) -> Error {
        let value = config.aliases[alias].value.clone();
        match get_path_type(&value) {
            PathType::RelativeToCurrent | PathType::RelativeToParent => self.navigate_through_path(&value),
            PathType::Aliased => {
                if let Some(e) = cycle_tracker.add(alias) {
                    return Some(e);
                }
                let next_alias = extract_alias(&value);
                match self.to_alias_override(&next_alias) {
                    (Some(e), _) => return Some(e),
                    (None, true) => return self.navigate_through_path(&value),
                    (None, false) => {}
                }
                if config.aliases.contains_key(&next_alias) {
                    if let Some(e) = self.navigate_to_alias(&next_alias, config, cycle_tracker) {
                        return Some(e);
                    }
                } else {
                    let mut parent_config = Config::new();
                    if let Some(e) = self.navigate_to_and_populate_config(&next_alias, &mut parent_config) {
                        return Some(e);
                    }
                    if parent_config.aliases.contains_key(&next_alias) {
                        if let Some(e) = self.navigate_to_alias(&next_alias, &parent_config, AliasCycleTracker::default()) {
                            return Some(e);
                        }
                    } else if let Some(e) = self.to_alias_fallback(&next_alias) {
                        return Some(e);
                    }
                }
                self.navigate_through_path(&value)
            }
            PathType::Unsupported => self.jump_to_alias(&value),
        }
    }

    fn navigate_to_and_populate_config(&mut self, desired_alias: &str, config: &mut Config) -> Error {
        while !config.aliases.contains_key(desired_alias) {
            *config = Config::new();

            match self.ctx.to_parent() {
                NavigateResult::Ambiguous => {
                    return Some(format!("could not navigate up the ancestry chain during search for alias \"{}\" (ambiguous)", desired_alias))
                }
                NavigateResult::NotFound => break, // reached the root
                NavigateResult::Success => {}
            }

            let status = self.ctx.get_config_status();
            match status {
                ConfigStatus::Absent => continue,
                ConfigStatus::Ambiguous => return Some(format!("could not resolve alias \"{}\" (ambiguous configuration file)", desired_alias)),
                _ => {}
            }
            if self.ctx.get_config_behavior() == ConfigBehavior::GetAlias {
                match self.ctx.get_alias(desired_alias) {
                    Some(alias_path) => config.set_alias(desired_alias, &alias_path),
                    None => return Some(format!("could not resolve alias \"{}\"", desired_alias)),
                }
                break;
            }
            let Some(contents) = self.ctx.get_config() else {
                return Some(format!("could not get configuration file contents to resolve alias \"{}\"", desired_alias));
            };
            let alias_options = AliasOptions { config_location: None, overwrite_aliases: false };
            if status == ConfigStatus::PresentJson {
                let opts = ConfigOptions { compat: false, alias_options: Some(alias_options) };
                if let Some(e) = parse_config(&contents, config, &opts) {
                    return Some(e);
                }
            } else {
                let callbacks = InterruptCallbacks { timeout: self.ctx.luau_config_timeout() };
                if let Some(e) = extract_luau_config(&contents, config, Some(alias_options), callbacks) {
                    return Some(e);
                }
            }
        }
        None
    }

    fn reset_to_requirer(&mut self) -> Error {
        match self.ctx.reset_to_requirer() {
            NavigateResult::Success => None,
            NavigateResult::Ambiguous => Some("could not reset to requiring context (ambiguous)".to_string()),
            NavigateResult::NotFound => Some("could not reset to requiring context".to_string()),
        }
    }

    fn jump_to_alias(&mut self, alias_path: &str) -> Error {
        match self.ctx.jump_to_alias(alias_path) {
            NavigateResult::Success => None,
            NavigateResult::Ambiguous => Some(format!("could not jump to alias \"{}\" (ambiguous)", alias_path)),
            NavigateResult::NotFound => Some(format!("could not jump to alias \"{}\"", alias_path)),
        }
    }

    fn navigate_to_parent(&mut self, previous_component: Option<String>) -> Error {
        let result = self.ctx.to_parent();
        if result == NavigateResult::Success {
            return None;
        }
        let mut msg = match previous_component {
            Some(p) => format!("could not get parent of component \"{}\"", p),
            None => "could not get parent of requiring context".to_string(),
        };
        if result == NavigateResult::Ambiguous {
            msg += " (ambiguous)";
        }
        Some(msg)
    }

    fn navigate_to_child(&mut self, component: &str) -> Error {
        match self.ctx.to_child(component) {
            NavigateResult::Success => None,
            NavigateResult::Ambiguous => Some(format!("could not resolve child component \"{}\" (ambiguous)", component)),
            NavigateResult::NotFound => Some(format!("could not resolve child component \"{}\"", component)),
        }
    }

    fn to_alias_override(&mut self, alias_unprefixed: &str) -> (Error, bool) {
        match self.ctx.to_alias_override(alias_unprefixed) {
            NavigateResult::Success => (None, true),
            NavigateResult::NotFound => (None, false),
            NavigateResult::Ambiguous => (Some(format!("@{} is not a valid alias (ambiguous)", alias_unprefixed)), false),
        }
    }

    fn to_alias_fallback(&mut self, alias_unprefixed: &str) -> Error {
        match self.ctx.to_alias_fallback(alias_unprefixed) {
            NavigateResult::Success => None,
            NavigateResult::Ambiguous => Some(format!("@{} is not a valid alias (ambiguous)", alias_unprefixed)),
            NavigateResult::NotFound => Some(format!("@{} is not a valid alias", alias_unprefixed)),
        }
    }
}
