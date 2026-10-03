//! CLI regression tests using isolated configuration directories.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct ConfigHome(PathBuf);

impl ConfigHome {
    fn new() -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "prctrl-config-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn config_path(&self) -> PathBuf {
        let base = if cfg!(target_os = "macos") {
            self.0.join("Library/Application Support")
        } else {
            self.0.join("config")
        };
        base.join("prctrl/config.toml")
    }

    fn write_config(&self, content: &str) {
        let path = self.config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn init(&self, force: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_prctrl"));
        command
            .args(["config", "init"])
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if force {
            command.arg("--force");
        }
        let mut child = command.spawn().unwrap();
        // The refusal path may exit before reading stdin.
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(b"test-token\ntest-user\ntest-org\nrepo\n\n");
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        output
    }

    fn assert_initialized(&self) {
        let content = std::fs::read_to_string(self.config_path()).unwrap();
        let config: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(config["github"]["username"].as_str(), Some("test-user"));
        assert_eq!(config["github"]["org"].as_str(), Some("test-org"));
    }
}

impl Drop for ConfigHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn config_init_creates_missing_file() {
    let home = ConfigHome::new();
    home.init(false);
    home.assert_initialized();
}

#[test]
fn config_init_populates_existing_empty_file() {
    let home = ConfigHome::new();
    home.write_config("");
    home.init(false);
    home.assert_initialized();
}

#[test]
fn config_init_preserves_existing_content() {
    let home = ConfigHome::new();
    let content = "# Existing configuration\n";
    home.write_config(content);
    let output = home.init(false);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Use --force"));
    assert_eq!(
        std::fs::read_to_string(home.config_path()).unwrap(),
        content
    );
}

#[test]
fn config_init_force_replaces_existing_content() {
    let home = ConfigHome::new();
    home.write_config("# Existing configuration\n");
    home.init(true);
    home.assert_initialized();
}

#[test]
fn config_loading_resolves_repository_owner() {
    for (file_owner, override_owner, expected) in [
        (None, None, "test-user"),
        (Some(""), None, "test-user"),
        (Some("  "), None, "test-user"),
        (Some("my-org"), None, "my-org"),
        (Some("my-org"), Some(""), "test-user"),
        (Some("my-org"), Some("other-org"), "other-org"),
    ] {
        let home = ConfigHome::new();
        let mut content = "[github]\ntoken = \"test-token\"\nusername = \"test-user\"\n".to_owned();
        if let Some(owner) = file_owner {
            content.push_str(&format!("org = \"{owner}\"\n"));
        }
        home.write_config(&content);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "config_loading_child"])
            .current_dir(&home.0)
            .env("HOME", &home.0)
            .env("XDG_CONFIG_HOME", home.0.join("config"));
        // Isolate overrides and dotenv loading without mutating this test process.
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("PRCTRL_") || name.starts_with("GITHUB_") {
                command.env_remove(key);
            }
        }
        command.env("PRCTRL_TEST_EXPECTED_OWNER", expected);
        if let Some(owner) = override_owner {
            command.env("PRCTRL_GITHUB_ORG", owner);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

#[test]
fn config_loading_child() {
    let Ok(expected) = std::env::var("PRCTRL_TEST_EXPECTED_OWNER") else {
        return;
    };
    let config = prctrl::config::Config::from_env().unwrap();
    assert_eq!(config.github_org, expected);
}
