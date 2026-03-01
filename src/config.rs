// KDL config parsing and validation.

use miette::{Context, IntoDiagnostic, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

type Result<T> = miette::Result<T>;

const CONFIG_FILENAMES: &[&str] = &["mtack.kdl", ".mtack.kdl"];
const DEFAULT_SCROLLBACK: usize = 2_000;
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq)]
pub struct Config {
    pub scrollback: usize,
    pub shutdown_timeout: Duration,
    pub procs: Vec<ProcConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcConfig {
    pub name: String,
    pub autostart: bool,
    pub autorestart: bool,
    pub cmd: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub(crate) scrollback: Option<usize>,
    pub unfocus_key: UnfocusKey,
}

impl ProcConfig {
    pub fn scrollback(&self, global: usize) -> usize {
        self.scrollback.unwrap_or(global)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnfocusKey {
    Esc,
    Char(char),
    Ctrl(char),
}

const KNOWN_GLOBAL_NODES: &[&str] = &["proc", "scrollback", "shutdown-timeout"];
const KNOWN_PROC_NODES: &[&str] = &[
    "autostart",
    "autorestart",
    "cmd",
    "cwd",
    "env",
    "scrollback",
    "shell",
    "unfocus-key",
];

impl Config {
    pub fn load(start: &Path) -> Result<Self> {
        let path = find_config_file(start)?;
        let input = std::fs::read_to_string(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to read {}", path.display()))?;
        parse(&input)
    }
}

pub fn find_config_file(start: &Path) -> Result<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        for name in CONFIG_FILENAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        if !dir.pop() {
            bail!("no mtack.kdl config file found");
        }
    }
}

pub fn parse(input: &str) -> Result<Config> {
    let doc: kdl::KdlDocument = input.parse().into_diagnostic()?;

    reject_unknown_nodes(doc.nodes(), KNOWN_GLOBAL_NODES, &["proc"], "top level")?;

    let scrollback = parse_positive_int(&doc, "scrollback")?.unwrap_or(DEFAULT_SCROLLBACK);
    let shutdown_timeout = parse_nonneg_int(&doc, "shutdown-timeout")?
        .map_or(DEFAULT_SHUTDOWN_TIMEOUT, Duration::from_secs);

    let proc_nodes: Vec<_> = doc
        .nodes()
        .iter()
        .filter(|n| n.name().value() == "proc")
        .collect();
    if proc_nodes.is_empty() {
        bail!("config must contain at least one proc");
    }

    let mut procs = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    for node in proc_nodes {
        let proc = parse_proc(node)?;
        if !seen_names.insert(proc.name.clone()) {
            bail!("duplicate proc name: {:?}", proc.name);
        }
        procs.push(proc);
    }

    Ok(Config {
        scrollback,
        shutdown_timeout,
        procs,
    })
}

fn reject_unknown_nodes(
    nodes: &[kdl::KdlNode],
    known: &[&str],
    allow_repeated: &[&str],
    context: &str,
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for node in nodes {
        let name = node.name().value();
        if !known.contains(&name) {
            bail!("unknown node {name:?} at {context}");
        }
        if !allow_repeated.contains(&name) && !seen.insert(name) {
            bail!("duplicate node {name:?} at {context}");
        }
    }
    Ok(())
}

fn parse_positive_int(doc: &kdl::KdlDocument, name: &str) -> Result<Option<usize>> {
    let Some(node) = doc.get(name) else {
        return Ok(None);
    };
    let val = node
        .get(0)
        .and_then(|v| v.as_integer())
        .ok_or_else(|| miette::miette!("{name} must be a positive integer"))?;
    if val <= 0 {
        bail!("{name} must be a positive integer");
    }
    let val: usize = val
        .try_into()
        .map_err(|_| miette::miette!("{name} value is too large"))?;
    Ok(Some(val))
}

fn parse_nonneg_int(doc: &kdl::KdlDocument, name: &str) -> Result<Option<u64>> {
    let Some(node) = doc.get(name) else {
        return Ok(None);
    };
    let val = node
        .get(0)
        .and_then(|v| v.as_integer())
        .ok_or_else(|| miette::miette!("{name} must be a non-negative integer"))?;
    if val < 0 {
        bail!("{name} must be a non-negative integer");
    }
    let val: u64 = val
        .try_into()
        .map_err(|_| miette::miette!("{name} value is too large"))?;
    Ok(Some(val))
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        if path == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn parse_proc(node: &kdl::KdlNode) -> Result<ProcConfig> {
    let name = node
        .get(0)
        .and_then(|v| v.as_string())
        .ok_or_else(|| miette::miette!("proc requires a name argument"))?
        .to_string();
    if name.is_empty() {
        bail!("proc name must not be empty");
    }

    let children = node
        .children()
        .ok_or_else(|| miette::miette!("proc {name:?} has no body"))?;

    reject_unknown_nodes(
        children.nodes(),
        KNOWN_PROC_NODES,
        &[],
        &format!("proc {name:?}"),
    )?;

    let cmd = parse_command(children, &name)?;
    let cwd = children
        .get_arg("cwd")
        .and_then(|v| v.as_string())
        .map(expand_tilde);

    let env = parse_env(children)?;
    let autostart = parse_bool_field(children, "autostart")?.unwrap_or(true);
    let autorestart = parse_bool_field(children, "autorestart")?.unwrap_or(true);
    let unfocus_key = parse_unfocus_key(children)?;
    let scrollback = parse_positive_int(children, "scrollback")?;

    Ok(ProcConfig {
        name,
        autostart,
        autorestart,
        cmd,
        cwd,
        env,
        scrollback,
        unfocus_key,
    })
}

fn parse_command(doc: &kdl::KdlDocument, proc_name: &str) -> Result<Vec<String>> {
    let has_cmd = doc.get("cmd").is_some();
    let has_shell = doc.get("shell").is_some();
    match (has_cmd, has_shell) {
        (true, true) => bail!("proc {proc_name:?} has both cmd and shell; use only one"),
        (false, false) => bail!("proc {proc_name:?} is missing cmd or shell"),
        (true, false) => parse_cmd(doc, proc_name),
        (false, true) => parse_shell(doc, proc_name),
    }
}

fn parse_cmd(doc: &kdl::KdlDocument, proc_name: &str) -> Result<Vec<String>> {
    let node = doc.get("cmd").unwrap();
    let positional: Vec<_> = node
        .entries()
        .iter()
        .filter(|e| e.name().is_none())
        .collect();
    if positional.is_empty() {
        bail!("cmd in proc {proc_name:?} must have at least one argument");
    }
    positional
        .into_iter()
        .map(|e| {
            e.value().as_string().map(String::from).ok_or_else(|| {
                miette::miette!("cmd arguments must be strings in proc {proc_name:?}")
            })
        })
        .collect()
}

fn parse_shell(doc: &kdl::KdlDocument, proc_name: &str) -> Result<Vec<String>> {
    let node = doc.get("shell").unwrap();
    let val = node
        .get(0)
        .and_then(|v| v.as_string())
        .ok_or_else(|| miette::miette!("shell in proc {proc_name:?} must be a string"))?;
    if val.is_empty() {
        bail!("shell in proc {proc_name:?} must not be empty");
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
    Ok(vec![shell, "-c".to_string(), val.to_string()])
}

fn parse_env(doc: &kdl::KdlDocument) -> Result<Vec<(String, String)>> {
    let Some(node) = doc.get("env") else {
        return Ok(Vec::new());
    };
    let children = node
        .children()
        .ok_or_else(|| miette::miette!("env must have a child block"))?;
    let mut env = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for child in children.nodes() {
        let key = child.name().value().to_string();
        if !seen.insert(key.clone()) {
            bail!("duplicate env variable {key:?}");
        }
        let val = child
            .get(0)
            .and_then(|v| v.as_string())
            .ok_or_else(|| miette::miette!("env variable {key:?} must have a string value"))?
            .to_string();
        env.push((key, val));
    }
    Ok(env)
}

fn parse_bool_field(doc: &kdl::KdlDocument, name: &str) -> Result<Option<bool>> {
    let Some(node) = doc.get(name) else {
        return Ok(None);
    };
    let val = node
        .get(0)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| miette::miette!("{name} must be a boolean"))?;
    Ok(Some(val))
}

fn parse_unfocus_key(doc: &kdl::KdlDocument) -> Result<UnfocusKey> {
    let Some(node) = doc.get("unfocus-key") else {
        return Ok(UnfocusKey::Esc);
    };
    let val = node
        .get(0)
        .and_then(|v| v.as_string())
        .ok_or_else(|| miette::miette!("unfocus-key must be a string"))?;
    match val {
        "esc" => Ok(UnfocusKey::Esc),
        s if s.starts_with("ctrl-") => {
            let ch = s.strip_prefix("ctrl-").unwrap();
            let mut chars = ch.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_lowercase() => Ok(UnfocusKey::Ctrl(c)),
                _ => bail!("unfocus-key ctrl- must be followed by a single lowercase letter"),
            }
        }
        s => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Ok(UnfocusKey::Char(c)),
                _ => bail!("unfocus-key must be \"esc\", a single character, or \"ctrl-<letter>\""),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn minimal_config() -> &'static str {
        r#"proc "test" { cmd "echo" "hello"; }"#
    }

    #[test]
    fn parse_minimal() {
        let config = parse(minimal_config()).unwrap();
        assert_eq!(config.scrollback, DEFAULT_SCROLLBACK);
        assert_eq!(config.shutdown_timeout, DEFAULT_SHUTDOWN_TIMEOUT);
        assert_eq!(config.procs.len(), 1);
        assert_eq!(config.procs[0].name, "test");
        assert_eq!(config.procs[0].cmd, vec!["echo", "hello"]);
        assert_eq!(config.procs[0].cwd, None);
        assert!(config.procs[0].env.is_empty());
        assert!(config.procs[0].autostart);
        assert!(config.procs[0].autorestart);
        assert_eq!(config.procs[0].unfocus_key, UnfocusKey::Esc);
    }

    #[test]
    fn parse_full_config() {
        let input = r#"
scrollback 5000
shutdown-timeout 10

proc "api" {
    cmd "docker" "compose" "up"
    cwd "~/repos/api"
    env {
        COMPOSE_PROJECT_NAME "api"
        LOG_LEVEL "debug"
    }
    autostart #false
    autorestart #false
    unfocus-key "ctrl-c"
}

proc "web" {
    cmd "pnpm" "dev"
}
"#;
        let config = parse(input).unwrap();
        assert_eq!(config.scrollback, 5000);
        assert_eq!(config.shutdown_timeout, Duration::from_secs(10));
        assert_eq!(config.procs.len(), 2);

        let api = &config.procs[0];
        assert_eq!(api.name, "api");
        assert_eq!(api.cmd, vec!["docker", "compose", "up"]);
        assert!(api.cwd.is_some());
        assert!(api.cwd.as_ref().unwrap().ends_with("repos/api"));
        assert_eq!(
            api.env,
            vec![
                ("COMPOSE_PROJECT_NAME".into(), "api".into()),
                ("LOG_LEVEL".into(), "debug".into()),
            ]
        );
        assert!(!api.autostart);
        assert!(!api.autorestart);
        assert_eq!(api.unfocus_key, UnfocusKey::Ctrl('c'));

        let web = &config.procs[1];
        assert_eq!(web.name, "web");
        assert_eq!(web.cmd, vec!["pnpm", "dev"]);
        assert!(web.autostart);
        assert!(web.autorestart);
    }

    #[test]
    fn error_no_procs() {
        let err = parse("scrollback 5000").unwrap_err();
        assert!(err.to_string().contains("at least one proc"));
    }

    #[test]
    fn error_missing_cmd_or_shell() {
        let err = parse(r#"proc "test" { cwd "/tmp"; }"#).unwrap_err();
        assert!(err.to_string().contains("missing cmd or shell"));
    }

    #[test]
    fn error_empty_cmd() {
        let err = parse(r#"proc "test" { cmd; }"#).unwrap_err();
        assert!(err.to_string().contains("at least one argument"));
    }

    #[test]
    fn error_duplicate_proc_names() {
        let input = r#"
proc "dup" { cmd "a"; }
proc "dup" { cmd "b"; }
"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("duplicate proc name"));
    }

    #[test]
    fn error_unknown_global_node() {
        let input = r#"
scroolback 5000
proc "test" { cmd "echo"; }
"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("unknown node"));
        assert!(err.to_string().contains("scroolback"));
    }

    #[test]
    fn error_unknown_proc_node() {
        let input = r#"proc "test" { cmd "echo"; comand "bad"; }"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("unknown node"));
        assert!(err.to_string().contains("comand"));
    }

    #[test]
    fn error_scrollback_not_positive() {
        let input = r#"
scrollback 0
proc "test" { cmd "echo"; }
"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("positive integer"));
    }

    #[test]
    fn error_scrollback_negative() {
        let input = r#"
scrollback -1
proc "test" { cmd "echo"; }
"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("positive integer"));
    }

    #[test]
    fn error_shutdown_timeout_negative() {
        let input = r#"
shutdown-timeout -1
proc "test" { cmd "echo"; }
"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("non-negative integer"));
    }

    #[test]
    fn shutdown_timeout_zero_is_valid() {
        let input = r#"
shutdown-timeout 0
proc "test" { cmd "echo"; }
"#;
        let config = parse(input).unwrap();
        assert_eq!(config.shutdown_timeout, Duration::ZERO);
    }

    #[test]
    fn error_proc_no_name() {
        let err = parse(r#"proc { cmd "echo"; }"#).unwrap_err();
        assert!(err.to_string().contains("requires a name"));
    }

    #[test]
    fn error_proc_empty_name() {
        let err = parse(r#"proc "" { cmd "echo"; }"#).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn tilde_expansion_in_cwd() {
        let input = r#"proc "test" { cmd "echo"; cwd "~/foo/bar"; }"#;
        let config = parse(input).unwrap();
        let cwd = config.procs[0].cwd.as_ref().unwrap();
        // Should not start with ~
        assert!(!cwd.to_str().unwrap().starts_with('~'));
        assert!(cwd.to_str().unwrap().ends_with("foo/bar"));
    }

    #[test]
    fn cwd_without_tilde() {
        let input = r#"proc "test" { cmd "echo"; cwd "/absolute/path"; }"#;
        let config = parse(input).unwrap();
        assert_eq!(
            config.procs[0].cwd.as_ref().unwrap(),
            &PathBuf::from("/absolute/path")
        );
    }

    #[test]
    fn find_config_walks_upward() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a").join("b").join("c");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.path().join("mtack.kdl"), "").unwrap();

        let found = find_config_file(&sub).unwrap();
        assert_eq!(found, dir.path().join("mtack.kdl"));
    }

    #[test]
    fn find_config_prefers_mtack_kdl_over_dotfile() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("mtack.kdl"), "").unwrap();
        fs::write(dir.path().join(".mtack.kdl"), "").unwrap();

        let found = find_config_file(dir.path()).unwrap();
        assert_eq!(found, dir.path().join("mtack.kdl"));
    }

    #[test]
    fn find_config_falls_back_to_dotfile() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".mtack.kdl"), "").unwrap();

        let found = find_config_file(dir.path()).unwrap();
        assert_eq!(found, dir.path().join(".mtack.kdl"));
    }

    #[test]
    fn find_config_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = find_config_file(dir.path()).unwrap_err();
        assert!(err.to_string().contains("no mtack.kdl config file found"));
    }

    #[test]
    fn unfocus_key_default_is_esc() {
        let config = parse(minimal_config()).unwrap();
        assert_eq!(config.procs[0].unfocus_key, UnfocusKey::Esc);
    }

    #[test]
    fn unfocus_key_explicit_esc() {
        let input = r#"proc "test" { cmd "echo"; unfocus-key "esc"; }"#;
        let config = parse(input).unwrap();
        assert_eq!(config.procs[0].unfocus_key, UnfocusKey::Esc);
    }

    #[test]
    fn unfocus_key_custom() {
        let input = r#"proc "test" { cmd "echo"; unfocus-key "ctrl-c"; }"#;
        let config = parse(input).unwrap();
        assert_eq!(config.procs[0].unfocus_key, UnfocusKey::Ctrl('c'));
    }

    #[test]
    fn error_invalid_unfocus_key() {
        let input = r#"proc "test" { cmd "echo"; unfocus-key "ctrl-"; }"#;
        assert!(parse(input).is_err());

        let input = r#"proc "test" { cmd "echo"; unfocus-key "ctrl-ab"; }"#;
        assert!(parse(input).is_err());

        let input = r#"proc "test" { cmd "echo"; unfocus-key "ctrl-C"; }"#;
        assert!(parse(input).is_err());

        let input = r#"proc "test" { cmd "echo"; unfocus-key "foo"; }"#;
        assert!(parse(input).is_err());
    }

    #[test]
    fn valid_single_char_unfocus_key() {
        let input = r#"proc "test" { cmd "echo"; unfocus-key "q"; }"#;
        let config = parse(input).unwrap();
        assert_eq!(config.procs[0].unfocus_key, UnfocusKey::Char('q'));
    }

    #[test]
    fn error_cmd_non_string_arg() {
        let input = r#"proc "test" { cmd "echo" 42; }"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("must be strings"));
    }

    #[test]
    fn error_duplicate_env_keys() {
        let input = r#"
proc "test" {
    cmd "echo"
    env {
        FOO "a"
        FOO "b"
    }
}
"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("duplicate env variable"));
    }

    #[test]
    fn error_env_without_children() {
        let input = r#"proc "test" { cmd "echo"; env; }"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("child block"));
    }

    #[test]
    fn error_duplicate_global_node() {
        let input = r#"
scrollback 5000
scrollback 8000
proc "test" { cmd "echo"; }
"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("duplicate node"));
    }

    #[test]
    fn bare_tilde_expansion() {
        let input = r#"proc "test" { cmd "echo"; cwd "~"; }"#;
        let config = parse(input).unwrap();
        let cwd = config.procs[0].cwd.as_ref().unwrap();
        assert!(!cwd.to_str().unwrap().contains('~'));
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("mtack.kdl"),
            r#"proc "test" { cmd "echo" "hi"; }"#,
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config.procs[0].name, "test");
    }

    #[test]
    fn shell_basic_parse() {
        let input = r#"proc "test" { shell "echo hello | cat"; }"#;
        let config = parse(input).unwrap();
        let cmd = &config.procs[0].cmd;
        assert_eq!(cmd.len(), 3);
        assert_eq!(cmd[1], "-c");
        assert_eq!(cmd[2], "echo hello | cat");
    }

    #[test]
    fn error_both_cmd_and_shell() {
        let input = r#"proc "test" { cmd "echo"; shell "echo"; }"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("both cmd and shell"));
    }

    #[test]
    fn error_shell_empty() {
        let input = r#"proc "test" { shell ""; }"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn proc_scrollback_override() {
        let input = r#"
scrollback 5000
proc "a" { cmd "echo"; scrollback 50000; }
proc "b" { cmd "echo"; }
"#;
        let config = parse(input).unwrap();
        assert_eq!(config.procs[0].scrollback(config.scrollback), 50000);
        assert_eq!(config.procs[1].scrollback(config.scrollback), 5000);
    }

    #[test]
    fn error_proc_scrollback_not_positive() {
        let input = r#"proc "test" { cmd "echo"; scrollback 0; }"#;
        let err = parse(input).unwrap_err();
        assert!(err.to_string().contains("positive integer"));
    }
}
