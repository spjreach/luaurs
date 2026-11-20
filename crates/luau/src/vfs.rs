//! Port of `CLI/src/VfsNavigator.cpp` and the path helpers of
//! `CLI/src/FileUtils.cpp`: navigation over the file system for `require`.

use luau_require::{ConfigStatus, NavigateResult};

const SUFFIXES: [&str; 2] = [".luau", ".lua"];
const INIT_SUFFIXES: [&str; 2] = ["/init.luau", "/init.lua"];

pub fn is_file(path: &str) -> bool {
    std::path::Path::new(path).is_file()
}

pub fn is_directory(path: &str) -> bool {
    std::path::Path::new(path).is_dir()
}

/// `isAbsolutePath`: "X:/", "X:\", "/" or "\" on Windows; "/" elsewhere.
pub fn is_absolute_path(path: &str) -> bool {
    let b = path.as_bytes();
    if cfg!(windows) {
        (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\'))
            || (!b.is_empty() && (b[0] == b'/' || b[0] == b'\\'))
    } else {
        !b.is_empty() && b[0] == b'/'
    }
}

pub fn get_current_working_directory() -> Option<String> {
    std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned())
}

/// `normalizePath`: resolves `.` and `..`, uses `/` separators; relative
/// paths start with `./` or `../`.
pub fn normalize_path(path: &str) -> String {
    let components: Vec<&str> = path.split(['/', '\\']).collect();
    let is_absolute = is_absolute_path(path);
    let mut normalized: Vec<&str> = Vec::new();
    for &component in components.iter().skip(if is_absolute { 1 } else { 0 }) {
        if component == ".." {
            if normalized.is_empty() {
                if !is_absolute {
                    normalized.push("..");
                }
            } else if *normalized.last().unwrap() == ".." {
                normalized.push("..");
            } else {
                normalized.pop();
            }
        } else if !component.is_empty() && component != "." {
            normalized.push(component);
        }
    }
    let mut out = String::new();
    if is_absolute {
        out.push_str(components[0]);
        out.push('/');
    } else if normalized.is_empty() || normalized[0] != ".." {
        out.push_str("./");
    }
    out.push_str(&normalized.join("/"));
    if out.ends_with("..") {
        out.push('/');
    }
    out
}

/// Real path of the module at `module_path` (file with a suffix, or a
/// directory, possibly with an init file).
fn get_real_path(module_path: &str) -> (NavigateResult, String) {
    let mut found = false;
    let mut suffix = "";
    let last_component = module_path.rsplit('/').next().unwrap_or("");
    if last_component != "init" {
        for s in SUFFIXES {
            if is_file(&format!("{}{}", module_path, s)) {
                if found {
                    return (NavigateResult::Ambiguous, String::new());
                }
                suffix = s;
                found = true;
            }
        }
    }
    if is_directory(module_path) {
        if found {
            return (NavigateResult::Ambiguous, String::new());
        }
        for s in INIT_SUFFIXES {
            if is_file(&format!("{}{}", module_path, s)) {
                if found {
                    return (NavigateResult::Ambiguous, String::new());
                }
                suffix = s;
                found = true;
            }
        }
        found = true;
    }
    if !found {
        return (NavigateResult::NotFound, String::new());
    }
    (NavigateResult::Success, format!("{}{}", module_path, suffix))
}

/// Module path of a file: `/` separators, no drive prefix, no suffix.
fn get_module_path(file_path: &str) -> String {
    let file_path = file_path.replace('\\', "/");
    let mut view: &str = &file_path;
    if is_absolute_path(view) {
        let first_slash = view.find('/').unwrap_or(0);
        view = &view[first_slash..];
    }
    for s in INIT_SUFFIXES.iter().chain(SUFFIXES.iter()) {
        if let Some(stripped) = view.strip_suffix(s) {
            return stripped.to_string();
        }
    }
    view.to_string()
}

#[derive(Default)]
pub struct VfsNavigator {
    real_path: String,
    absolute_real_path: String,
    absolute_path_prefix: String,
    module_path: String,
    absolute_module_path: String,
}

impl VfsNavigator {
    fn update_real_paths(&mut self) -> NavigateResult {
        let (status, real) = get_real_path(&self.module_path);
        let (abs_status, abs_real) = get_real_path(&self.absolute_module_path);
        if status != NavigateResult::Success || abs_status != NavigateResult::Success {
            return status;
        }
        self.real_path = if is_absolute_path(&real) { format!("{}{}", self.absolute_path_prefix, real) } else { real };
        self.absolute_real_path = format!("{}{}", self.absolute_path_prefix, abs_real);
        NavigateResult::Success
    }

    pub fn reset_to_stdin(&mut self) -> NavigateResult {
        let Some(cwd) = get_current_working_directory() else { return NavigateResult::NotFound };
        self.real_path = "./stdin".to_string();
        self.absolute_real_path = normalize_path(&format!("{}/stdin", cwd));
        self.module_path = "./stdin".to_string();
        self.absolute_module_path = get_module_path(&self.absolute_real_path);
        let first_slash = self.absolute_real_path.find('/').unwrap_or(0);
        self.absolute_path_prefix = self.absolute_real_path[..first_slash].to_string();
        NavigateResult::Success
    }

    pub fn reset_to_path(&mut self, path: &str) -> NavigateResult {
        let normalized = normalize_path(path);
        if is_absolute_path(&normalized) {
            self.module_path = get_module_path(&normalized);
            self.absolute_module_path = self.module_path.clone();
            let first_slash = normalized.find('/').unwrap_or(0);
            self.absolute_path_prefix = normalized[..first_slash].to_string();
        } else {
            let Some(cwd) = get_current_working_directory() else { return NavigateResult::NotFound };
            self.module_path = get_module_path(&normalized);
            let joined = normalize_path(&format!("{}/{}", cwd, normalized));
            self.absolute_module_path = get_module_path(&joined);
            let first_slash = joined.find('/').unwrap_or(0);
            self.absolute_path_prefix = joined[..first_slash].to_string();
        }
        self.update_real_paths()
    }

    pub fn to_parent(&mut self) -> NavigateResult {
        if self.absolute_module_path == "/" {
            return NavigateResult::NotFound;
        }
        if self.absolute_module_path.bytes().filter(|&c| c == b'/').count() == 1 {
            return NavigateResult::NotFound;
        }
        self.module_path = normalize_path(&format!("{}/..", self.module_path));
        self.absolute_module_path = normalize_path(&format!("{}/..", self.absolute_module_path));
        // there is no ambiguity when navigating up in a tree
        match self.update_real_paths() {
            NavigateResult::Ambiguous => NavigateResult::Success,
            s => s,
        }
    }

    pub fn to_child(&mut self, name: &str) -> NavigateResult {
        if name == ".config" {
            return NavigateResult::NotFound;
        }
        self.module_path = normalize_path(&format!("{}/{}", self.module_path, name));
        self.absolute_module_path = normalize_path(&format!("{}/{}", self.absolute_module_path, name));
        self.update_real_paths()
    }

    pub fn get_file_path(&self) -> &str {
        &self.real_path
    }

    pub fn get_absolute_file_path(&self) -> &str {
        &self.absolute_real_path
    }

    fn get_config_path(&self, filename: &str) -> String {
        let mut directory: &str = &self.real_path;
        for s in INIT_SUFFIXES.iter().chain(SUFFIXES.iter()) {
            if let Some(stripped) = directory.strip_suffix(s) {
                directory = stripped;
                break;
            }
        }
        format!("{}/{}", directory, filename)
    }

    pub fn get_config_status(&self) -> ConfigStatus {
        let luaurc = is_file(&self.get_config_path(luau_config::CONFIG_NAME));
        let luau_config = is_file(&self.get_config_path(luau_config::LUAU_CONFIG_NAME));
        match (luaurc, luau_config) {
            (true, true) => ConfigStatus::Ambiguous,
            (_, true) => ConfigStatus::PresentLuau,
            (true, _) => ConfigStatus::PresentJson,
            _ => ConfigStatus::Absent,
        }
    }

    pub fn get_config(&self) -> Option<String> {
        let name = match self.get_config_status() {
            ConfigStatus::PresentJson => luau_config::CONFIG_NAME,
            ConfigStatus::PresentLuau => luau_config::LUAU_CONFIG_NAME,
            _ => return None,
        };
        std::fs::read(self.get_config_path(name)).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(normalize_path("a/b/../c"), "./a/c");
        assert_eq!(normalize_path("./a/./b"), "./a/b");
        assert_eq!(normalize_path("../a"), "../a");
        assert_eq!(normalize_path("a/../.."), "../");
        assert_eq!(normalize_path("/a/../../b"), "/b");
        assert_eq!(normalize_path(""), "./");
        if cfg!(windows) {
            assert_eq!(normalize_path("C:\\a\\..\\b"), "C:/b");
        }
    }

    #[test]
    fn module_paths() {
        assert_eq!(get_module_path("./a/b.luau"), "./a/b");
        assert_eq!(get_module_path("./a/init.lua"), "./a");
        if cfg!(windows) {
            assert_eq!(get_module_path("C:/x/y.lua"), "/x/y");
        }
    }
}
