//! Maps the items of the Context panel (memory files, skills, custom agents, MCP servers, the
//! transcript) to the local file that defines them, so the panel can open them.
//!
//! Everything here is pure or reads only small local files and fails soft: an item that cannot
//! be matched simply has no file. Resolution is meant to run on a background executor.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::claude_ctx::{AgentItem, CtxView, McpItem, MemoryItem, SkillItem};

/// How deep the plugin directories are searched.
const MAX_DEPTH: usize = 8;
/// `~/.claude.json` can be large (every project's history lives in it).
const MAX_CLAUDE_JSON_BYTES: u64 = 32 * 1024 * 1024;
const MAX_MCP_JSON_BYTES: u64 = 1024 * 1024;
/// A resolution is repeated after this long (files can appear or go away).
pub(crate) const RESOLVE_TTL_MS: u64 = 30_000;
const LIST_SEPARATOR: char = if cfg!(windows) { ';' } else { ':' };

/// Something in the panel that may be backed by a file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Item {
    Memory(PathBuf),
    Transcript(PathBuf),
    Skill { name: String, source: Option<String>, plugin: Option<String>, root: Option<PathBuf> },
    Agent { name: String, source: Option<String>, root: Option<PathBuf> },
    /// An MCP server by the name used in tool names (`plugin_context7_context7`).
    Mcp { server: String, root: Option<PathBuf> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub path: PathBuf,
    /// True for files that must not be edited from Sich (Claude's own live state).
    pub read_only: bool,
}

/// Where Claude keeps its files on this machine.
#[derive(Clone, Debug, Default)]
pub(crate) struct Env {
    /// `CLAUDE_CONFIG_DIR` or `~/.claude`.
    pub config_dir: PathBuf,
    /// Claude's user state file: `<home>/.claude.json`, or inside the config dir when that is overridden.
    pub claude_json: PathBuf,
    /// Extra plugin folders (`env.CLAUDE_CODE_PLUGIN_DIRS` of `settings.json`).
    pub plugin_dirs: Vec<PathBuf>,
}

impl Env {
    pub(crate) fn from_system() -> Option<Env> {
        let config_dir = sich_hooks::claude_install::default_claude_config_dir()?;
        let claude_json = claude_json_path(std::env::var_os("CLAUDE_CONFIG_DIR"), &config_dir, dirs::home_dir());
        let plugin_dirs = std::fs::read_to_string(config_dir.join("settings.json"))
            .map(|text| plugin_dirs_from_settings(&text))
            .unwrap_or_default();
        Some(Env { config_dir, claude_json, plugin_dirs })
    }
}

/// `<config dir>/.claude.json` when `CLAUDE_CONFIG_DIR` is set, else `<home>/.claude.json`.
pub(crate) fn claude_json_path(env_dir: Option<std::ffi::OsString>, config_dir: &Path, home: Option<PathBuf>) -> PathBuf {
    match (env_dir.filter(|v| !v.is_empty()), home) {
        (None, Some(home)) => home.join(".claude.json"),
        _ => config_dir.join(".claude.json"),
    }
}

/// The folders of `env.CLAUDE_CODE_PLUGIN_DIRS` in a `settings.json` text.
pub(crate) fn plugin_dirs_from_settings(text: &str) -> Vec<PathBuf> {
    let Ok(value) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    value["env"]["CLAUDE_CODE_PLUGIN_DIRS"]
        .as_str()
        .map(|list| list.split(LIST_SEPARATOR).map(str::trim).filter(|p| !p.is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Names and sources
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    User,
    Project,
    Plugin,
    Other,
}

/// The engine's source word (`userSettings`, `projectSettings`, `plugin`, ...) as a scope.
fn scope(source: Option<&str>) -> Scope {
    match source.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("usersettings" | "user") => Scope::User,
        Some("projectsettings" | "project" | "localsettings" | "local") => Scope::Project,
        Some("plugin") => Scope::Plugin,
        _ => Scope::Other,
    }
}

/// `plugin:skill` -> `skill` (the folder name).
pub(crate) fn strip_namespace(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// How Claude names servers in tool names: anything outside `[A-Za-z0-9_-]` becomes `_`.
pub(crate) fn normalize_server(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn same_dir(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_ascii_lowercase();
    norm(a) == norm(b)
}

// ---------------------------------------------------------------------------------------------
// Plugin index
// ---------------------------------------------------------------------------------------------

/// What one walk over the plugin folders found.
#[derive(Debug, Default)]
pub(crate) struct PluginIndex {
    /// (skill folder name, its SKILL.md)
    skills: Vec<(String, PathBuf)>,
    /// (agent name, its .md)
    agents: Vec<(String, PathBuf)>,
    /// `.mcp.json` and `.claude-plugin/plugin.json` files.
    mcp_files: Vec<PathBuf>,
}

impl PluginIndex {
    /// Walks `<config>/plugins` and every extra plugin folder.
    pub(crate) fn scan(env: &Env) -> PluginIndex {
        let mut index = PluginIndex::default();
        let roots = std::iter::once(env.config_dir.join("plugins")).chain(env.plugin_dirs.iter().cloned());
        for root in roots {
            index.walk(&root, 0);
        }
        index
    }

    fn walk(&mut self, dir: &Path, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if !is_dir {
                if name == ".mcp.json" || (name == "plugin.json" && dir.file_name().is_some_and(|n| n == ".claude-plugin")) {
                    self.mcp_files.push(path);
                }
                continue;
            }
            match name.as_str() {
                "node_modules" | ".git" => {}
                "skills" => self.collect_skills(&path),
                "agents" => self.collect_agents(&path),
                _ if depth < MAX_DEPTH => self.walk(&path, depth + 1),
                _ => {}
            }
        }
    }

    fn collect_skills(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let skill = entry.path().join("SKILL.md");
            if skill.is_file() {
                self.skills.push((entry.file_name().to_string_lossy().into_owned(), skill));
            }
        }
    }

    fn collect_agents(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")) && path.is_file() {
                if let Some(stem) = path.file_stem() {
                    self.agents.push((stem.to_string_lossy().into_owned(), path));
                }
            }
        }
    }
}

/// The best of several candidates: one whose path contains the plugin name, else the greatest
/// path (so the newest version folder wins).
fn pick(mut candidates: Vec<PathBuf>, plugin: Option<&str>) -> Option<PathBuf> {
    candidates.sort();
    let want = plugin.map(str::trim).filter(|p| !p.is_empty()).map(str::to_ascii_lowercase);
    if let Some(want) = want {
        let hit = candidates.iter().rev().find(|p| p.to_string_lossy().replace('\\', "/").to_ascii_lowercase().contains(&want));
        if let Some(hit) = hit {
            return Some(hit.clone());
        }
    }
    candidates.pop()
}

// ---------------------------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------------------------

fn found(path: PathBuf, exists: &dyn Fn(&Path) -> bool) -> Option<Resolved> {
    exists(&path).then_some(Resolved { path, read_only: false })
}

fn resolve_skill(
    name: &str,
    source: Option<&str>,
    plugin: Option<&str>,
    root: Option<&Path>,
    env: &Env,
    index: &PluginIndex,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<Resolved> {
    let folder = strip_namespace(name);
    if folder.is_empty() || folder.contains(['/', '\\']) || folder == ".." {
        return None;
    }
    match scope(source) {
        Scope::User => found(env.config_dir.join("skills").join(folder).join("SKILL.md"), exists),
        Scope::Project => found(root?.join(".claude").join("skills").join(folder).join("SKILL.md"), exists),
        Scope::Plugin => {
            let all = index.skills.iter().filter(|(n, p)| n == folder && exists(p)).map(|(_, p)| p.clone()).collect();
            pick(all, plugin).map(|path| Resolved { path, read_only: false })
        }
        Scope::Other => None,
    }
}

fn resolve_agent(
    name: &str,
    source: Option<&str>,
    root: Option<&Path>,
    env: &Env,
    index: &PluginIndex,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<Resolved> {
    let agent = strip_namespace(name);
    if agent.is_empty() || agent.contains(['/', '\\']) || agent == ".." {
        return None;
    }
    let file = format!("{agent}.md");
    match scope(source) {
        Scope::User => found(env.config_dir.join("agents").join(file), exists),
        Scope::Project => found(root?.join(".claude").join("agents").join(file), exists),
        Scope::Plugin => {
            // The namespace names the plugin: `plugin:agent`.
            let plugin = name.strip_suffix(agent).map(|p| p.trim_end_matches(':')).filter(|p| !p.is_empty());
            let all = index.agents.iter().filter(|(n, p)| n == agent && exists(p)).map(|(_, p)| p.clone()).collect();
            pick(all, plugin).map(|path| Resolved { path, read_only: false })
        }
        Scope::Other => None,
    }
}

fn read_json(path: &Path, max: u64) -> Option<Value> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > max {
        return None;
    }
    let mut text = String::new();
    file.take(max).read_to_string(&mut text).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

/// Server keys of a `.mcp.json` (`{"mcpServers": {..}}` or the bare `{name: {..}}` form) or of the
/// inline `mcpServers` object of a `plugin.json` (`bare_ok = false`).
fn mcp_keys(value: &Value, bare_ok: bool) -> Vec<String> {
    if let Some(servers) = value.get("mcpServers").and_then(Value::as_object) {
        return servers.keys().cloned().collect();
    }
    match value.as_object() {
        Some(map) if bare_ok => map.iter().filter(|(_, v)| v.is_object()).map(|(k, _)| k.clone()).collect(),
        _ => Vec::new(),
    }
}

/// Names a plugin goes by: its folder names and the `name` of its `plugin.json`.
fn plugin_names(file: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        file.components().filter_map(|c| c.as_os_str().to_str()).map(normalize_server).collect();
    let manifest = if file.file_name().is_some_and(|n| n == "plugin.json") {
        file.to_path_buf()
    } else {
        file.parent().map(|p| p.join(".claude-plugin").join("plugin.json")).unwrap_or_default()
    };
    if let Some(name) = read_json(&manifest, MAX_MCP_JSON_BYTES).and_then(|v| v["name"].as_str().map(normalize_server)) {
        names.push(name);
    }
    names
}

fn resolve_mcp(server: &str, root: Option<&Path>, env: &Env, index: &PluginIndex, exists: &dyn Fn(&Path) -> bool) -> Option<Resolved> {
    // Connectors of the Claude account have no local file.
    if server.is_empty() || server.starts_with("claude_ai_") {
        return None;
    }
    let matches_key = |keys: &[String]| keys.iter().any(|k| normalize_server(k) == server);

    if let Some(path) = root.map(|r| r.join(".mcp.json")).filter(|p| exists(p)) {
        if read_json(&path, MAX_MCP_JSON_BYTES).is_some_and(|v| matches_key(&mcp_keys(&v, true))) {
            return Some(Resolved { path, read_only: false });
        }
    }

    if let Some(rest) = server.strip_prefix("plugin_") {
        let mut hits = Vec::new();
        for file in index.mcp_files.iter().filter(|f| exists(f)) {
            let is_manifest = file.file_name().is_some_and(|n| n == "plugin.json");
            let Some(value) = read_json(file, MAX_MCP_JSON_BYTES) else { continue };
            let keys = mcp_keys(&value, !is_manifest);
            let names = plugin_names(file);
            if keys.iter().any(|k| {
                let key = normalize_server(k);
                names.iter().any(|n| rest == format!("{n}_{key}"))
            }) {
                hits.push(file.clone());
            }
        }
        // A `.mcp.json` is preferred over the manifest that may only mention it.
        let (mcp, manifests): (Vec<_>, Vec<_>) = hits.into_iter().partition(|f| f.file_name().is_some_and(|n| n == ".mcp.json"));
        if let Some(path) = pick(mcp, None).or_else(|| pick(manifests, None)) {
            return Some(Resolved { path, read_only: false });
        }
        return None;
    }

    if exists(&env.claude_json) {
        if let Some(value) = read_json(&env.claude_json, MAX_CLAUDE_JSON_BYTES) {
            let mut keys = mcp_keys(&serde_json::json!({ "mcpServers": value["mcpServers"] }), false);
            if let (Some(root), Some(projects)) = (root, value["projects"].as_object()) {
                let root = root.to_string_lossy();
                for (dir, project) in projects {
                    if same_dir(dir, &root) {
                        keys.extend(mcp_keys(project, false));
                    }
                }
            }
            if matches_key(&keys) {
                // Claude's own live state: never editable from here.
                return Some(Resolved { path: env.claude_json.clone(), read_only: true });
            }
        }
    }
    None
}

/// The file of one item, if there is one. `index` is only read for plugin items.
pub(crate) fn resolve(item: &Item, env: &Env, index: &PluginIndex, exists: &dyn Fn(&Path) -> bool) -> Option<Resolved> {
    match item {
        Item::Memory(path) => found(path.clone(), exists).filter(|_| path.is_absolute()),
        Item::Transcript(path) => exists(path).then(|| Resolved { path: path.clone(), read_only: true }),
        Item::Skill { name, source, plugin, root } => {
            resolve_skill(name, source.as_deref(), plugin.as_deref(), root.as_deref(), env, index, exists)
        }
        Item::Agent { name, source, root } => resolve_agent(name, source.as_deref(), root.as_deref(), env, index, exists),
        Item::Mcp { server, root } => resolve_mcp(server, root.as_deref(), env, index, exists),
    }
}

fn needs_index(item: &Item) -> bool {
    match item {
        Item::Skill { source, .. } | Item::Agent { source, .. } => scope(source.as_deref()) == Scope::Plugin,
        Item::Mcp { server, .. } => server.starts_with("plugin_"),
        _ => false,
    }
}

/// Resolves a batch, walking the plugin folders at most once.
pub(crate) fn resolve_all(env: &Env, items: Vec<Item>, exists: &dyn Fn(&Path) -> bool) -> Vec<(Item, Option<Resolved>)> {
    let index = if items.iter().any(needs_index) { PluginIndex::scan(env) } else { PluginIndex::default() };
    items.into_iter().map(|item| {
        let resolved = resolve(&item, env, &index, exists);
        (item, resolved)
    }).collect()
}

pub(crate) fn memory_item(item: &MemoryItem) -> Option<Item> {
    item.detail.as_deref().map(str::trim).filter(|p| !p.is_empty()).map(|p| Item::Memory(PathBuf::from(p)))
}

pub(crate) fn skill_item(item: &SkillItem, root: Option<&Path>) -> Item {
    Item::Skill { name: item.name.clone(), source: item.source.clone(), plugin: item.detail.clone(), root: root.map(Path::to_path_buf) }
}

pub(crate) fn agent_item(item: &AgentItem, root: Option<&Path>) -> Item {
    Item::Agent { name: item.name.clone(), source: item.source.clone(), root: root.map(Path::to_path_buf) }
}

pub(crate) fn mcp_item(item: &McpItem, root: Option<&Path>) -> Option<Item> {
    item.detail.as_deref().filter(|s| !s.is_empty()).map(|server| Item::Mcp { server: server.to_string(), root: root.map(Path::to_path_buf) })
}

/// Every item of a session view that may open a file.
pub(crate) fn items_of(view: &CtxView) -> Vec<Item> {
    let root = view.root.as_deref();
    let mut all: Vec<Item> = Vec::new();
    all.extend(view.memory.iter().filter_map(memory_item));
    all.extend(view.skills.iter().flat_map(|s| s.items.iter()).map(|s| skill_item(s, root)));
    all.extend(view.agents.iter().map(|a| agent_item(a, root)));
    all.extend(view.mcp.iter().filter_map(|m| mcp_item(m, root)));
    all.extend(view.transcript_path.iter().map(|p| Item::Transcript(p.clone())));
    let mut seen = HashSet::new();
    all.retain(|item| seen.insert(item.clone()));
    all
}

// ---------------------------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------------------------

/// Resolutions with the time they were made; entries are redone after [`RESOLVE_TTL_MS`].
#[derive(Default)]
pub(crate) struct ResolveCache {
    entries: HashMap<Item, (Option<Resolved>, u64)>,
}

impl ResolveCache {
    /// Items that were never resolved or whose resolution is stale.
    pub(crate) fn due(&self, items: &[Item], now_ms: u64) -> Vec<Item> {
        let mut seen = HashSet::new();
        items
            .iter()
            .filter(|item| self.entries.get(*item).is_none_or(|(_, at)| now_ms.saturating_sub(*at) >= RESOLVE_TTL_MS))
            .filter(|item| seen.insert((*item).clone()))
            .cloned()
            .collect()
    }

    pub(crate) fn store(&mut self, results: Vec<(Item, Option<Resolved>)>, now_ms: u64) {
        for (item, resolved) in results {
            self.entries.insert(item, (resolved, now_ms));
        }
    }

    #[cfg(test)]
    pub(crate) fn get(&self, item: &Item) -> Option<&Resolved> {
        self.entries.get(item).and_then(|(r, _)| r.as_ref())
    }

    /// The resolved files only, for the view layer.
    pub(crate) fn snapshot(&self) -> HashMap<Item, Resolved> {
        self.entries.iter().filter_map(|(item, (r, _))| r.clone().map(|r| (item.clone(), r))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn real(path: &Path) -> bool {
        path.is_file()
    }

    struct Fx {
        _dir: tempfile::TempDir,
        base: PathBuf,
        env: Env,
        root: PathBuf,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let config = base.join("cfg");
        let env = Env { config_dir: config.clone(), claude_json: base.join("home").join(".claude.json"), plugin_dirs: Vec::new() };
        let root = base.join("proj");
        fs::create_dir_all(&root).unwrap();
        Fx { _dir: dir, base, env, root }
    }

    fn skill(name: &str, source: &str, plugin: Option<&str>, root: &Path) -> Item {
        Item::Skill { name: name.into(), source: Some(source.into()), plugin: plugin.map(Into::into), root: Some(root.into()) }
    }

    fn run(fx: &Fx, item: &Item) -> Option<Resolved> {
        resolve(item, &fx.env, &PluginIndex::scan(&fx.env), &real)
    }

    #[test]
    fn scopes_and_names() {
        assert_eq!(scope(Some("userSettings")), Scope::User);
        assert_eq!(scope(Some("user")), Scope::User);
        assert_eq!(scope(Some("projectSettings")), Scope::Project);
        assert_eq!(scope(Some("local")), Scope::Project);
        assert_eq!(scope(Some("plugin")), Scope::Plugin);
        for other in [Some("built-in"), Some("mcp"), Some("syncedSkills"), Some("policySettings"), Some("weird"), None] {
            assert_eq!(scope(other), Scope::Other, "{other:?}");
        }
        assert_eq!(strip_namespace("plug:skill"), "skill");
        assert_eq!(strip_namespace("a:b:c"), "c");
        assert_eq!(strip_namespace("plain"), "plain");
        assert_eq!(normalize_server("plugin:context7:context7"), "plugin_context7_context7");
        assert_eq!(normalize_server("my server.x"), "my_server_x");
        assert_eq!(normalize_server("ok-name_1"), "ok-name_1");
        assert!(same_dir("C:\\Dev\\App\\", "c:/dev/app"));
        assert!(!same_dir("C:/dev/app", "C:/dev/app2"));
    }

    #[test]
    fn user_and_project_skills_and_agents() {
        let fx = fx();
        write(&fx.env.config_dir.join("skills/mine/SKILL.md"), "x");
        write(&fx.root.join(".claude/skills/local/SKILL.md"), "x");
        write(&fx.env.config_dir.join("agents/scout.md"), "x");
        write(&fx.root.join(".claude/agents/proj-agent.md"), "x");
        let user = run(&fx, &skill("mine", "userSettings", None, &fx.root)).unwrap();
        assert_eq!(user.path, fx.env.config_dir.join("skills/mine/SKILL.md"));
        assert!(!user.read_only);
        assert!(run(&fx, &skill("local", "projectSettings", None, &fx.root)).is_some());
        // Wrong scope for the folder it lives in.
        assert!(run(&fx, &skill("local", "userSettings", None, &fx.root)).is_none());
        assert!(run(&fx, &skill("mine", "projectSettings", None, &fx.root)).is_none());
        // Missing file.
        assert!(run(&fx, &skill("ghost", "userSettings", None, &fx.root)).is_none());
        // Project scope without a session root cannot resolve.
        let no_root = Item::Skill { name: "local".into(), source: Some("projectSettings".into()), plugin: None, root: None };
        assert!(run(&fx, &no_root).is_none());
        let agent = |name: &str, source: &str| Item::Agent { name: name.into(), source: Some(source.into()), root: Some(fx.root.clone()) };
        assert_eq!(run(&fx, &agent("scout", "userSettings")).unwrap().path, fx.env.config_dir.join("agents/scout.md"));
        assert!(run(&fx, &agent("proj-agent", "projectSettings")).is_some());
        assert!(run(&fx, &agent("scout", "projectSettings")).is_none());
        assert!(run(&fx, &agent("scout", "built-in")).is_none());
    }

    #[test]
    fn namespaced_names_use_the_last_segment() {
        let fx = fx();
        write(&fx.env.config_dir.join("skills/deploy/SKILL.md"), "x");
        assert!(run(&fx, &skill("team:deploy", "userSettings", None, &fx.root)).is_some());
        // Path tricks never escape the skills folder.
        write(&fx.base.join("secret/SKILL.md"), "x");
        assert!(run(&fx, &skill("../secret", "userSettings", None, &fx.root)).is_none());
        assert!(run(&fx, &skill("", "userSettings", None, &fx.root)).is_none());
    }

    #[test]
    fn builtin_and_unknown_sources_have_no_file() {
        let fx = fx();
        write(&fx.env.config_dir.join("skills/x/SKILL.md"), "x");
        for source in ["built-in", "mcp", "syncedSkills", "policySettings", "future"] {
            assert!(run(&fx, &skill("x", source, None, &fx.root)).is_none(), "{source}");
        }
        let no_source = Item::Skill { name: "x".into(), source: None, plugin: None, root: None };
        assert!(run(&fx, &no_source).is_none());
    }

    #[test]
    fn plugin_skills_prefer_the_plugin_and_the_newest_version() {
        let fx = fx();
        let cache = fx.env.config_dir.join("plugins/cache/market");
        write(&cache.join("alpha/1.0.0/skills/review/SKILL.md"), "a");
        write(&cache.join("beta/1.0.0/skills/review/SKILL.md"), "b");
        write(&cache.join("beta/1.2.0/skills/review/SKILL.md"), "b2");
        write(&cache.join("beta/1.2.0/agents/helper.md"), "h");
        let got = |plugin: Option<&str>| run(&fx, &skill("beta:review", "plugin", plugin, &fx.root)).unwrap().path;
        assert_eq!(got(Some("alpha")), cache.join("alpha/1.0.0/skills/review/SKILL.md"));
        assert_eq!(got(Some("beta")), cache.join("beta/1.2.0/skills/review/SKILL.md"));
        // Without a plugin name the greatest path wins.
        assert_eq!(got(None), cache.join("beta/1.2.0/skills/review/SKILL.md"));
        // An unknown plugin name still finds a match by skill name.
        assert!(got(Some("zzz")).ends_with("skills/review/SKILL.md"));
        assert!(run(&fx, &skill("nothing", "plugin", Some("beta"), &fx.root)).is_none());
        let agent = Item::Agent { name: "beta:helper".into(), source: Some("plugin".into()), root: None };
        assert_eq!(run(&fx, &agent).unwrap().path, cache.join("beta/1.2.0/agents/helper.md"));
    }

    #[test]
    fn plugin_dirs_from_settings_are_searched_and_bounded() {
        let mut fx = fx();
        let extra = fx.base.join("extra-plugin");
        write(&extra.join("skills/zip/SKILL.md"), "x");
        fx.env.plugin_dirs = vec![extra.clone(), fx.base.join("missing")];
        assert_eq!(run(&fx, &skill("zip", "plugin", None, &fx.root)).unwrap().path, extra.join("skills/zip/SKILL.md"));
        // Too deep to be found.
        let deep = fx.env.config_dir.join("plugins/a/b/c/d/e/f/g/h/i/j/skills/deep/SKILL.md");
        write(&deep, "x");
        assert!(run(&fx, &skill("deep", "plugin", None, &fx.root)).is_none());
        // node_modules is skipped.
        write(&fx.env.config_dir.join("plugins/p/node_modules/q/skills/nm/SKILL.md"), "x");
        assert!(run(&fx, &skill("nm", "plugin", None, &fx.root)).is_none());
    }

    #[test]
    fn plugin_dirs_setting_parsing() {
        let sep = LIST_SEPARATOR;
        let text = format!(r#"{{"env":{{"CLAUDE_CODE_PLUGIN_DIRS":" /a/b {sep}{sep}/c "}}}}"#);
        assert_eq!(plugin_dirs_from_settings(&text), [PathBuf::from("/a/b"), PathBuf::from("/c")]);
        assert!(plugin_dirs_from_settings("{}").is_empty());
        assert!(plugin_dirs_from_settings("garbage").is_empty());
        assert!(plugin_dirs_from_settings(r#"{"env":{"CLAUDE_CODE_PLUGIN_DIRS":5}}"#).is_empty());
    }

    #[test]
    fn claude_json_follows_the_config_dir_override() {
        let cfg = PathBuf::from("/cfg");
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(claude_json_path(None, &cfg, home.clone()), PathBuf::from("/home/u/.claude.json"));
        assert_eq!(claude_json_path(Some("".into()), &cfg, home.clone()), PathBuf::from("/home/u/.claude.json"));
        assert_eq!(claude_json_path(Some("/cfg".into()), &cfg, home), PathBuf::from("/cfg/.claude.json"));
        assert_eq!(claude_json_path(None, &cfg, None), PathBuf::from("/cfg/.claude.json"));
    }

    #[test]
    fn memory_and_transcript_need_an_existing_file() {
        let fx = fx();
        let file = fx.base.join("CLAUDE.md");
        write(&file, "x");
        assert_eq!(run(&fx, &Item::Memory(file.clone())).unwrap(), Resolved { path: file.clone(), read_only: false });
        assert!(run(&fx, &Item::Memory(fx.base.join("none.md"))).is_none());
        assert!(run(&fx, &Item::Memory(PathBuf::from("relative/CLAUDE.md"))).is_none());
        let log = fx.base.join("s.jsonl");
        write(&log, "{}");
        assert!(run(&fx, &Item::Transcript(log)).unwrap().read_only);
        assert!(run(&fx, &Item::Transcript(fx.base.join("no.jsonl"))).is_none());
    }

    #[test]
    fn injected_exists_check_is_used() {
        let fx = fx();
        let item = skill("mine", "userSettings", None, &fx.root);
        let index = PluginIndex::default();
        assert!(resolve(&item, &fx.env, &index, &|_| true).is_some());
        assert!(resolve(&item, &fx.env, &index, &|_| false).is_none());
    }

    fn mcp(server: &str, fx: &Fx) -> Option<Resolved> {
        run(fx, &Item::Mcp { server: server.into(), root: Some(fx.root.clone()) })
    }

    #[test]
    fn mcp_project_file_must_contain_the_server() {
        let fx = fx();
        write(&fx.root.join(".mcp.json"), r#"{"mcpServers":{"my server":{"command":"x"},"db":{}}}"#);
        let hit = mcp("my_server", &fx).unwrap();
        assert_eq!(hit, Resolved { path: fx.root.join(".mcp.json"), read_only: false });
        assert!(mcp("db", &fx).is_some());
        assert!(mcp("other", &fx).is_none());
        // Bare format and a broken file.
        write(&fx.root.join(".mcp.json"), r#"{"bare":{"command":"x"}}"#);
        assert!(mcp("bare", &fx).is_some());
        write(&fx.root.join(".mcp.json"), "{ nope");
        assert!(mcp("bare", &fx).is_none());
    }

    #[test]
    fn mcp_plugin_servers_match_plugin_and_key() {
        let fx = fx();
        let plug = fx.env.config_dir.join("plugins/cache/mk/context7/1.0.0");
        write(&plug.join(".mcp.json"), r#"{"mcpServers":{"context7":{"command":"npx"}}}"#);
        write(&plug.join(".claude-plugin/plugin.json"), r#"{"name":"context7"}"#);
        let other = fx.env.config_dir.join("plugins/cache/mk/tools/2.0.0");
        write(&other.join(".claude-plugin/plugin.json"), r#"{"name":"tools","mcpServers":{"fetch":{"command":"x"}}}"#);
        assert_eq!(mcp("plugin_context7_context7", &fx).unwrap().path, plug.join(".mcp.json"));
        assert_eq!(mcp("plugin_tools_fetch", &fx).unwrap().path, other.join(".claude-plugin/plugin.json"));
        assert!(mcp("plugin_tools_context7", &fx).is_none());
        assert!(mcp("plugin_context7_other", &fx).is_none());
        assert!(!mcp("plugin_tools_fetch", &fx).unwrap().read_only);
        // A plugin server never falls through to ~/.claude.json.
        write(&fx.env.claude_json, r#"{"mcpServers":{"plugin_x_y":{}}}"#);
        assert!(mcp("plugin_x_y", &fx).is_none());
    }

    #[test]
    fn mcp_user_state_is_read_only_and_checks_the_project() {
        let fx = fx();
        let root_text = fx.root.to_string_lossy().replace('\\', "/");
        let json = serde_json::json!({
            "mcpServers": {"global one": {}},
            "projects": {
                root_text.clone(): {"mcpServers": {"local": {}}},
                "/elsewhere": {"mcpServers": {"foreign": {}}}
            }
        });
        write(&fx.env.claude_json, &json.to_string());
        let global = mcp("global_one", &fx).unwrap();
        assert_eq!(global, Resolved { path: fx.env.claude_json.clone(), read_only: true });
        assert!(mcp("local", &fx).unwrap().read_only);
        assert!(mcp("foreign", &fx).is_none());
        assert!(mcp("nope", &fx).is_none());
        // No session root: only the top-level servers count.
        let no_root = Item::Mcp { server: "local".into(), root: None };
        assert!(run(&fx, &no_root).is_none());
        // Missing and broken state files.
        fs::remove_file(&fx.env.claude_json).unwrap();
        assert!(mcp("global_one", &fx).is_none());
        write(&fx.env.claude_json, "garbage");
        assert!(mcp("global_one", &fx).is_none());
    }

    #[test]
    fn mcp_project_file_wins_and_claude_ai_connectors_have_no_file() {
        let fx = fx();
        write(&fx.root.join(".mcp.json"), r#"{"mcpServers":{"shared":{}}}"#);
        write(&fx.env.claude_json, r#"{"mcpServers":{"shared":{},"claude_ai_Claude_Docs":{}}}"#);
        assert_eq!(mcp("shared", &fx).unwrap().path, fx.root.join(".mcp.json"));
        assert!(mcp("claude_ai_Claude_Docs", &fx).is_none());
        assert!(mcp("", &fx).is_none());
    }

    #[test]
    fn items_of_collects_everything_once() {
        use crate::claude_ctx::SkillsInfo;
        let view = CtxView {
            root: Some("/w".into()),
            memory: vec![
                MemoryItem { name: "a".into(), detail: Some("/w/CLAUDE.md".into()), ..Default::default() },
                MemoryItem { name: "no path".into(), detail: None, ..Default::default() },
                MemoryItem { name: "dup".into(), detail: Some("/w/CLAUDE.md".into()), ..Default::default() },
            ],
            skills: Some(SkillsInfo {
                items: vec![SkillItem { name: "s".into(), source: Some("plugin".into()), detail: Some("p".into()), tokens: 1 }],
                ..Default::default()
            }),
            agents: vec![AgentItem { name: "ag".into(), source: Some("userSettings".into()), tokens: 1 }],
            mcp: vec![
                McpItem { name: "t1".into(), detail: Some("srv".into()), ..Default::default() },
                McpItem { name: "t2".into(), detail: Some("srv".into()), ..Default::default() },
                McpItem { name: "t3".into(), detail: None, ..Default::default() },
            ],
            transcript_path: Some("/t/a.jsonl".into()),
            ..Default::default()
        };
        let items = items_of(&view);
        assert_eq!(items.len(), 5);
        assert!(items.contains(&Item::Memory("/w/CLAUDE.md".into())));
        assert!(items.contains(&Item::Mcp { server: "srv".into(), root: Some("/w".into()) }));
        assert!(items.contains(&Item::Transcript("/t/a.jsonl".into())));
        assert!(items_of(&CtxView::default()).is_empty());
    }

    #[test]
    fn cache_redoes_missing_and_stale_entries_only() {
        let a = Item::Memory("/a".into());
        let b = Item::Memory("/b".into());
        let mut cache = ResolveCache::default();
        assert_eq!(cache.due(&[a.clone(), a.clone(), b.clone()], 0), [a.clone(), b.clone()]);
        cache.store(vec![(a.clone(), Some(Resolved { path: "/a".into(), read_only: false })), (b.clone(), None)], 1_000);
        assert!(cache.due(&[a.clone(), b.clone()], 1_000 + RESOLVE_TTL_MS - 1).is_empty());
        assert_eq!(cache.due(&[a.clone()], 1_000 + RESOLVE_TTL_MS).len(), 1);
        // A clock that went backwards does not re-resolve.
        assert!(cache.due(&[a.clone()], 5).is_empty());
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&b).is_none());
        let snap = cache.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(snap.contains_key(&a));
    }

    #[test]
    fn resolve_all_scans_once_and_keeps_order() {
        let fx = fx();
        write(&fx.env.config_dir.join("plugins/x/skills/s/SKILL.md"), "x");
        write(&fx.env.config_dir.join("skills/u/SKILL.md"), "x");
        let items = vec![skill("u", "userSettings", None, &fx.root), skill("s", "plugin", None, &fx.root), skill("zz", "plugin", None, &fx.root)];
        let out = resolve_all(&fx.env, items.clone(), &real);
        assert_eq!(out.iter().map(|(i, _)| i.clone()).collect::<Vec<_>>(), items);
        assert_eq!(out.iter().map(|(_, r)| r.is_some()).collect::<Vec<_>>(), [true, true, false]);
    }
}
