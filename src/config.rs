//! Shared configuration types and parsing for .meta files.
//!
//! This module provides the core types and functions for finding and parsing
//! .meta configuration files (JSON and YAML formats).

use crate::display::{escape, escape_path, is_unsafe_char};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Represents a project entry in the .meta config.
/// Can be either a simple git URL string or an extended object with additional fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ProjectEntry {
    /// Simple format: just a git URL string
    Simple(String),
    /// Extended format: object with repo, path, tags, and dependency info
    Extended {
        /// Git remote URL (optional for local-only projects).
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        provides: Vec<String>,
        #[serde(default)]
        depends_on: Vec<String>,
        /// If true, this directory contains a nested .meta config
        #[serde(default)]
        meta: bool,
    },
}

/// Parsed project info after normalization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub name: String,
    pub path: String,
    /// Git remote URL (optional for local-only projects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub tags: Vec<String>,
    /// What this project provides (e.g., APIs, libraries)
    #[serde(default)]
    pub provides: Vec<String>,
    /// What this project depends on (other project names or provided items)
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// If true, this directory contains a nested .meta config
    #[serde(default)]
    pub meta: bool,
}

impl ProjectInfo {
    /// Returns true if this project has no git repo URL (cannot be cloned)
    pub fn has_no_repo(&self) -> bool {
        self.repo.is_none()
    }
}

/// Default settings that can be configured in .meta
#[derive(Debug, Deserialize, Clone)]
pub struct MetaDefaults {
    /// Run commands in parallel by default (defaults to true)
    #[serde(default = "default_true")]
    pub parallel: bool,
}

fn default_true() -> bool {
    true
}

impl Default for MetaDefaults {
    fn default() -> Self {
        Self { parallel: true }
    }
}

/// The meta configuration file structure
#[derive(Debug, Deserialize, Default)]
pub struct MetaConfig {
    /// Projects in this meta repo. Values can be:
    /// - A git URL string: `"project": "git@github.com:org/repo.git"`
    /// - An extended object: `"project": { "repo": "...", "path": "...", "meta": true }`
    #[serde(default)]
    pub projects: HashMap<String, ProjectEntry>,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub defaults: MetaDefaults,
    /// Custom directory for worktrees (overrides default .worktrees/)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktrees_dir: Option<String>,
}

/// Determines the format of a config file based on extension
#[derive(Debug, Clone)]
pub enum ConfigFormat {
    Json,
    Yaml,
}

/// Check if a directory has a meta config file (.meta, .meta.yaml, or .meta.yml).
///
/// Unlike `find_meta_config`, this does NOT walk up the directory tree.
/// Returns the path and format if found.
pub fn find_meta_config_in(dir: &Path) -> Option<(PathBuf, ConfigFormat)> {
    for (name, format) in &[
        (".meta", ConfigFormat::Json),
        (".meta.json", ConfigFormat::Json),
        (".meta.yaml", ConfigFormat::Yaml),
        (".meta.yml", ConfigFormat::Yaml),
    ] {
        let candidate = dir.join(name);
        if candidate.exists() && candidate.is_file() {
            return Some((candidate, format.clone()));
        }
    }
    None
}

/// Find the meta config file, checking for .meta, .meta.yaml, and .meta.yml
///
/// Walks up from `start_dir` to the filesystem root, looking for config files.
/// If `config_name` is provided, only looks for that specific filename.
pub fn find_meta_config(
    start_dir: &Path,
    config_name: Option<&PathBuf>,
) -> Option<(PathBuf, ConfigFormat)> {
    let candidates: Vec<(String, ConfigFormat)> = if let Some(name) = config_name {
        // User specified a config file name
        let name_str = name.to_string_lossy().to_string();
        if name_str.ends_with(".yaml") || name_str.ends_with(".yml") {
            vec![(name_str, ConfigFormat::Yaml)]
        } else {
            vec![(name_str, ConfigFormat::Json)]
        }
    } else {
        // Default: check all supported names
        vec![
            (".meta".to_string(), ConfigFormat::Json),
            (".meta.json".to_string(), ConfigFormat::Json),
            (".meta.yaml".to_string(), ConfigFormat::Yaml),
            (".meta.yml".to_string(), ConfigFormat::Yaml),
        ]
    };

    let mut current_dir = start_dir.to_path_buf();
    loop {
        for (name, format) in &candidates {
            let candidate = current_dir.join(name);
            if candidate.exists() && candidate.is_file() {
                return Some((candidate, format.clone()));
            }
        }
        current_dir = current_dir.parent()?.to_path_buf();
    }
}

/// Parse a meta config file (JSON or YAML) and return normalized project info and ignore list.
///
/// Every project is checked by [`validate_project`] and no two projects may
/// share or nest paths ([`check_no_overlap`]); a bad entry fails the parse
/// with the file and project named.
pub fn parse_meta_config(meta_path: &Path) -> anyhow::Result<(Vec<ProjectInfo>, Vec<String>)> {
    let config_str = std::fs::read_to_string(meta_path).with_context(|| {
        format!(
            "Failed to read meta config file: '{}'",
            escape_path(meta_path)
        )
    })?;

    // Determine format from file extension
    let path_str = meta_path.to_string_lossy();
    let config: MetaConfig = if path_str.ends_with(".yaml") || path_str.ends_with(".yml") {
        serde_yaml_ng::from_str(&config_str).with_context(|| {
            format!(
                "Failed to parse YAML config file: {}",
                escape_path(meta_path)
            )
        })?
    } else {
        serde_json::from_str(&config_str).with_context(|| {
            format!(
                "Failed to parse JSON config file: {}",
                escape_path(meta_path)
            )
        })?
    };

    // Convert project entries to normalized, validated ProjectInfo
    // in name order, so the first bad entry reported is always the same one
    let base_dir = meta_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // Where worktrees live; no project may go there
    let worktrees = config
        .worktrees_dir
        .as_deref()
        .and_then(|dir| relative_dir(base_dir, dir).ok())
        .unwrap_or_else(|| ".worktrees".to_string());
    let mut entries: Vec<_> = config.projects.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let checked: Vec<(ProjectInfo, PathBuf)> = entries
        .into_iter()
        .map(|(name, entry)| {
            let (repo, path, tags, provides, depends_on, meta) = match entry {
                // Simple string -> git URL
                ProjectEntry::Simple(url) => {
                    (Some(url), name.clone(), vec![], vec![], vec![], false)
                }
                // Extended object -> repo with additional fields
                // meta: true indicates this project is also a meta-repo (has its own .meta)
                ProjectEntry::Extended {
                    repo,
                    path,
                    tags,
                    provides,
                    depends_on,
                    meta,
                } => {
                    let resolved_path = path.unwrap_or_else(|| name.clone());
                    (repo, resolved_path, tags, provides, depends_on, meta)
                }
            };
            let mut info = ProjectInfo {
                name,
                path,
                repo,
                tags,
                provides,
                depends_on,
                meta,
            };
            let location = validate_project(base_dir, &worktrees, &mut info).map_err(|reason| {
                anyhow::anyhow!(
                    "Invalid project '{}' in {}: {reason}",
                    escape(&info.name),
                    escape_path(meta_path)
                )
            })?;
            Ok((info, location))
        })
        .collect::<anyhow::Result<_>>()?;
    check_no_overlap(&checked)
        .map_err(|e| anyhow::anyhow!("Invalid projects in {}: {e}", escape_path(meta_path)))?;
    let projects = checked.into_iter().map(|(info, _)| info).collect();

    Ok((projects, config.ignore))
}

/// Check one project entry and normalize its path, or say what is wrong.
///
/// These rules are checked once, when a config is parsed, so every caller can
/// join `path` onto the directory of the `.meta` file without checking again:
/// - No value (name, path, repo, tags, provides, depends_on) is empty or holds
///   a control or bidirectional-control character ([`is_unsafe_char`]).
/// - `name` is a relative path in normal form (usually one component; a
///   nested alias like `vendor/lib` is a worktree destination too), held to
///   the same rules as `path`.
/// - The name and every path component pass [`component_problem`]: ASCII
///   letters, digits, space and `._-+@` only, no trailing dot or space, no
///   Windows device name, not `.git`. The first component is not `.worktrees`.
/// - `path` (the name when omitted) is relative and stays inside the config's
///   directory: `\` reads as `/`, empty and `.` components are dropped, `..`
///   is rejected, something must remain, and no symlink along it may lead
///   outside the directory.
/// - `repo` does not start with `-`, so git cannot read it as an option.
/// - Tags hold no whitespace and no `,`, which the `--tag` filter splits on.
///
/// Returns where the project lives on disk (see [`resolve_inside`]).
fn validate_project(
    base_dir: &Path,
    worktrees: &str,
    info: &mut ProjectInfo,
) -> Result<PathBuf, String> {
    let lists = [
        ("tag", &info.tags),
        ("provides entry", &info.provides),
        ("depends_on entry", &info.depends_on),
    ];
    for (what, value) in [("name", &info.name), ("path", &info.path)]
        .into_iter()
        .chain(info.repo.iter().map(|r| ("repo", r)))
        .chain(
            lists
                .into_iter()
                .flat_map(|(what, l)| l.iter().map(move |v| (what, v))),
        )
    {
        if value.is_empty() {
            return Err(format!("{what} is empty"));
        }
        if value.chars().any(is_unsafe_char) {
            return Err(format!(
                "{what} '{}' contains control characters",
                escape(value)
            ));
        }
    }
    // A name may be a relative path, such as `vendor/lib` for a nested
    // repo: worktrees put the project at `<worktree>/<name>`. So it follows
    // the path rules, and must already be in their normal form.
    let normal = normalize_project_path(&info.name).map_err(|why| format!("name: {why}"))?;
    if normal != info.name {
        return Err(format!(
            "name must be a plain relative path such as 'api' or 'vendor/lib', not '{}'",
            info.name
        ));
    }
    if info.repo.as_deref().is_some_and(|r| r.starts_with('-')) {
        return Err("repo must not start with '-'".into());
    }
    if let Some(tag) = info
        .tags
        .iter()
        .find(|t| t.contains(|c: char| c.is_whitespace() || c == ','))
    {
        return Err(format!("tag '{tag}' contains whitespace or ','"));
    }
    info.path = normalize_project_path(&info.path)?;
    let in_worktrees = |value: &str| {
        let (value, worktrees) = (value.to_ascii_lowercase(), worktrees.to_ascii_lowercase());
        value == worktrees || value.starts_with(&format!("{worktrees}/"))
    };
    for value in [&info.name, &info.path] {
        if in_worktrees(value) {
            return Err(format!(
                "'{value}' is inside {worktrees}, where worktrees live"
            ));
        }
    }
    let location = resolve_inside(base_dir, &info.path)?;
    // Where it really goes must not be reserved either (`alias -> .git`)
    let base = base_dir
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", escape_path(base_dir)))?;
    if let Ok(inside) = location.strip_prefix(&base) {
        let parts: Vec<String> = inside
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        if parts.iter().any(|p| p.eq_ignore_ascii_case(".git")) || in_worktrees(&parts.join("/")) {
            return Err(format!(
                "path '{}' leads into .git or {worktrees} through a symlink",
                info.path
            ));
        }
    }
    Ok(location)
}

/// Check a directory a config names relative to its own directory, such as
/// `worktrees_dir`, by the rules for project paths, and return its normal
/// form. (An absolute or outside location is the user's to choose, through
/// an environment variable such as `META_WORKTREES`, not the config's.)
pub fn relative_dir(base_dir: &Path, raw: &str) -> Result<String, String> {
    // An absolute path is fine when it lies inside base_dir
    let raw_path = Path::new(raw);
    let relative = if raw_path.is_absolute() {
        // the real path of the nearest part that exists, then the rest
        let real = |p: &Path| {
            let existing = p.ancestors().find(|a| a.exists())?;
            let rest = p.strip_prefix(existing).ok()?;
            Some(existing.canonicalize().ok()?.join(rest))
        };
        real(base_dir)
            .zip(real(raw_path))
            .and_then(|(base, path)| {
                path.strip_prefix(base)
                    .ok()
                    .map(|rest| rest.to_string_lossy().into_owned())
            })
            .ok_or_else(|| format!("'{}' is outside {}", escape(raw), escape_path(base_dir)))?
    } else {
        raw.to_string()
    };
    let normal = normalize_project_path(&relative)?;
    resolve_inside(base_dir, &normal)?;
    Ok(normal)
}

/// Refuse two projects at one location, or one inside another's directory:
/// a clone of the outer repo could check out a symlink that the inner path
/// then follows out of the workspace. Locations are compared as resolved
/// through existing symlinks, and without case, which some filesystems
/// ignore. A repo inside another belongs in a nested `.meta` (`meta: true`).
fn check_no_overlap(projects: &[(ProjectInfo, PathBuf)]) -> Result<(), String> {
    let keyed: Vec<_> = projects
        .iter()
        .map(|(info, location)| {
            (
                PathBuf::from(location.to_string_lossy().to_lowercase()),
                info,
            )
        })
        .collect();
    for (i, (outer_key, outer)) in keyed.iter().enumerate() {
        for (inner_key, inner) in keyed.iter().skip(i + 1) {
            let (outer, inner) = if inner_key.starts_with(outer_key) {
                (outer, inner)
            } else if outer_key.starts_with(inner_key) {
                (inner, outer)
            } else {
                continue;
            };
            return Err(format!(
                "project '{}' (path '{}') is at or inside project '{}' (path '{}'); \
                 list it in a nested .meta instead",
                escape(&inner.name),
                escape(&inner.path),
                escape(&outer.name),
                escape(&outer.path)
            ));
        }
    }
    Ok(())
}

/// What is wrong with a name or path component, if anything. The same
/// `.meta` serves every platform, so each must name one and the same
/// directory everywhere:
/// - only ASCII letters, digits, space and `._-+@`, which no filesystem
///   folds, normalizes or treats specially (Unicode names can alias each
///   other on macOS and Windows; `<>:"|?*` and `C:` mean something on Windows);
/// - no trailing dot or space, which Windows drops (`a.` is `a` there);
/// - not a Windows device name such as `NUL` or `COM1`;
/// - not `.git`, which would put a repo inside a git directory (hooks).
fn component_problem(part: &str) -> Option<&'static str> {
    if !part
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-' | '+' | '@'))
    {
        return Some("a character other than ASCII letters, digits, space and ._-+@");
    }
    if part.ends_with(['.', ' ']) {
        return Some("a trailing dot or space, which Windows drops");
    }
    let stem = part
        .split('.')
        .next()
        .unwrap_or(part)
        .trim_end()
        .to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|n| n.len() == 1 && n.chars().all(|c| c.is_ascii_digit()))
    };
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered("COM") || numbered("LPT")
    {
        return Some("a Windows device name");
    }
    if part.eq_ignore_ascii_case(".git") {
        return Some("the name .git");
    }
    None
}

/// Normalize a project path to `a/b` form, rejecting anything that is not a
/// plain relative path below the config's directory.
fn normalize_project_path(raw: &str) -> Result<String, String> {
    let unified = raw.replace('\\', "/");
    if unified.starts_with('/') {
        return Err(format!("path '{}' must be relative", escape(raw)));
    }
    let mut parts = Vec::new();
    for part in unified.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("path '{}' must not contain '..'", escape(raw))),
            part => match component_problem(part) {
                Some(why) => {
                    return Err(format!("path '{}' has a component with {why}", escape(raw)))
                }
                None => parts.push(part),
            },
        }
    }
    if parts.is_empty() {
        return Err(format!(
            "path '{}' must name a directory below the .meta file",
            escape(raw)
        ));
    }
    Ok(parts.join("/"))
}

/// Resolve `rel` below `base_dir` through the symlinks that exist now, and
/// refuse it if one leads outside `base_dir` or if it ends at `base_dir`.
/// Returns the project's location: the real path of the part that exists,
/// followed by the part not created yet.
pub fn resolve_inside(base_dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let base = base_dir
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", escape_path(base_dir)))?;
    if rel.starts_with('/') || rel.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return Err(format!("path '{rel}' must be a plain relative path"));
    }
    let mut current = base.clone();
    let mut exists = true;
    for part in rel.split('/') {
        current.push(part);
        if !exists {
            continue;
        }
        match current.symlink_metadata() {
            Err(_) => exists = false,
            Ok(meta) if meta.file_type().is_symlink() => match current.canonicalize() {
                Ok(target) if target.starts_with(&base) => current = target,
                Ok(_) => {
                    return Err(format!(
                        "path '{rel}' leads outside {} through the symlink '{}'",
                        escape_path(&base),
                        escape_path(&current)
                    ))
                }
                // Its target may not exist yet, but where it would lead
                // cannot be checked, so it is refused
                Err(e) => {
                    return Err(format!(
                        "path '{rel}' goes through the symlink '{}', which cannot be resolved: {e}",
                        escape_path(&current)
                    ))
                }
            },
            Ok(_) => {}
        }
    }
    if current == base {
        return Err(format!(
            "path '{rel}' leads to the directory of the .meta file itself"
        ));
    }
    Ok(current)
}

/// Load defaults from a meta config file.
/// Returns MetaDefaults::default() if no config found or on parse error.
pub fn load_meta_defaults(start_dir: &Path) -> MetaDefaults {
    let Some((config_path, _format)) = find_meta_config_in(start_dir) else {
        return MetaDefaults::default();
    };

    let config_str = match std::fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(_) => return MetaDefaults::default(),
    };

    let path_str = config_path.to_string_lossy();
    let config: MetaConfig = if path_str.ends_with(".yaml") || path_str.ends_with(".yml") {
        serde_yaml_ng::from_str(&config_str).unwrap_or_default()
    } else {
        serde_json::from_str(&config_str).unwrap_or_default()
    };

    config.defaults
}

// ============================================================================
// Tree Walking
// ============================================================================

/// A node in the meta project tree, representing a project and its nested children.
#[derive(Debug, Clone, Serialize)]
pub struct MetaTreeNode {
    pub info: ProjectInfo,
    pub is_meta: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<MetaTreeNode>,
}

/// Walk a meta repository tree, discovering nested .meta repos.
///
/// Parses the .meta config at `start_dir` and for each project checks
/// if it has its own .meta file. Recursively expands children up to `max_depth`.
/// Uses cycle detection via path canonicalization.
///
/// `max_depth` of `None` means unlimited recursion.
/// `max_depth` of `Some(0)` means no recursion (only top-level projects).
pub fn walk_meta_tree(
    start_dir: &Path,
    max_depth: Option<usize>,
) -> anyhow::Result<Vec<MetaTreeNode>> {
    let (config_path, _format) = find_meta_config(start_dir, None)
        .ok_or_else(|| anyhow::anyhow!("No .meta config found in {}", escape_path(start_dir)))?;

    let (projects, _ignore) = parse_meta_config(&config_path)?;
    let meta_dir = config_path.parent().unwrap_or(Path::new("."));

    let mut visited = std::collections::HashSet::new();
    visited.insert(meta_dir.canonicalize().unwrap_or(meta_dir.to_path_buf()));

    let depth = max_depth.unwrap_or(usize::MAX);
    walk_inner(meta_dir, &projects, depth, 0, &mut visited)
}

/// Flatten a meta tree into fully-qualified path strings.
///
/// For nested children, paths are joined with their parent
/// (e.g., a child with path "grandchild" under parent "child" becomes "child/grandchild").
pub fn flatten_meta_tree(nodes: &[MetaTreeNode]) -> Vec<String> {
    let mut paths = Vec::new();
    flatten_inner(nodes, "", &mut paths);
    paths
}

fn flatten_inner(nodes: &[MetaTreeNode], prefix: &str, paths: &mut Vec<String>) {
    for node in nodes {
        let full_path = if prefix.is_empty() {
            node.info.path.clone()
        } else {
            format!("{}/{}", prefix, node.info.path)
        };
        paths.push(full_path.clone());
        flatten_inner(&node.children, &full_path, paths);
    }
}

/// Build a map from full path to (resolved path, ProjectInfo) for nested lookups.
///
/// This is useful for looking up projects by their full nested path
/// (e.g., "vendor/nested-lib" -> resolved filesystem path and project info).
pub fn build_project_map(
    nodes: &[MetaTreeNode],
    base_dir: &Path,
    prefix: &str,
) -> std::collections::HashMap<String, (PathBuf, ProjectInfo)> {
    let mut map = std::collections::HashMap::new();
    for node in nodes {
        let full_path = if prefix.is_empty() {
            node.info.path.clone()
        } else {
            format!("{}/{}", prefix, node.info.path)
        };
        let resolved_path = base_dir.join(&full_path);
        map.insert(full_path.clone(), (resolved_path, node.info.clone()));
        // Recurse into children
        map.extend(build_project_map(&node.children, base_dir, &full_path));
    }
    map
}

// ============================================================================
// Orphan Detection
// ============================================================================

/// Information about an orphaned nested meta repo.
#[derive(Debug, Clone)]
pub struct OrphanWarning {
    /// Path to the current (orphaned) meta directory
    pub current: PathBuf,
    /// Path to the parent meta directory
    pub parent: PathBuf,
    /// Suggested key to add to parent's .meta file
    pub suggested_key: String,
    /// Format of the parent's config file (for showing appropriate syntax)
    pub parent_format: ConfigFormat,
}

/// Find a parent .meta config (if any) above the given meta directory.
///
/// Starts searching from the parent of `meta_dir` and walks up.
/// Returns None if this is the root meta repo (no parent .meta found).
pub fn find_parent_meta_config(meta_dir: &Path) -> Option<(PathBuf, ConfigFormat)> {
    let parent = meta_dir.parent()?;
    find_meta_config(parent, None)
}

/// Find the root (topmost) meta directory by walking up through parent .meta configs.
///
/// Starting from `meta_dir`, repeatedly calls `find_parent_meta_config` until
/// no more parents are found. Returns the topmost meta directory in the hierarchy.
/// If `meta_dir` is already the root, returns it unchanged.
pub fn find_root_meta_dir(meta_dir: &Path) -> PathBuf {
    let mut current = meta_dir.to_path_buf();
    loop {
        match find_parent_meta_config(&current) {
            Some((parent_config_path, _format)) => {
                current = parent_config_path
                    .parent()
                    .expect("config file path should always have a parent directory")
                    .to_path_buf();
            }
            None => break current,
        }
    }
}

/// Check if a meta directory is tracked by its parent meta config.
///
/// Returns `Some(OrphanWarning)` if there's a parent .meta that doesn't include
/// this directory in its project list (directly or transitively).
/// Returns `None` if tracked or if there's no parent meta.
pub fn check_orphan_status(meta_dir: &Path) -> Option<OrphanWarning> {
    let (parent_config, parent_format) = find_parent_meta_config(meta_dir)?;
    let parent_meta_dir = parent_config.parent()?;

    // Walk the parent's project tree to see what's tracked
    let tree = walk_meta_tree(parent_meta_dir, None).ok()?;
    let flat_paths = flatten_meta_tree(&tree);

    // Get the relative path from parent to current
    let relative = meta_dir.strip_prefix(parent_meta_dir).ok()?;
    let relative_str = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");

    // Check if this path is in the flattened tree
    if flat_paths.iter().any(|p| p == &*relative_str) {
        None // Tracked, not orphan
    } else {
        // Extract the first path component as the suggested key
        let suggested_key = relative
            .components()
            .next()?
            .as_os_str()
            .to_string_lossy()
            .to_string();

        Some(OrphanWarning {
            current: meta_dir.to_path_buf(),
            parent: parent_meta_dir.to_path_buf(),
            suggested_key,
            parent_format,
        })
    }
}

fn walk_inner(
    base_dir: &Path,
    projects: &[ProjectInfo],
    max_depth: usize,
    current_depth: usize,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> anyhow::Result<Vec<MetaTreeNode>> {
    let mut nodes = Vec::new();

    for project in projects {
        let project_dir = base_dir.join(&project.path);

        // Check if this project has its own .meta file directly in its directory
        let nested_config = if project_dir.is_dir() {
            find_meta_config(&project_dir, None)
                .filter(|(path, _)| path.parent() == Some(project_dir.as_path()))
        } else {
            None
        };
        let has_meta = nested_config.is_some();

        // Recurse into children if within depth limit and this is a meta repo.
        // A nested config that fails to parse or validate is an error, not an
        // empty subtree: skipping it would silently drop its repos.
        let mut children = vec![];
        if let Some((nested_config_path, _)) = nested_config {
            let canonical = project_dir.canonicalize().unwrap_or(project_dir.clone());
            // `visited` stops cycles through symlinks
            if current_depth < max_depth && visited.insert(canonical) {
                let (nested_projects, _) = parse_meta_config(&nested_config_path)?;
                children = walk_inner(
                    &project_dir,
                    &nested_projects,
                    max_depth,
                    current_depth + 1,
                    visited,
                )?;
            }
        }

        nodes.push(MetaTreeNode {
            info: project.clone(),
            is_meta: has_meta,
            children,
        });
    }

    nodes.sort_by(|a, b| a.info.name.cmp(&b.info.name));
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_walk_meta_tree_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let result = walk_meta_tree(dir.path(), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_walk_meta_tree_empty_projects() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".meta"), r#"{"projects": {}}"#).unwrap();
        let tree = walk_meta_tree(dir.path(), None).unwrap();
        assert!(tree.is_empty());
    }

    #[test]
    fn test_walk_meta_tree_multiple_projects_sorted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zebra")).unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        std::fs::create_dir(dir.path().join("middle")).unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {
                "zebra": "git@github.com:org/zebra.git",
                "alpha": "git@github.com:org/alpha.git",
                "middle": "git@github.com:org/middle.git"
            }}"#,
        )
        .unwrap();

        let tree = walk_meta_tree(dir.path(), None).unwrap();
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].info.name, "alpha");
        assert_eq!(tree[1].info.name, "middle");
        assert_eq!(tree[2].info.name, "zebra");
    }

    #[test]
    fn test_walk_meta_tree_is_meta_flag() {
        let dir = tempfile::tempdir().unwrap();
        let has_meta = dir.path().join("has_meta");
        let no_meta = dir.path().join("no_meta");
        std::fs::create_dir(&has_meta).unwrap();
        std::fs::create_dir(&no_meta).unwrap();
        std::fs::write(has_meta.join(".meta"), r#"{"projects": {}}"#).unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {
                "has_meta": "git@github.com:org/has_meta.git",
                "no_meta": "git@github.com:org/no_meta.git"
            }}"#,
        )
        .unwrap();

        let tree = walk_meta_tree(dir.path(), None).unwrap();
        let has = tree.iter().find(|n| n.info.name == "has_meta").unwrap();
        let no = tree.iter().find(|n| n.info.name == "no_meta").unwrap();
        assert!(has.is_meta);
        assert!(!no.is_meta);
    }

    #[test]
    fn test_walk_meta_tree_depth_zero_no_recursion() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        let grandchild = child.join("grandchild");
        std::fs::create_dir_all(&grandchild).unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"child": "git@github.com:org/child.git"}}"#,
        )
        .unwrap();
        std::fs::write(
            child.join(".meta"),
            r#"{"projects": {"grandchild": "git@github.com:org/grandchild.git"}}"#,
        )
        .unwrap();

        let tree = walk_meta_tree(dir.path(), Some(0)).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].info.name, "child");
        assert!(tree[0].is_meta);
        assert!(tree[0].children.is_empty()); // No recursion
    }

    #[test]
    fn test_walk_meta_tree_rejects_symlink_cycles() {
        // A nested project that links back to its own meta dir (or any
        // ancestor) is refused, so the walk cannot loop
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir(&child).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&child, child.join("loop")).unwrap();

        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"child": "git@github.com:org/child.git"}}"#,
        )
        .unwrap();
        std::fs::write(
            child.join(".meta"),
            r#"{"projects": {"loop": "git@github.com:org/loop.git"}}"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            let err = walk_meta_tree(dir.path(), None).unwrap_err().to_string();
            assert!(
                err.contains("leads to the directory of the .meta file itself"),
                "{err}"
            );
        }
    }

    #[test]
    fn test_flatten_meta_tree_empty() {
        let paths = flatten_meta_tree(&[]);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_flatten_meta_tree_deeply_nested() {
        let dir = tempfile::tempdir().unwrap();
        let l1 = dir.path().join("l1");
        let l2 = l1.join("l2");
        let l3 = l2.join("l3");
        std::fs::create_dir_all(&l3).unwrap();

        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"l1": "git@github.com:org/l1.git"}}"#,
        )
        .unwrap();
        std::fs::write(
            l1.join(".meta"),
            r#"{"projects": {"l2": "git@github.com:org/l2.git"}}"#,
        )
        .unwrap();
        std::fs::write(
            l2.join(".meta"),
            r#"{"projects": {"l3": "git@github.com:org/l3.git"}}"#,
        )
        .unwrap();

        let tree = walk_meta_tree(dir.path(), None).unwrap();
        let paths = flatten_meta_tree(&tree);

        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0], "l1");
        assert_eq!(paths[1], "l1/l2");
        assert_eq!(paths[2], "l1/l2/l3");
    }

    #[test]
    fn test_walk_meta_tree_nonexistent_project_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Project listed in .meta but directory doesn't exist
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"missing": "git@github.com:org/missing.git"}}"#,
        )
        .unwrap();

        let tree = walk_meta_tree(dir.path(), None).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].info.name, "missing");
        assert!(!tree[0].is_meta);
        assert!(tree[0].children.is_empty());
    }

    #[test]
    fn test_walk_meta_tree_extended_format() {
        let dir = tempfile::tempdir().unwrap();
        let custom_path = dir.path().join("custom/path");
        std::fs::create_dir_all(&custom_path).unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {
                "myproject": {
                    "repo": "git@github.com:org/myproject.git",
                    "path": "custom/path",
                    "tags": ["frontend", "react"]
                }
            }}"#,
        )
        .unwrap();

        let tree = walk_meta_tree(dir.path(), None).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].info.name, "myproject");
        assert_eq!(tree[0].info.path, "custom/path");
        assert_eq!(tree[0].info.tags, vec!["frontend", "react"]);
    }

    #[test]
    fn test_load_meta_defaults_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let defaults = load_meta_defaults(dir.path());
        // Defaults to parallel=true when no config exists
        assert!(defaults.parallel);
    }

    #[test]
    fn test_load_meta_defaults_no_defaults_section() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".meta"), r#"{"projects": {}}"#).unwrap();
        let defaults = load_meta_defaults(dir.path());
        // Defaults to parallel=true when defaults section is missing
        assert!(defaults.parallel);
    }

    #[test]
    fn test_load_meta_defaults_parallel_true() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {}, "defaults": {"parallel": true}}"#,
        )
        .unwrap();
        let defaults = load_meta_defaults(dir.path());
        assert!(defaults.parallel);
    }

    #[test]
    fn test_load_meta_defaults_parallel_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {}, "defaults": {"parallel": false}}"#,
        )
        .unwrap();
        let defaults = load_meta_defaults(dir.path());
        assert!(!defaults.parallel);
    }

    #[test]
    fn test_load_meta_defaults_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".meta.yaml"),
            "projects: {}\ndefaults:\n  parallel: true\n",
        )
        .unwrap();
        let defaults = load_meta_defaults(dir.path());
        assert!(defaults.parallel);
    }

    // ============================================================================
    // Nested meta repos (meta: true field)
    // ============================================================================

    #[test]
    fn test_parse_nested_meta_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{
                "projects": {
                    "core": "git@github.com:org/core.git",
                    "vendor": { "repo": "git@github.com:org/vendor.git", "meta": true }
                }
            }"#,
        )
        .unwrap();

        let (projects, _) = parse_meta_config(&dir.path().join(".meta")).unwrap();
        assert_eq!(projects.len(), 2);

        let vendor = projects.iter().find(|p| p.name == "vendor").unwrap();
        assert_eq!(
            vendor.repo.as_ref().unwrap(),
            "git@github.com:org/vendor.git"
        );
        assert_eq!(vendor.path, "vendor");

        let core = projects.iter().find(|p| p.name == "core").unwrap();
        assert!(core.repo.is_some());
        assert_eq!(core.repo.as_ref().unwrap(), "git@github.com:org/core.git");
    }

    #[test]
    fn test_parse_nested_meta_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".meta.yaml"),
            "projects:\n  core: git@github.com:org/core.git\n  vendor:\n    repo: git@github.com:org/vendor.git\n    meta: true\n",
        )
        .unwrap();

        let (projects, _) = parse_meta_config(&dir.path().join(".meta.yaml")).unwrap();
        assert_eq!(projects.len(), 2);

        let vendor = projects.iter().find(|p| p.name == "vendor").unwrap();
        assert_eq!(
            vendor.repo.as_ref().unwrap(),
            "git@github.com:org/vendor.git"
        );
    }

    #[test]
    fn test_parse_nested_meta_with_custom_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{
                "projects": {
                    "vendor": {
                        "repo": "git@github.com:org/vendor.git",
                        "path": "third_party/vendor",
                        "meta": true
                    }
                }
            }"#,
        )
        .unwrap();

        let (projects, _) = parse_meta_config(&dir.path().join(".meta")).unwrap();
        assert_eq!(projects.len(), 1);

        let vendor = &projects[0];
        assert_eq!(
            vendor.repo.as_ref().unwrap(),
            "git@github.com:org/vendor.git"
        );
        assert_eq!(vendor.name, "vendor");
        assert_eq!(vendor.path, "third_party/vendor");
    }

    #[test]
    fn test_has_no_repo() {
        let info = ProjectInfo {
            name: "vendor".to_string(),
            path: "vendor".to_string(),
            repo: None,
            tags: vec![],
            provides: vec![],
            depends_on: vec![],
            meta: false,
        };
        assert!(info.has_no_repo());

        let info_with_repo = ProjectInfo {
            name: "core".to_string(),
            path: "core".to_string(),
            repo: Some("git@github.com:org/core.git".to_string()),
            tags: vec![],
            provides: vec![],
            depends_on: vec![],
            meta: false,
        };
        assert!(!info_with_repo.has_no_repo());
    }

    #[test]
    fn test_walk_meta_tree_nested_meta_with_children() {
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("vendor");
        let nested = vendor.join("tree-sitter-markdown");
        std::fs::create_dir_all(&nested).unwrap();

        // Root .meta with nested meta project (has repo URL + meta: true)
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"vendor": {"repo": "git@github.com:org/vendor.git", "meta": true}}}"#,
        )
        .unwrap();

        // Nested .meta inside vendor (simulates what exists after cloning)
        std::fs::write(
            vendor.join(".meta"),
            r#"{"projects": {"tree-sitter-markdown": "git@github.com:org/tsm.git"}}"#,
        )
        .unwrap();

        let tree = walk_meta_tree(dir.path(), None).unwrap();
        assert_eq!(tree.len(), 1);

        let vendor_node = &tree[0];
        assert_eq!(vendor_node.info.name, "vendor");
        assert_eq!(
            vendor_node.info.repo.as_ref().unwrap(),
            "git@github.com:org/vendor.git"
        );
        assert!(vendor_node.is_meta);
        assert_eq!(vendor_node.children.len(), 1);

        let nested_node = &vendor_node.children[0];
        assert_eq!(nested_node.info.name, "tree-sitter-markdown");
        assert!(nested_node.info.repo.is_some());
    }

    // ============================================================================
    // Orphan detection tests
    // ============================================================================

    #[test]
    fn test_find_parent_meta_config_none_at_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".meta"), r#"{"projects": {}}"#).unwrap();

        // No parent above this directory
        let result = find_parent_meta_config(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_find_parent_meta_config_finds_parent() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("vendor");
        std::fs::create_dir(&nested).unwrap();

        // Parent .meta
        std::fs::write(dir.path().join(".meta"), r#"{"projects": {}}"#).unwrap();
        // Nested .meta
        std::fs::write(nested.join(".meta"), r#"{"projects": {}}"#).unwrap();

        let result = find_parent_meta_config(&nested);
        assert!(result.is_some());
        let (path, _) = result.unwrap();
        assert_eq!(path, dir.path().join(".meta"));
    }

    #[test]
    fn test_check_orphan_status_not_orphan_when_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("vendor");
        std::fs::create_dir(&vendor).unwrap();

        // Parent .meta tracks vendor with repo URL and meta: true
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"vendor": {"repo": "git@github.com:org/vendor.git", "meta": true}}}"#,
        )
        .unwrap();
        // Nested .meta (simulates what exists after cloning vendor)
        std::fs::write(vendor.join(".meta"), r#"{"projects": {}}"#).unwrap();

        let result = check_orphan_status(&vendor);
        assert!(result.is_none(), "vendor should not be orphan when tracked");
    }

    #[test]
    fn test_check_orphan_status_is_orphan_when_not_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("vendor");
        std::fs::create_dir(&vendor).unwrap();

        // Parent .meta does NOT track vendor
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"other": "git@github.com:org/other.git"}}"#,
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("other")).unwrap();
        // Nested .meta
        std::fs::write(vendor.join(".meta"), r#"{"projects": {}}"#).unwrap();

        let result = check_orphan_status(&vendor);
        assert!(result.is_some(), "vendor should be orphan when not tracked");

        let warning = result.unwrap();
        assert_eq!(warning.current, vendor);
        assert_eq!(warning.parent, dir.path());
        assert_eq!(warning.suggested_key, "vendor");
    }

    #[test]
    fn test_check_orphan_status_no_parent_means_not_orphan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".meta"), r#"{"projects": {}}"#).unwrap();

        // No parent .meta above this one
        let result = check_orphan_status(dir.path());
        assert!(result.is_none(), "should not be orphan if no parent exists");
    }

    #[test]
    fn test_check_orphan_status_deeply_nested_tracked() {
        // Test 3 levels deep: root -> vendor -> sub-vendor -> deep-lib
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("vendor");
        let sub_vendor = vendor.join("sub-vendor");
        let deep_lib = sub_vendor.join("deep-lib");
        std::fs::create_dir_all(&deep_lib).unwrap();

        // Root tracks vendor as nested meta
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"vendor": {"repo": "git@github.com:org/vendor.git", "meta": true}}}"#,
        )
        .unwrap();

        // Vendor tracks sub-vendor as nested meta
        std::fs::write(
            vendor.join(".meta"),
            r#"{"projects": {"sub-vendor": {"repo": "git@github.com:org/sub-vendor.git", "meta": true}}}"#,
        )
        .unwrap();

        // Sub-vendor tracks deep-lib
        std::fs::write(
            sub_vendor.join(".meta"),
            r#"{"projects": {"deep-lib": "git@github.com:org/deep-lib.git"}}"#,
        )
        .unwrap();

        // Check from sub-vendor's perspective (should not be orphan - tracked by vendor)
        let result = check_orphan_status(&sub_vendor);
        assert!(
            result.is_none(),
            "sub-vendor should not be orphan when tracked by vendor"
        );
    }

    #[test]
    fn test_check_orphan_status_deeply_nested_orphan() {
        // Test orphan detection at 2 levels deep
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("vendor");
        let orphan_dir = vendor.join("orphan-project");
        std::fs::create_dir_all(&orphan_dir).unwrap();
        std::fs::create_dir(dir.path().join("backend")).unwrap();

        // Root tracks vendor and backend
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"vendor": {"repo": "git@github.com:org/vendor.git", "meta": true}, "backend": "git@github.com:org/backend.git"}}"#,
        )
        .unwrap();

        // Vendor does NOT track orphan-project
        std::fs::write(
            vendor.join(".meta"),
            r#"{"projects": {"lib": "git@github.com:org/lib.git"}}"#,
        )
        .unwrap();
        std::fs::create_dir(vendor.join("lib")).unwrap();

        // Orphan has its own .meta
        std::fs::write(orphan_dir.join(".meta"), r#"{"projects": {}}"#).unwrap();

        // Check from orphan's perspective
        let result = check_orphan_status(&orphan_dir);
        assert!(
            result.is_some(),
            "orphan-project should be orphan when not tracked by vendor"
        );

        let warning = result.unwrap();
        assert_eq!(warning.suggested_key, "orphan-project");
        assert_eq!(warning.parent, vendor);
    }

    #[test]
    fn test_walk_meta_tree_fails_on_malformed_nested_meta() {
        // A broken nested .meta must surface, not silently drop its repos
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("vendor");
        std::fs::create_dir(&vendor).unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"vendor": {"repo": "git@github.com:org/vendor.git", "meta": true}}}"#,
        )
        .unwrap();
        std::fs::write(
            vendor.join(".meta"),
            r#"{"projects": {"x": {"path": "../../escape"}}}"#,
        )
        .unwrap();

        let err = walk_meta_tree(dir.path(), None).unwrap_err().to_string();
        assert!(err.contains("Invalid project 'x'"), "{err}");
        assert!(err.contains("vendor"), "{err}");
    }

    fn parse_err_in(dir: &Path, json: &str) -> String {
        std::fs::write(dir.join(".meta"), json).unwrap();
        parse_meta_config(&dir.join(".meta"))
            .unwrap_err()
            .to_string()
    }

    fn parse_err(json: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        parse_err_in(dir.path(), json)
    }

    #[test]
    fn test_parse_rejects_paths_outside_the_workspace() {
        for path in [
            "../../dropped",
            "a/../../b",
            "/abs/dir",
            "C:/x",
            "C:x",
            "\\\\host\\s",
            ".",
            "./",
        ] {
            let err = parse_err(&format!(
                r#"{{"projects": {{"p": {{"repo": "u", "path": {path:?}}}}}}}"#
            ));
            assert!(err.contains("Invalid project 'p' in "), "{path}: {err}");
            assert!(err.contains(".meta: path"), "{path}: {err}");
        }
    }

    #[test]
    fn test_parse_rejects_bad_names_tags_and_repos() {
        for (json, what) in [
            (
                r#"{"projects": {"../../../../escaped-wt": {"repo": "u", "path": "a"}}}"#,
                "name: path '../../../../escaped-wt' must not contain '..'",
            ),
            (
                r#"{"projects": {"/abs": "u"}}"#,
                "name: path '/abs' must be relative",
            ),
            (r#"{"projects": {"a//b": "u"}}"#, "plain relative path"),
            (
                r#"{"projects": {"..": {"repo": "u", "path": "a"}}}"#,
                "must not contain '..'",
            ),
            (
                r#"{"projects": {"x\u001b]0;T\u0007": "u"}}"#,
                "control characters",
            ),
            (
                r#"{"projects": {"a": {"repo": "u", "tags": ["n\nfake"]}}}"#,
                "control characters",
            ),
            (
                r#"{"projects": {"a": {"repo": "u", "tags": ["a,b"]}}}"#,
                "whitespace or ','",
            ),
            (
                r#"{"projects": {"a": {"repo": "u", "tags": [""]}}}"#,
                "tag is empty",
            ),
            (
                r#"{"projects": {"a": "--upload-pack=touch x"}}"#,
                "must not start with '-'",
            ),
            (
                r#"{"projects": {"a": {"repo": "u", "path": "t\u001b[2J"}}}"#,
                "control characters",
            ),
        ] {
            let err = parse_err(json);
            assert!(err.contains(what), "{json}: {err}");
            // The offending value is escaped in the message itself
            assert!(!err.contains('\u{1b}'), "{json}: {err}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_rejects_overlap_through_symlink_alias() {
        // `alias` points at `real`, so `alias/outer/link/x` sits inside
        // `real/outer`, whose clone could plant `link` pointing anywhere.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"outer": {"repo": "u", "path": "real/outer"}, "inner": {"repo": "u", "path": "alias/outer/link/x"}}}"#,
        )
        .unwrap();
        let err = parse_meta_config(&dir.path().join(".meta"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "project 'inner' (path 'alias/outer/link/x') is at or inside project 'outer'"
            ),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_rejects_reserved_dirs_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::os::unix::fs::symlink(dir.path().join(".git"), dir.path().join("alias")).unwrap();
        let err = parse_err_in(
            dir.path(),
            r#"{"projects": {"p": {"repo": "u", "path": "alias/hooks"}}}"#,
        );
        assert!(err.contains("leads into .git or .worktrees"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_allows_symlink_alias_of_the_workspace_itself() {
        // `here -> .` is harmless as a step on the way; only ending at the
        // config's own directory is refused
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("here")).unwrap();
        let meta = dir.path().join(".meta");
        std::fs::write(
            &meta,
            r#"{"projects": {"svc": {"repo": "u", "path": "here/service"}}}"#,
        )
        .unwrap();
        assert!(parse_meta_config(&meta).is_ok());

        std::fs::write(
            &meta,
            r#"{"projects": {"svc": {"repo": "u", "path": "here"}}}"#,
        )
        .unwrap();
        let err = parse_meta_config(&meta).unwrap_err().to_string();
        assert!(
            err.contains("leads to the directory of the .meta file itself"),
            "{err}"
        );
    }

    #[cfg(unix)] // Windows refuses control characters in file names
    #[test]
    fn test_config_errors_escape_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws\nfake line\u{1b}[2J");
        std::fs::create_dir(&ws).unwrap();
        std::fs::write(ws.join(".meta"), "{not json").unwrap();
        let err = format!("{:#}", parse_meta_config(&ws.join(".meta")).unwrap_err());
        assert!(!err.contains('\n') && !err.contains('\u{1b}'), "{err}");
        assert!(err.contains("ws\\nfake line\\u{1b}[2J"), "{err}");
    }

    #[test]
    fn test_parse_rejects_drive_prefixes_anywhere() {
        for json in [
            r#"{"projects": {"p": {"repo": "u", "path": "./C:/outside"}}}"#,
            r#"{"projects": {"p": {"repo": "u", "path": "a/C:b"}}}"#,
            r#"{"projects": {"C:outside": {"repo": "u", "path": "safe"}}}"#,
        ] {
            let err = parse_err(json);
            assert!(err.contains("ASCII letters"), "{json}: {err}");
        }
    }

    #[test]
    fn test_parse_rejects_names_and_paths_that_alias_or_mislead() {
        for (json, what) in [
            (
                r#"{"projects": {"p": {"repo": "u", "path": "a./x"}}}"#,
                "trailing dot",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "a /x"}}}"#,
                "trailing dot or space",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "x/nul"}}}"#,
                "device name",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "con.txt"}}}"#,
                "device name",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "x/com1"}}}"#,
                "device name",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "C:x"}}}"#,
                "ASCII letters",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "ab:c"}}}"#,
                "ASCII letters",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "PROGRA~1/x"}}}"#,
                "ASCII letters",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "caf\u00e9"}}}"#,
                "ASCII letters",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "cafe\u0301"}}}"#,
                "ASCII letters",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "COM\u00b9"}}}"#,
                "ASCII letters",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": ".git/hooks"}}}"#,
                "the name .git",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "x/.GIT/hooks"}}}"#,
                "the name .git",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": ".worktrees/t"}}}"#,
                "inside .worktrees",
            ),
            (
                r#"{"projects": {"p": {"repo": "u", "path": "sets/x"}}, "worktrees_dir": "sets"}"#,
                "inside sets",
            ),
            (r#"{"projects": {"x|y": "u"}}"#, "ASCII letters"),
            (
                r#"{"projects": {"a.": {"repo": "u", "path": "b"}}}"#,
                "trailing dot",
            ),
        ] {
            let err = parse_err(json);
            assert!(err.contains(what), "{json}: {err}");
        }
        // Ordinary names that only look similar are fine
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"console": "u", "a.b": "u", "com10": "u", "x y+z@w": "u", ".github": "u"}}"#,
        )
        .unwrap();
        assert!(parse_meta_config(&dir.path().join(".meta")).is_ok());
    }

    #[test]
    fn test_parse_rejects_overlapping_paths() {
        let err = parse_err(
            r#"{"projects": {"outer": {"repo": "u", "path": "a"}, "inner": {"repo": "u", "path": "a/link/x"}}}"#,
        );
        assert!(
            err.contains("project 'inner' (path 'a/link/x') is at or inside project 'outer'"),
            "{err}"
        );

        let err = parse_err(
            r#"{"projects": {"one": {"repo": "u", "path": "x"}, "two": {"repo": "u", "path": "./x/"}}}"#,
        );
        assert!(
            err.contains("project 'two' (path 'x') is at or inside project 'one'"),
            "{err}"
        );

        // Case differences don't separate paths (some filesystems ignore case)
        let err = parse_err(
            r#"{"projects": {"one": {"repo": "u", "path": "Lib"}, "two": {"repo": "u", "path": "lib/x"}}}"#,
        );
        assert!(err.contains("is at or inside project 'one'"), "{err}");

        // A shared name prefix is not nesting
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"a": "u", "a-b": "u", "ab": {"repo": "u", "path": "a.b/c"}}}"#,
        )
        .unwrap();
        assert!(parse_meta_config(&dir.path().join(".meta")).is_ok());
    }

    #[test]
    fn test_parse_accepts_nested_alias_names() {
        // as gitkb's .meta.yaml lists `vendor/tree-sitter-markdown`
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".meta.yaml"),
            "projects:\n  core: u\n  vendor/tree-sitter-markdown: u\n",
        )
        .unwrap();
        let (projects, _) = parse_meta_config(&dir.path().join(".meta.yaml")).unwrap();
        assert_eq!(projects[1].name, "vendor/tree-sitter-markdown");
        assert_eq!(projects[1].path, "vendor/tree-sitter-markdown");
    }

    #[test]
    fn test_relative_dir_stays_inside() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(relative_dir(dir.path(), "wt").unwrap(), "wt");
        let inside = dir.path().join("custom").to_string_lossy().into_owned();
        assert_eq!(relative_dir(dir.path(), &inside).unwrap(), "custom");
        let deeper = dir
            .path()
            .join("build/worktrees")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            relative_dir(dir.path(), &deeper).unwrap(),
            "build/worktrees"
        );
        assert!(relative_dir(dir.path(), "../out").is_err());
        // a value from the config is escaped in the message
        let err = relative_dir(dir.path(), "../bad\nFORGED").unwrap_err();
        assert!(!err.contains('\n') && err.contains("bad\\nFORGED"), "{err}");
        assert!(relative_dir(dir.path(), "/").is_err());
    }

    #[test]
    fn test_parse_normalizes_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"a": {"repo": "u", "path": "./third_party//vendor/"}, "b": {"repo": "u", "path": "x\\y"}}}"#,
        )
        .unwrap();
        let (projects, _) = parse_meta_config(&dir.path().join(".meta")).unwrap();
        assert_eq!(projects[0].path, "third_party/vendor");
        assert_eq!(projects[1].path, "x/y");
    }

    #[cfg(unix)]
    #[test]
    fn test_parse_rejects_symlink_escape() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("inside")).unwrap();
        let meta = dir.path().join(".meta");

        for path in ["link", "link/sub"] {
            std::fs::write(
                &meta,
                format!(r#"{{"projects": {{"p": {{"repo": "u", "path": "{path}"}}}}}}"#),
            )
            .unwrap();
            let err = parse_meta_config(&meta).unwrap_err().to_string();
            assert!(err.contains("through the symlink"), "{path}: {err}");
        }

        // A symlink that stays inside the workspace is fine
        std::fs::write(
            &meta,
            r#"{"projects": {"p": {"repo": "u", "path": "inside/sub"}}}"#,
        )
        .unwrap();
        assert!(parse_meta_config(&meta).is_ok());
    }

    #[test]
    fn test_build_project_map_empty_tree() {
        let dir = tempfile::tempdir().unwrap();
        let tree: Vec<MetaTreeNode> = vec![];
        let map = build_project_map(&tree, dir.path(), "");
        assert!(map.is_empty());
    }

    #[test]
    fn test_build_project_map_with_custom_path() {
        // Test that custom paths are handled correctly in the map
        let dir = tempfile::tempdir().unwrap();
        let custom_path = dir.path().join("custom/path/to/project");
        std::fs::create_dir_all(&custom_path).unwrap();

        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"my-project": {"repo": "git@github.com:org/my-project.git", "path": "custom/path/to/project"}}}"#,
        )
        .unwrap();

        let tree = walk_meta_tree(dir.path(), None).unwrap();
        let map = build_project_map(&tree, dir.path(), "");

        // The key should be the path, not the name
        assert!(map.contains_key("custom/path/to/project"));
        let (path, info) = map.get("custom/path/to/project").unwrap();
        assert_eq!(info.name, "my-project");
        assert_eq!(*path, custom_path);
    }

    #[test]
    fn test_check_orphan_status_yaml_format() {
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("vendor");
        std::fs::create_dir(&vendor).unwrap();

        // Parent .meta.yaml does NOT track vendor
        std::fs::write(
            dir.path().join(".meta.yaml"),
            "projects:\n  other:\n    repo: git@github.com:org/other.git\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("other")).unwrap();

        // Nested .meta (JSON)
        std::fs::write(vendor.join(".meta"), r#"{"projects": {}}"#).unwrap();

        let result = check_orphan_status(&vendor);
        assert!(result.is_some(), "vendor should be orphan when not tracked");

        let warning = result.unwrap();
        assert!(matches!(warning.parent_format, ConfigFormat::Yaml));
    }

    // ============================================================================
    // find_root_meta_dir tests
    // ============================================================================

    #[test]
    fn test_find_root_meta_dir_no_parent() {
        // Single-level meta dir (no parent) → returns self
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".meta"), r#"{"projects": {}}"#).unwrap();

        let root = find_root_meta_dir(dir.path());
        assert_eq!(root, dir.path());
    }

    #[test]
    fn test_find_root_meta_dir_two_levels() {
        // root → child: from child, should return root
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir(&child).unwrap();

        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"child": {"repo": "git@github.com:org/child.git", "meta": true}}}"#,
        )
        .unwrap();
        std::fs::write(child.join(".meta"), r#"{"projects": {}}"#).unwrap();

        let root = find_root_meta_dir(&child);
        assert_eq!(root, dir.path());
    }

    #[test]
    fn test_find_root_meta_dir_three_levels() {
        // root → child → grandchild: from grandchild, should return root
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        let grandchild = child.join("grandchild");
        std::fs::create_dir_all(&grandchild).unwrap();

        std::fs::write(
            dir.path().join(".meta"),
            r#"{"projects": {"child": {"repo": "git@github.com:org/child.git", "meta": true}}}"#,
        )
        .unwrap();
        std::fs::write(
            child.join(".meta"),
            r#"{"projects": {"grandchild": {"repo": "git@github.com:org/gc.git", "meta": true}}}"#,
        )
        .unwrap();
        std::fs::write(grandchild.join(".meta"), r#"{"projects": {}}"#).unwrap();

        let root = find_root_meta_dir(&grandchild);
        assert_eq!(root, dir.path());
    }

    // ============================================================================
    // A `.meta/` directory (meta's local folder) is never config
    // ============================================================================

    #[test]
    fn test_meta_directory_is_not_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".meta/skills")).unwrap();
        assert!(find_meta_config_in(dir.path()).is_none());

        std::fs::write(dir.path().join(".meta.yaml"), "projects: {}\n").unwrap();
        let expected = dir.path().join(".meta.yaml");
        assert_eq!(find_meta_config_in(dir.path()).unwrap().0, expected);
        assert_eq!(find_meta_config(dir.path(), None).unwrap().0, expected);
    }

    #[test]
    fn test_find_meta_config_walks_past_meta_directory() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir_all(child.join(".meta")).unwrap();
        // Legacy `.meta` config file in the parent
        std::fs::write(dir.path().join(".meta"), r#"{"projects": {"child": "x"}}"#).unwrap();

        let (path, _) = find_meta_config(&child, None).unwrap();
        assert_eq!(path, dir.path().join(".meta"));

        let tree = walk_meta_tree(dir.path(), None).unwrap();
        assert!(!tree[0].is_meta);
    }
}
