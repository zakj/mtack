// Runs the real binary through the background-server lifecycle. The
// terminal-facing parts (attach) are covered in-process by app.rs tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Its own server directory (so tests never see real servers), a directory
/// to run from that has no config, and helpers to make configs. Lives under
/// /tmp because socket paths must fit in about 100 bytes.
struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::create_dir(root.path().join("elsewhere")).unwrap();
        Self { root }
    }

    fn config(&self, name: &str, proc_body: &str) -> PathBuf {
        let dir = self.root.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let config = dir.join("mtack.kdl");
        let body = format!(r#"proc "p" {{ {proc_body} autorestart #false; }}"#);
        std::fs::write(&config, body).unwrap();
        config
    }

    /// Runs mtack from a directory without a config.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mtack"));
        command
            .current_dir(self.root.path().join("elsewhere"))
            .env("MTACK_TMPDIR", self.root.path())
            .args(args);
        command
    }

    fn mtack(&self, args: &[&str]) -> (bool, String) {
        let out = self.command(args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn with_config(&self, config: &Path, cmd: &str) -> (bool, String) {
        self.mtack(&["-c", config.to_str().unwrap(), cmd])
    }
}

/// Stops every server started in the sandbox, even if an assertion fails or
/// a server has wedged or become unreachable.
struct Cleanup<'a>(&'a Sandbox, Vec<PathBuf>);

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        for config in &self.1 {
            let mut down = self
                .0
                .command(&["-c", config.to_str().unwrap(), "down"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while down.try_wait().unwrap().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = down.kill();
        }
        // Whatever is left names a config inside the sandbox.
        let root = self.0.root.path().canonicalize().unwrap();
        let _ = Command::new("pkill")
            .args(["-f", root.to_str().unwrap()])
            .status();
    }
}

/// `up` returns once the socket is ready, before processes have spawned.
fn wait_for_contents(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::read_to_string(path) {
            Ok(contents) if !contents.is_empty() => return contents.trim().to_string(),
            _ => {
                assert!(Instant::now() < deadline, "process never ran");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn up_and_down() {
    let sandbox = Sandbox::new();
    let marker = sandbox.root.path().join("ran");
    let config = sandbox.config(
        "proj",
        &format!(r#"shell "pwd -P > {}; sleep 30";"#, marker.display()),
    );
    let _cleanup = Cleanup(&sandbox, vec![config.clone()]);

    let (ok, err) = sandbox.with_config(&config, "up");
    assert!(ok && err.contains("started"), "{err}");
    let (ok, err) = sandbox.with_config(&config, "up");
    assert!(ok && err.contains("already running"), "{err}");

    wait_for_contents(&marker);

    let (ok, err) = sandbox.with_config(&config, "down");
    assert!(ok && err.is_empty(), "{err}");
    let (ok, err) = sandbox.with_config(&config, "down");
    assert!(ok && err.contains("not running"), "{err}");
}

#[test]
fn without_a_config_down_finds_running_servers() {
    let sandbox = Sandbox::new();
    let a = sandbox.config("a", r#"cmd "sleep" "30";"#);
    let b = sandbox.config("b", r#"cmd "sleep" "30";"#);
    let _cleanup = Cleanup(&sandbox, vec![a.clone(), b.clone()]);
    assert!(sandbox.with_config(&a, "up").0);
    assert!(sandbox.with_config(&b, "up").0);

    let (ok, err) = sandbox.mtack(&["down"]);
    assert!(
        !ok && err.contains("a/mtack.kdl") && err.contains("b/mtack.kdl"),
        "{err}"
    );

    assert!(sandbox.with_config(&a, "down").0);
    let (ok, err) = sandbox.mtack(&["down"]);
    assert!(ok, "{err}");
    let (_, err) = sandbox.with_config(&b, "down");
    assert!(
        err.contains("not running"),
        "only server wasn't stopped: {err}"
    );

    let (ok, err) = sandbox.mtack(&["down"]);
    assert!(!ok && err.contains("no mtack.kdl"), "{err}");
}

#[test]
fn a_deleted_config_can_still_be_stopped() {
    let sandbox = Sandbox::new();
    let a = sandbox.config("a", r#"cmd "sleep" "30";"#);
    let b = sandbox.config("b", r#"cmd "sleep" "30";"#);
    // What the running-server listing shows.
    let listed_a = a.canonicalize().unwrap();
    let listed_b = b.canonicalize().unwrap();
    let _cleanup = Cleanup(&sandbox, vec![listed_a.clone(), listed_b.clone()]);
    assert!(sandbox.with_config(&a, "up").0);
    assert!(sandbox.with_config(&b, "up").0);
    std::fs::remove_file(&a).unwrap();
    std::fs::remove_file(&b).unwrap();

    let (ok, err) = sandbox.with_config(&listed_a, "down");
    assert!(ok && err.is_empty(), "{err}");
    let (ok, err) = sandbox.mtack(&["down"]);
    assert!(ok && err.is_empty(), "{err}");
    let (_, err) = sandbox.mtack(&["down"]);
    assert!(
        err.contains("no mtack.kdl"),
        "a server is still running: {err}"
    );
}

#[test]
fn an_unsafe_server_directory_is_reported() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let sandbox = Sandbox::new();
    let uid = sandbox.root.path().metadata().unwrap().uid();
    let dir = sandbox.root.path().join(format!("mtack-{uid}"));
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (ok, err) = sandbox.mtack(&["down"]);
    assert!(!ok && err.contains("mode 0700"), "{err}");
}
