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
    let mut parser_state = read_state(state);
    let Ok(line_str) = core::str::from_utf8(line) else {
        return (encode_state(&parser_state), Vec::new());
    };
    let line_str = strip_ansi(line_str);
    let trimmed = line_str.trim();

    let mut events = Vec::new();

    if trimmed.is_empty() {
        parser_state.flush_diagnostic(&mut events);
        return (encode_state(&parser_state), events);
    }

    if action_id == "gleam-build" || action_id == "gleam-test" {
        if trimmed.starts_with("error: ") || trimmed.starts_with("warning: ") {
            parser_state.flush_diagnostic(&mut events);
            parser_state.severity = if trimmed.starts_with("error: ") { 1 } else { 2 };
            parser_state.title = trimmed.to_string();
            parser_state.has_location = false;
            parser_state.path.clear();
            parser_state.line = 0;
            parser_state.col = 0;
            parser_state.explanation.clear();
            return (encode_state(&parser_state), events);
        }

        if trimmed.starts_with('┌') {
            if let Some((_, location)) = trimmed.split_once('─') {
                let location = location.trim();
                if let Some((path, line_no, col)) = split_location(location) {
                    if parser_state.has_location {
                        parser_state.flush_diagnostic(&mut events);
                    }
                    if parser_state.severity == 0 {
                        parser_state.severity = 1;
                        parser_state.title = location.to_string();
                    }
                    parser_state.has_location = true;
                    parser_state.path = path;
                    parser_state.line = line_no;
                    parser_state.col = col;
                    parser_state.explanation.clear();
                    return (encode_state(&parser_state), events);
                }
            }
        }
    }

    if action_id == "gleam-test" {
        if let Some((passed, failures)) = parse_gleeunit_summary(trimmed) {
            parser_state.flush_diagnostic(&mut events);
            events.push(AgentEvent::CloseEntry {
                vendor_id: format!("gleam-test-{}", parser_state.counter),
                exit_code: if failures > 0 { 1 } else { 0 },
                output_lines: vec![format!("{passed} passed, {failures} failures")],
            });
            parser_state.counter += 1;
            return (encode_state(&parser_state), events);
        } else if let Some(rest) = trimmed.strip_prefix("Running ") {
            parser_state.flush_diagnostic(&mut events);
            events.push(AgentEvent::NewEntry {
                vendor_id: format!("gleam-{}", parser_state.counter),
                tool: "gleam test".into(),
                category: "test".into(),
                raw_cmd: format!("Running {rest}"),
                file_paths: Vec::new(),
            });
            parser_state.counter += 1;
            return (encode_state(&parser_state), events);
        }
    }

    if action_id == "gleam-format-check" && trimmed.starts_with("- ") {
        parser_state.flush_diagnostic(&mut events);
        events.push(AgentEvent::NewEntry {
            vendor_id: format!("gleam-{}", parser_state.counter),
            tool: "gleam format".into(),
            category: "format".into(),
            raw_cmd: trimmed.to_string(),
            file_paths: vec![trimmed[2..].trim().replace('\\', "/")],
        });
        parser_state.counter += 1;
        return (encode_state(&parser_state), events);
    }

    if (action_id == "gleam-build" || action_id == "gleam-test") && parser_state.has_location {
        if let Some(annotation) = extract_frame_annotation(trimmed) {
            if parser_state.explanation.is_empty() {
                parser_state.explanation = annotation;
            } else {
                parser_state.explanation.push('\n');
                parser_state.explanation.push_str(&annotation);
            }
            return (encode_state(&parser_state), events);
        }

        if is_frame_line(trimmed) {
            return (encode_state(&parser_state), events);
        }

        if parser_state.explanation.is_empty() {
            parser_state.explanation = trimmed.to_string();
        } else {
            parser_state.explanation.push('\n');
            parser_state.explanation.push_str(trimmed);
        }
        return (encode_state(&parser_state), events);
    }

    (encode_state(&parser_state), events)
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

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct ParserState {
    counter: u32,
    severity: u8,
    has_location: bool,
    line: u32,
    col: u32,
    title: String,
    path: String,
    explanation: String,
}

impl ParserState {
    fn flush_diagnostic(&mut self, events: &mut Vec<AgentEvent>) {
        if self.has_location && self.severity > 0 {
            let message = if self.explanation.is_empty() {
                self.title.clone()
            } else if self.title.is_empty() {
                self.explanation.clone()
            } else {
                format!("{}\n{}", self.title, self.explanation)
            };

            events.push(AgentEvent::NewEntry {
                vendor_id: format!("gleam-{}", self.counter),
                tool: "gleam".into(),
                category: if self.severity == 2 {
                    "diagnostic".into()
                } else {
                    "build".into()
                },
                raw_cmd: diagnostic_json(
                    &self.path,
                    self.line,
                    self.col,
                    if self.severity == 2 {
                        "warning"
                    } else {
                        "error"
                    },
                    &message,
                ),
                file_paths: Vec::new(),
            });
            self.counter += 1;
            self.severity = 0;
            self.has_location = false;
            self.title.clear();
            self.path.clear();
            self.line = 0;
            self.col = 0;
            self.explanation.clear();
        }
    }
}

fn read_state(state: &[u8]) -> ParserState {
    if state.len() < 16 {
        return ParserState::default();
    }
    let counter = u32::from_le_bytes([state[0], state[1], state[2], state[3]]);
    let severity = state[4];
    let has_location = state[5] != 0;
    let line = u32::from_le_bytes([state[6], state[7], state[8], state[9]]);
    let col = u32::from_le_bytes([state[10], state[11], state[12], state[13]]);
    let title_len = u16::from_le_bytes([state[14], state[15]]) as usize;

    let mut offset = 16;
    let title_end = (offset + title_len).min(state.len());
    let title = String::from_utf8_lossy(&state[offset..title_end]).into_owned();
    offset = title_end;

    if offset + 2 > state.len() {
        return ParserState {
            counter,
            severity,
            has_location,
            line,
            col,
            title,
            path: String::new(),
            explanation: String::new(),
        };
    }

    let path_len = u16::from_le_bytes([state[offset], state[offset + 1]]) as usize;
    offset += 2;
    let path_end = (offset + path_len).min(state.len());
    let path = String::from_utf8_lossy(&state[offset..path_end]).into_owned();
    offset = path_end;

    if offset + 4 > state.len() {
        return ParserState {
            counter,
            severity,
            has_location,
            line,
            col,
            title,
            path,
            explanation: String::new(),
        };
    }

    let expl_len = u32::from_le_bytes([
        state[offset],
        state[offset + 1],
        state[offset + 2],
        state[offset + 3],
    ]) as usize;
    offset += 4;
    let expl_end = (offset + expl_len).min(state.len());
    let explanation = String::from_utf8_lossy(&state[offset..expl_end]).into_owned();

    ParserState {
        counter,
        severity,
        has_location,
        line,
        col,
        title,
        path,
        explanation,
    }
}

fn encode_state(state: &ParserState) -> Vec<u8> {
    let title_bytes = state.title.as_bytes();
    let title_len = title_bytes.len().min(u16::MAX as usize) as u16;

    let path_bytes = state.path.as_bytes();
    let path_len = path_bytes.len().min(u16::MAX as usize) as u16;

    let expl_bytes = state.explanation.as_bytes();
    let expl_len = expl_bytes.len().min(u32::MAX as usize) as u32;

    let mut out = Vec::with_capacity(
        16 + 2 + title_len as usize + 2 + path_len as usize + 4 + expl_len as usize,
    );
    out.extend_from_slice(&state.counter.to_le_bytes());
    out.push(state.severity);
    out.push(if state.has_location { 1 } else { 0 });
    out.extend_from_slice(&state.line.to_le_bytes());
    out.extend_from_slice(&state.col.to_le_bytes());
    out.extend_from_slice(&title_len.to_le_bytes());
    out.extend_from_slice(&title_bytes[..title_len as usize]);
    out.extend_from_slice(&path_len.to_le_bytes());
    out.extend_from_slice(&path_bytes[..path_len as usize]);
    out.extend_from_slice(&expl_len.to_le_bytes());
    out.extend_from_slice(&expl_bytes[..expl_len as usize]);
    out
}

fn is_frame_line(trimmed: &str) -> bool {
    if trimmed.contains('│') {
        return true;
    }
    if trimmed == "|" || trimmed.starts_with('|') {
        return true;
    }
    if let Some((prefix, _)) = trimmed.split_once('|') {
        let prefix = prefix.trim();
        if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    if trimmed.chars().all(|c| c == '^' || c == ' ' || c == '-') && trimmed.contains('^') {
        return true;
    }
    false
}

fn extract_frame_annotation(trimmed: &str) -> Option<String> {
    let after_bar = if let Some((_, after)) = trimmed.split_once('│') {
        after
    } else if let Some((prefix, after)) = trimmed.split_once('|') {
        if prefix.trim().is_empty() || prefix.trim().chars().all(|c| c.is_ascii_digit()) {
            after
        } else {
            return None;
        }
    } else {
        return None;
    };

    if after_bar.contains('^') {
        let text = after_bar.trim_start_matches(|c| c == ' ' || c == '^').trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }
    None
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
        // The header stores pending severity and title; the location line sets the location;
        // an empty separator line flushes the complete diagnostic entry.
        let (state, header_events) =
            review_action_parse_line("gleam-build", b"error: Unknown variable", &[]);
        assert!(header_events.is_empty());
        assert_eq!(read_state(&state).severity, 1);

        let (state, location_events) = review_action_parse_line(
            "gleam-build",
            b"  \xe2\x94\x8c\xe2\x94\x80 C:\\ws\\src\\main.gleam:2:7",
            &state,
        );
        assert!(location_events.is_empty());

        let (_, flush_events) = review_action_parse_line("gleam-build", b"", &state);
        assert!(matches!(
            &flush_events[0],
            AgentEvent::NewEntry { category, raw_cmd, file_paths, .. }
                if category == "build"
                    && raw_cmd.contains("\"line\":2")
                    && raw_cmd.contains("main.gleam")
                    && raw_cmd.contains("Unknown variable")
                    && file_paths.is_empty()
        ));
    }

    #[test]
    fn multiline_error_preserves_arity_explanation() {
        let lines = [
            "error: Incorrect arity",
            "  ┌─ test/add_helper_test.gleam:2213:5",
            "  │",
            "  The constructor expects 8 arguments, but was given 7.",
            "  Missing argument: audio_path",
            "",
        ];
        let mut state = Vec::new();
        let mut events = Vec::new();
        for line in lines {
            let (s, ev) = review_action_parse_line("gleam-build", line.as_bytes(), &state);
            state = s;
            events.extend(ev);
        }

        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::NewEntry { category, raw_cmd, file_paths, .. } => {
                assert_eq!(category, "build");
                assert!(file_paths.is_empty());
                assert!(raw_cmd.contains("\"file\":\"test/add_helper_test.gleam\""));
                assert!(raw_cmd.contains("\"line\":2213"));
                assert!(raw_cmd.contains("\"col\":5"));
                assert!(raw_cmd.contains("\"severity\":\"error\""));
                assert!(raw_cmd.contains("error: Incorrect arity"));
                assert!(raw_cmd.contains("The constructor expects 8 arguments, but was given 7."));
                assert!(raw_cmd.contains("Missing argument: audio_path"));
            }
            _ => panic!("Expected NewEntry event"),
        }
    }

    #[test]
    fn multiline_error_preserves_unknown_labels_explanation() {
        let lines = [
            "error: Unknown labels",
            "  ┌─ test/add_helper_test.gleam:2191:7",
            "  │",
            "  The label `audio_url` is not recognized. Did you mean `audio_path`?",
            "",
        ];
        let mut state = Vec::new();
        let mut events = Vec::new();
        for line in lines {
            let (s, ev) = review_action_parse_line("gleam-build", line.as_bytes(), &state);
            state = s;
            events.extend(ev);
        }

        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::NewEntry { category, raw_cmd, .. } => {
                assert_eq!(category, "build");
                assert!(raw_cmd.contains("\"file\":\"test/add_helper_test.gleam\""));
                assert!(raw_cmd.contains("\"line\":2191"));
                assert!(raw_cmd.contains("\"col\":7"));
                assert!(raw_cmd.contains("\"severity\":\"error\""));
                assert!(raw_cmd.contains("error: Unknown labels"));
                assert!(raw_cmd.contains("The label `audio_url` is not recognized. Did you mean `audio_path`?"));
                assert!(raw_cmd.contains("Did you mean `audio_path`?"));
            }
            _ => panic!("Expected NewEntry event"),
        }
    }

    #[test]
    fn consecutive_errors_flush_on_next_header() {
        let lines = [
            "error: Incorrect arity",
            "  ┌─ test/add_helper_test.gleam:2213:5",
            "  │",
            "  The constructor expects 8 arguments, but was given 7.",
            "  Missing argument: audio_path",
            "error: Unknown labels",
            "  ┌─ test/add_helper_test.gleam:2191:7",
            "  │",
            "  The label `audio_url` is not recognized. Did you mean `audio_path`?",
            "",
        ];
        let mut state = Vec::new();
        let mut events = Vec::new();
        for line in lines {
            let (s, ev) = review_action_parse_line("gleam-build", line.as_bytes(), &state);
            state = s;
            events.extend(ev);
        }

        assert_eq!(events.len(), 2);
        match &events[0] {
            AgentEvent::NewEntry { raw_cmd, .. } => {
                assert!(raw_cmd.contains("\"line\":2213"));
                assert!(raw_cmd.contains("Missing argument: audio_path"));
            }
            _ => panic!("Expected NewEntry for first error"),
        }
        match &events[1] {
            AgentEvent::NewEntry { raw_cmd, .. } => {
                assert!(raw_cmd.contains("\"line\":2191"));
                assert!(raw_cmd.contains("Did you mean `audio_path`?"));
            }
            _ => panic!("Expected NewEntry for second error"),
        }
    }

    #[test]
    fn multiline_error_flushed_by_gleeunit_summary() {
        let lines = [
            "error: Incorrect arity",
            "  ┌─ test/add_helper_test.gleam:2213:5",
            "  │",
            "  The constructor expects 8 arguments, but was given 7.",
            "  Missing argument: audio_path",
            "0 passed, 1 failures",
        ];
        let mut state = Vec::new();
        let mut events = Vec::new();
        for line in lines {
            let (s, ev) = review_action_parse_line("gleam-test", line.as_bytes(), &state);
            state = s;
            events.extend(ev);
        }

        assert_eq!(events.len(), 2);
        match &events[0] {
            AgentEvent::NewEntry { raw_cmd, .. } => {
                assert!(raw_cmd.contains("\"line\":2213"));
                assert!(raw_cmd.contains("Missing argument: audio_path"));
            }
            _ => panic!("Expected NewEntry for error"),
        }
        match &events[1] {
            AgentEvent::CloseEntry { exit_code, output_lines, .. } => {
                assert_eq!(*exit_code, 1);
                assert_eq!(output_lines[0], "0 passed, 1 failures");
            }
            _ => panic!("Expected CloseEntry for gleeunit summary"),
        }
    }

    #[test]
    fn multiline_error_with_code_snippet() {
        let lines = [
            "error: Type mismatch",
            "  ┌─ src/main.gleam:10:3",
            "  │",
            "10 │   let x: Int = \"hello\"",
            "   │                ^^^^^^^",
            "  │",
            "  Expected: Int",
            "  Found: String",
            "",
        ];
        let mut state = Vec::new();
        let mut events = Vec::new();
        for line in lines {
            let (s, ev) = review_action_parse_line("gleam-build", line.as_bytes(), &state);
            state = s;
            events.extend(ev);
        }

        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::NewEntry { raw_cmd, .. } => {
                assert!(raw_cmd.contains("\"line\":10"));
                assert!(raw_cmd.contains("\"col\":3"));
                assert!(raw_cmd.contains("Type mismatch"));
                assert!(raw_cmd.contains("Expected: Int"));
                assert!(raw_cmd.contains("Found: String"));
                assert!(!raw_cmd.contains("let x: Int"));
            }
            _ => panic!("Expected NewEntry"),
        }
    }

    #[test]
    fn warning_preserves_explanation_and_diagnostic_category() {
        let lines = [
            "warning: Unused variable",
            "  ┌─ src/main.gleam:5:7",
            "  │",
            "  This variable is never used.",
            "",
        ];
        let mut state = Vec::new();
        let mut events = Vec::new();
        for line in lines {
            let (s, ev) = review_action_parse_line("gleam-build", line.as_bytes(), &state);
            state = s;
            events.extend(ev);
        }

        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::NewEntry { category, raw_cmd, .. } => {
                assert_eq!(category, "diagnostic");
                assert!(raw_cmd.contains("\"severity\":\"warning\""));
                assert!(raw_cmd.contains("This variable is never used."));
            }
            _ => panic!("Expected NewEntry"),
        }
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
