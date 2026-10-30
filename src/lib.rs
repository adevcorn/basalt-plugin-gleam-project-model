//! Tier-0 project-model plugin for Gleam / BEAM packages.
//!
//! Emits the shared generic project-model JSON schema under
//! `project-model:gleam`, and exposes `gleam build` / `gleam test` /
//! `gleam format --check` as review actions.
//!
//! Review-action output is mapped to agent events: compiler headers
//! (`error:` / `warning:`) and their `┌─ path:line:col` locations become
//! diagnostic entries, and gleeunit's `N passed, M failures` summary closes
//! the test run with the right exit code.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use basalt_plugin_sdk::prelude::*;

basalt_plugin_meta! {
    name:         "gleam-project-model",
    version:      "0.1.1",
    hook_flags:   CAP_PROJECT_MODEL | CAP_REVIEW_ACTIONS | CAP_BUILD_SYSTEM | CAP_TEST_RUNNER | CAP_SHADOW_EXCLUDES,
    provides:     "project-model:gleam",
    requires:     "",
    file_globs:   "",
    activates_on: "gleam.toml\nmanifest.toml\n**/gleam.toml\n**/manifest.toml\n**/*.gleam",
    activation_events: "",
}

/// Gleam compiles into `build/`; it is reproducible output, not source.
#[basalt_plugin]
fn shadow_excludes(_root: &str) -> Vec<u8> {
    br#"["**/build/**"]"#.to_vec()
}

#[cfg(target_arch = "wasm32")]
extern "C" {
    fn basalt_read_file(path_ptr: i32, path_len: i32, out_ptr: i32, out_cap: i32) -> i32;
    fn basalt_list_files(root_ptr: i32, root_len: i32, out_ptr: i32, out_cap: i32) -> i32;
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_read_file(
    _path_ptr: i32,
    _path_len: i32,
    _out_ptr: i32,
    _out_cap: i32,
) -> i32 {
    -1
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_list_files(
    _root_ptr: i32,
    _root_len: i32,
    _out_ptr: i32,
    _out_cap: i32,
) -> i32 {
    -1
}

const FILE_BUF_SIZE: usize = 4 * 1024 * 1024;
const LIST_BUF_SIZE: usize = 8 * 1024 * 1024;

static mut FILE_BUF: [u8; FILE_BUF_SIZE] = [0u8; FILE_BUF_SIZE];
static mut LIST_BUF: [u8; LIST_BUF_SIZE] = [0u8; LIST_BUF_SIZE];

#[derive(Debug, Default, Clone)]
struct Package {
    name: String,
    version: Option<String>,
    target: Option<String>,
    dependencies: Vec<String>,
    dev_dependencies: Vec<String>,
}

#[basalt_plugin]
fn build_project_model(root: &str) -> Vec<u8> {
    let Some(manifest) = resolve_manifest(root) else {
        return Vec::new();
    };
    let contents = read_host_file(&manifest).unwrap_or_default();
    let Some(package) = parse_gleam_toml(&contents) else {
        return Vec::new();
    };

    let package_root = parent_dir(&manifest);
    let main_path = join_path(&package_root, &format!("src/{}.gleam", package.name));
    let main_src = read_host_file(&main_path);
    let package_prefix = relative_to(root, &package_root);

    emit_model(
        root,
        &manifest,
        &package,
        main_src.as_deref(),
        &package_prefix,
    )
    .into_bytes()
}

/// `root` may be the workspace root, a package directory, or the manifest
/// itself. Returns the absolute manifest path.
fn resolve_manifest(root: &str) -> Option<String> {
    let normalized = root.replace('\\', "/");
    if normalized.ends_with("gleam.toml") || normalized.ends_with("manifest.toml") {
        return Some(root.to_string());
    }

    let direct = join_path(root, "gleam.toml");
    if read_host_file(&direct).is_some() {
        return Some(direct);
    }

    let mut best: Option<(usize, String)> = None;
    for rel in list_host_files(root) {
        let rel = normalize_rel_path(&rel);
        if file_name(&rel) != "gleam.toml" {
            continue;
        }
        let depth = rel.matches('/').count();
        if best.as_ref().map(|(d, _)| depth < *d).unwrap_or(true) {
            best = Some((depth, join_path(root, &rel)));
        }
    }
    best.map(|(_, path)| path)
}

// ── gleam.toml parsing ──────────────────────────────────────────────────────

fn parse_gleam_toml(contents: &str) -> Option<Package> {
    let mut package = Package::default();
    let mut section = String::new();

    for raw in contents.lines() {
        let line = strip_toml_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());
        match section.as_str() {
            "" => match key {
                "name" => package.name = value,
                "version" => package.version = Some(value),
                "target" => package.target = Some(value),
                _ => {}
            },
            "dependencies" => {
                if !key.is_empty() {
                    package.dependencies.push(key.to_string());
                }
            }
            "dev-dependencies" => {
                if !key.is_empty() {
                    package.dev_dependencies.push(key.to_string());
                }
            }
            "gleam" => {
                if key == "target" {
                    package.target = Some(value);
                }
            }
            _ if section.starts_with("dependencies.") => {}
            _ => {}
        }
    }

    if package.name.is_empty() {
        None
    } else {
        Some(package)
    }
}

/// Drop a trailing `#` comment, taking care not to cut inside a quoted value.
fn strip_toml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

// ── Model emission ──────────────────────────────────────────────────────────

fn emit_model(
    root: &str,
    manifest: &str,
    package: &Package,
    main_src: Option<&str>,
    package_prefix: &str,
) -> String {
    let is_app = main_src
        .map(|src| source_declares_main(src))
        .unwrap_or(false);
    let kind = if is_app { "app" } else { "library" };
    let subtype = if is_app {
        "gleam-application"
    } else {
        "gleam-package"
    };
    let mut capabilities = vec![(if is_app {
        "is-executable"
    } else {
        "is-library"
    })
    .to_string()];
    if package
        .target
        .as_deref()
        .map(|t| t.eq_ignore_ascii_case("javascript"))
        .unwrap_or(false)
    {
        capabilities.push("compiles-to-javascript".to_string());
    }
    let project_id = sanitize_id(&package.name);

    let source_roots = vec![with_prefix(package_prefix, "src/")];
    let test_roots = vec![with_prefix(package_prefix, "test/")];
    let build_roots = vec![with_prefix(package_prefix, "build/")];

    let target = format!(
        concat!(
            "{{",
            "\"id\":\"{}\",",
            "\"name\":\"{}\",",
            "\"kind\":\"{}\",",
            "\"subtype\":\"{}\",",
            "\"capabilities\":[{}],",
            "\"language\":\"gleam\",",
            "\"source_roots\":[{}],",
            "\"test_roots\":[{}],",
            "\"build_roots\":[{}]",
            "}}"
        ),
        escape_json(&project_id),
        escape_json(&package.name),
        escape_json(kind),
        escape_json(subtype),
        quote_list(&capabilities),
        quote_list(&source_roots),
        quote_list(&test_roots),
        quote_list(&build_roots),
    );

    let project = format!(
        concat!(
            "{{",
            "\"id\":\"{}\",",
            "\"name\":\"{}\",",
            "\"kind\":\"{}\",",
            "\"subtype\":\"{}\",",
            "\"capabilities\":[{}],",
            "\"manifest_path\":\"{}\",",
            "\"source_roots\":[{}],",
            "\"test_roots\":[{}],",
            "\"build_roots\":[{}],",
            "\"targets\":[{}]",
            "}}"
        ),
        escape_json(&project_id),
        escape_json(&package.name),
        escape_json(kind),
        escape_json(subtype),
        quote_list(&capabilities),
        escape_json(manifest),
        quote_list(&source_roots),
        quote_list(&test_roots),
        quote_list(&build_roots),
        target,
    );

    let mut owners = vec![
        owner_json(
            &with_prefix(package_prefix, "gleam.toml"),
            "exact",
            &package.name,
            &project_id,
            "manifest",
        ),
        owner_json(
            &with_prefix(package_prefix, "manifest.toml"),
            "exact",
            &package.name,
            &project_id,
            "manifest",
        ),
        owner_json(
            &with_prefix(package_prefix, "src/"),
            "prefix",
            &package.name,
            &project_id,
            "source",
        ),
        owner_json(
            &with_prefix(package_prefix, "test/"),
            "prefix",
            &package.name,
            &project_id,
            "test",
        ),
        owner_json(
            &with_prefix(package_prefix, "build/"),
            "prefix",
            &package.name,
            &project_id,
            "build",
        ),
    ];
    if let Some(version) = &package.version {
        if !version.is_empty() {
            owners.push(owner_json(
                &format!(".basalt/gleam@{}", version),
                "exact",
                &package.name,
                &project_id,
                "version",
            ));
        }
    }

    format!(
        concat!(
            "{{",
            "\"schema_version\":1,",
            "\"ecosystem\":\"gleam\",",
            "\"root\":\"{}\",",
            "\"display_name\":\"{}\",",
            "\"primary_manifest_path\":\"{}\",",
            "\"projects\":[{}],",
            "\"owners\":[{}]",
            "}}"
        ),
        escape_json(root),
        escape_json(&package.name),
        escape_json(manifest),
        project,
        owners.join(","),
    )
}

/// True when the package entry module declares `pub fn main(...)`.
fn source_declares_main(src: &str) -> bool {
    let mut search = src;
    while let Some(idx) = search.find("pub fn main") {
        let after = &search[idx + "pub fn main".len()..];
        let trimmed = after.trim_start();
        if trimmed.starts_with('(') {
            return true;
        }
        search = &search[idx + 1..];
    }
    false
}

fn owner_json(path: &str, match_kind: &str, label: &str, project_id: &str, kind: &str) -> String {
    format!(
        "{{\"path\":\"{}\",\"match_kind\":\"{}\",\"label\":\"{}\",\"project_id\":\"{}\",\"target_id\":\"{}\",\"kind\":\"{}\"}}",
        escape_json(path),
        escape_json(match_kind),
        escape_json(label),
        escape_json(project_id),
        escape_json(project_id),
        escape_json(kind),
    )
}

// ── Host I/O ────────────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
fn read_host_file(path: &str) -> Option<String> {
    let buf: &mut [u8; FILE_BUF_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(FILE_BUF) };
    let n = unsafe {
        basalt_read_file(
            path.as_ptr() as i32,
            path.len() as i32,
            buf.as_mut_ptr() as i32,
            FILE_BUF_SIZE as i32,
        )
    };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

#[cfg(not(target_arch = "wasm32"))]
fn read_host_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(target_arch = "wasm32")]
fn list_host_files(root: &str) -> Vec<String> {
    let buf: &mut [u8; LIST_BUF_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(LIST_BUF) };
    let n = unsafe {
        basalt_list_files(
            root.as_ptr() as i32,
            root.len() as i32,
            buf.as_mut_ptr() as i32,
            LIST_BUF_SIZE as i32,
        )
    };
    if n <= 0 {
        return Vec::new();
    }
    String::from_utf8_lossy(&buf[..n as usize])
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
fn list_host_files(root: &str) -> Vec<String> {
    fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>, depth: usize) {
        if depth > 8 || out.len() > 20_000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(
                name.as_str(),
                ".git" | ".basalt" | "build" | ".gradle" | "node_modules" | "target"
            ) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out, depth + 1);
            } else if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    let base = std::path::Path::new(root);
    let mut out = Vec::new();
    walk(base, base, &mut out, 0);
    out
}

// ── Review actions ──────────────────────────────────────────────────────────

#[basalt_plugin]
fn review_actions(workspace_root: &str, _session_workspace: &str) -> Vec<ReviewActionDescriptor> {
    if resolve_manifest(workspace_root).is_none() {
        return Vec::new();
    }
    vec![
        ReviewActionDescriptor {
            id: "gleam-build".into(),
            title: "Build Gleam Package".into(),
            kind: ReviewActionKind::Build,
            ecosystem: "gleam".into(),
            command_preview: "gleam build".into(),
            mutates_workspace: false,
            priority: 100,
        },
        ReviewActionDescriptor {
            id: "gleam-test".into(),
            title: "Run Gleam Tests".into(),
            kind: ReviewActionKind::Test,
            ecosystem: "gleam".into(),
            command_preview: "gleam test".into(),
            mutates_workspace: false,
            priority: 100,
        },
        ReviewActionDescriptor {
            id: "gleam-format-check".into(),
            title: "Check Gleam Formatting".into(),
            kind: ReviewActionKind::FormatCheck,
            ecosystem: "gleam".into(),
            command_preview: "gleam format --check src test".into(),
            mutates_workspace: false,
            priority: 80,
        },
    ]
}

#[basalt_plugin]
fn review_action_plan(
    action_id: &str,
    _workspace_root: &str,
    _session_workspace: &str,
) -> Option<ReviewActionExecutionPlan> {
    match action_id {
        "gleam-build" => Some(ReviewActionExecutionPlan {
            executable: "gleam".into(),
            args: vec!["build".into()],
            env: Vec::new(),
            cwd_mode: ReviewActionCwdMode::SessionWorkspace,
            output_category: "build".into(),
        }),
        "gleam-test" => Some(ReviewActionExecutionPlan {
            executable: "gleam".into(),
            args: vec!["test".into()],
            env: Vec::new(),
            cwd_mode: ReviewActionCwdMode::SessionWorkspace,
            output_category: "test".into(),
        }),
        "gleam-format-check" => Some(ReviewActionExecutionPlan {
            executable: "gleam".into(),
            args: vec![
                "format".into(),
                "--check".into(),
                "src".into(),
                "test".into(),
            ],
            env: Vec::new(),
            cwd_mode: ReviewActionCwdMode::SessionWorkspace,
            output_category: "format".into(),
        }),
        _ => None,
    }
}

#[basalt_plugin]
fn review_action_parse_line(
    action_id: &str,
    line: &[u8],
    state: &[u8],
) -> (Vec<u8>, Vec<AgentEvent>) {
    let (mut counter, mut pending_severity, mut pending_message) = read_state(state);
    let Ok(line_str) = core::str::from_utf8(line) else {
        return (
            encode_state(counter, pending_severity, &pending_message),
            Vec::new(),
        );
    };
    let line_str = strip_ansi(line_str);
    let trimmed = line_str.trim();
    if trimmed.is_empty() {
        return (
            encode_state(counter, pending_severity, &pending_message),
            Vec::new(),
        );
    }

    let mut events = Vec::new();

    if action_id == "gleam-build" || action_id == "gleam-test" {
        if trimmed.starts_with("error: ") || trimmed.starts_with("warning: ") {
            pending_severity = if trimmed.starts_with("error: ") { 1 } else { 2 };
            pending_message = trimmed.to_string();
            return (
                encode_state(counter, pending_severity, &pending_message),
                events,
            );
        }

        if trimmed.starts_with('┌') {
            if let Some((_, location)) = trimmed.split_once('─') {
                let location = location.trim();
                if let Some((path, line_no, col)) = split_location(location) {
                    let message = if pending_message.is_empty() {
                        location.to_string()
                    } else {
                        pending_message.clone()
                    };
                    events.push(AgentEvent::NewEntry {
                        vendor_id: format!("gleam-{counter}"),
                        tool: "gleam".into(),
                        category: if pending_severity == 2 {
                            "diagnostic".into()
                        } else {
                            "build".into()
                        },
                        raw_cmd: diagnostic_json(
                            &path,
                            line_no,
                            col,
                            if pending_severity == 2 {
                                "warning"
                            } else {
                                "error"
                            },
                            &message,
                        ),
                        file_paths: Vec::new(),
                    });
                    counter += 1;
                    pending_severity = 0;
                    pending_message.clear();
                }
            }
        }
    }

    if action_id == "gleam-test" {
        if let Some((passed, failures)) = parse_gleeunit_summary(trimmed) {
            events.push(AgentEvent::CloseEntry {
                vendor_id: format!("gleam-test-{counter}"),
                exit_code: if failures > 0 { 1 } else { 0 },
                output_lines: vec![format!("{passed} passed, {failures} failures")],
            });
            counter += 1;
        } else if let Some(rest) = trimmed.strip_prefix("Running ") {
            events.push(AgentEvent::NewEntry {
                vendor_id: format!("gleam-{counter}"),
                tool: "gleam test".into(),
                category: "test".into(),
                raw_cmd: format!("Running {rest}"),
                file_paths: Vec::new(),
            });
            counter += 1;
        }
    }

    if action_id == "gleam-format-check" && trimmed.starts_with("- ") {
        events.push(AgentEvent::NewEntry {
            vendor_id: format!("gleam-{counter}"),
            tool: "gleam format".into(),
            category: "format".into(),
            raw_cmd: trimmed.to_string(),
            file_paths: vec![trimmed[2..].trim().replace('\\', "/")],
        });
        counter += 1;
    }

    (
        encode_state(counter, pending_severity, &pending_message),
        events,
    )
}

/// `DiagnosticItem` JSON understood by `basalt_core::build_runner`.
fn diagnostic_json(file: &str, line: u32, col: u32, severity: &str, message: &str) -> String {
    format!(
        "{{\"file\":\"{}\",\"line\":{},\"col\":{},\"severity\":\"{}\",\"message\":\"{}\"}}",
        escape_json(file),
        line,
        col,
        severity,
        escape_json(message),
    )
}

/// Parser state: counter + pending severity (`0` none, `1` error, `2` warning)
/// + pending diagnostic message (waiting for its `┌─ path:line:col` line).
fn read_state(state: &[u8]) -> (u32, u8, String) {
    if state.len() < 7 {
        return (0, 0, String::new());
    }
    let counter = u32::from_le_bytes([state[0], state[1], state[2], state[3]]);
    let severity = state[4];
    let len = u16::from_le_bytes([state[5], state[6]]) as usize;
    let end = (7 + len).min(state.len());
    let message = String::from_utf8_lossy(&state[7..end]).into_owned();
    (counter, severity, message)
}

fn encode_state(counter: u32, severity: u8, message: &str) -> Vec<u8> {
    let bytes = message.as_bytes();
    let len = bytes.len().min(u16::MAX as usize) as u16;
    let mut out = Vec::with_capacity(7 + len as usize);
    out.extend_from_slice(&counter.to_le_bytes());
    out.push(severity);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
    out
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// Split `path:line:col` while tolerating Windows drive letters.
fn split_location(location: &str) -> Option<(String, u32, u32)> {
    let mut parts = location.rsplitn(3, ':');
    let col: u32 = parts.next()?.trim().parse().ok()?;
    let line: u32 = parts.next()?.trim().parse().ok()?;
    let path = parts.next()?.trim();
    if path.is_empty() {
        None
    } else {
        Some((path.to_string(), line, col))
    }
}

/// Parse gleeunit's trailing `N passed, M failures` / `N passed, no failures`.
fn parse_gleeunit_summary(line: &str) -> Option<(u32, u32)> {
    let (passed_part, rest) = line.split_once(" passed, ")?;
    let passed: u32 = passed_part
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let failures = if rest.starts_with("no failures") {
        0
    } else {
        rest.split_whitespace().next()?.parse().ok()?
    };
    Some((passed, failures))
}

// ── Small path/JSON helpers ─────────────────────────────────────────────────

fn normalize_rel_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

/// Join a workspace-relative prefix with a relative subpath.
fn with_prefix(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        path.to_string()
    } else {
        format!("{prefix}/{}", path.trim_start_matches('/'))
    }
}

/// Workspace-relative path of `path` under `base` ("" when equal).
fn relative_to(base: &str, path: &str) -> String {
    let base = base.replace('\\', "/");
    let base = base.trim_end_matches('/');
    let path = path.replace('\\', "/");
    if path == base {
        return String::new();
    }
    let needle = format!("{base}/");
    path.strip_prefix(&needle).unwrap_or(&path).to_string()
}

fn join_path(root: &str, suffix: &str) -> String {
    if root.is_empty() {
        return suffix.to_string();
    }
    if root.ends_with('/') || root.ends_with('\\') {
        format!("{root}{suffix}")
    } else {
        format!("{root}/{suffix}")
    }
}

fn parent_dir(path: &str) -> String {
    path.rsplit_once(['/', '\\'])
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn sanitize_id(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "gleam-package".to_string();
    }
    trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn quote_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("\"{}\"", escape_json(item)))
        .collect::<Vec<_>>()
        .join(",")
}

fn escape_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                let n = c as u32;
                out.push_str(&format!("\\u{n:04X}"));
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GLEAM_TOML: &str = r#"
name = "add_helper"
version = "1.0.0"
target = "erlang"

[dependencies]
gleam_stdlib = ">= 1.0.0 and < 2.0.0"
lustre = ">= 5.7.1 and < 6.0.0"
mist = ">= 6.0.3 and < 7.0.0"

[dev-dependencies]
gleeunit = ">= 1.0.0 and < 2.0.0"
"#;

    fn temp_workspace(files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gleam-model-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, contents) in files {
            std::fs::write(dir.join(name), contents).unwrap();
        }
        dir
    }

    #[test]
    fn parses_package_manifest() {
        let pkg = parse_gleam_toml(GLEAM_TOML).expect("package");
        assert_eq!(pkg.name, "add_helper");
        assert_eq!(pkg.version.as_deref(), Some("1.0.0"));
        assert_eq!(pkg.target.as_deref(), Some("erlang"));
        assert_eq!(pkg.dependencies.len(), 3);
        assert!(pkg.dependencies.contains(&"lustre".to_string()));
        assert_eq!(pkg.dev_dependencies, vec!["gleeunit".to_string()]);
    }

    #[test]
    fn comments_and_single_quotes_are_handled() {
        let pkg =
            parse_gleam_toml("name = 'demo' # package\nversion = \"0.1.0\"\n").expect("package");
        assert_eq!(pkg.name, "demo");
        assert_eq!(pkg.version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn detects_main_entrypoint() {
        assert!(source_declares_main("pub fn main() {\n  Nil\n}\n"));
        assert!(!source_declares_main("pub fn main_helper() { Nil }"));
        assert!(!source_declares_main("fn main() { Nil }"));
    }

    #[test]
    fn model_marks_app_and_test_roots() {
        let pkg = parse_gleam_toml(GLEAM_TOML).unwrap();
        let json = emit_model(
            "/ws",
            "/ws/gleam.toml",
            &pkg,
            Some("pub fn main() { Nil }"),
            "",
        );
        assert!(json.contains("\"schema_version\":1"));
        assert!(json.contains("\"ecosystem\":\"gleam\""));
        assert!(json.contains("\"kind\":\"app\""));
        assert!(json.contains("\"subtype\":\"gleam-application\""));
        assert!(json.contains("\"language\":\"gleam\""));
        assert!(json.contains("\"path\":\"test/\",\"match_kind\":\"prefix\""));
        assert!(json.contains("\"path\":\"gleam.toml\",\"match_kind\":\"exact\""));
    }

    #[test]
    fn model_defaults_to_library() {
        let pkg = parse_gleam_toml(GLEAM_TOML).unwrap();
        let json = emit_model("/ws", "/ws/gleam.toml", &pkg, None, "");
        assert!(json.contains("\"kind\":\"library\""));
        assert!(json.contains("\"is-library\""));
    }

    #[test]
    fn review_actions_include_build_test_and_format() {
        let ws = temp_workspace(&[("gleam.toml", "name = \"demo\"\nversion = \"1.0.0\"\n")]);
        let actions = review_actions(ws.to_str().unwrap(), "/tmp/.shadow");
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["gleam-build", "gleam-test", "gleam-format-check"]);
        assert_eq!(actions[2].kind, ReviewActionKind::FormatCheck);
        assert!(review_actions("/definitely/not/a/gleam/project", "/tmp").is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn review_plans_use_gleam_commands() {
        assert_eq!(
            review_action_plan("gleam-test", "/ws", "/ws").unwrap().args,
            vec!["test".to_string()]
        );
        assert_eq!(
            review_action_plan("gleam-format-check", "/ws", "/ws")
                .unwrap()
                .args,
            vec!["format", "--check", "src", "test"]
        );
    }

    #[test]
    fn build_errors_emit_diagnostic_events() {
        // The header stores the pending message; the location line emits one
        // JSON DiagnosticItem with the exact file/line/col.
        let (state, header_events) =
            review_action_parse_line("gleam-build", b"error: Unknown variable", &[]);
        assert!(header_events.is_empty());
        assert_eq!(read_state(&state).1, 1);

        let (_, location_events) = review_action_parse_line(
            "gleam-build",
            b"  \xe2\x94\x8c\xe2\x94\x80 C:\\ws\\src\\main.gleam:2:7",
            &state,
        );
        assert!(matches!(
            &location_events[0],
            AgentEvent::NewEntry { category, raw_cmd, file_paths, .. }
                if category == "build"
                    && raw_cmd.contains("\"line\":2")
                    && raw_cmd.contains("main.gleam")
                    && raw_cmd.contains("Unknown variable")
                    && file_paths.is_empty()
        ));
    }

    #[test]
    fn gleeunit_summary_sets_exit_code() {
        let (_, fail_events) = review_action_parse_line("gleam-test", b"0 passed, 1 failures", &[]);
        assert!(matches!(
            &fail_events[0],
            AgentEvent::CloseEntry { exit_code: 1, .. }
        ));

        let (_, pass_events) =
            review_action_parse_line("gleam-test", b"1 passed, no failures", &[]);
        assert!(matches!(
            &pass_events[0],
            AgentEvent::CloseEntry { exit_code: 0, .. }
        ));
    }

    #[test]
    fn ansi_sequences_are_stripped() {
        let raw = b"\x1b[32m1 passed, no failures\x1b[39m";
        let (_, events) = review_action_parse_line("gleam-test", raw, &[]);
        assert!(matches!(
            &events[0],
            AgentEvent::CloseEntry { exit_code: 0, .. }
        ));
    }

    /// Opt-in end-to-end model build against the real add-helper checkout.
    #[test]
    #[ignore = "requires ../../add-helper"]
    fn models_real_add_helper_package() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../add-helper");
        let bytes = build_project_model(root.to_str().unwrap());
        let json = String::from_utf8(bytes).expect("utf8 model");
        assert!(json.contains("\"ecosystem\":\"gleam\""), "{json}");
        assert!(json.contains("\"name\":\"add_helper\""), "{json}");
        assert!(json.contains("\"kind\":\"app\""), "{json}");
        assert!(
            json.contains("\"path\":\"src/\",\"match_kind\":\"prefix\""),
            "{json}"
        );
        assert!(
            json.contains("\"path\":\"test/\",\"match_kind\":\"prefix\""),
            "{json}"
        );
    }
}
