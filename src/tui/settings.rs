use anyhow::{Context, Result};
use std::path::Path;

pub(crate) const FIELDS: &[(&str, &str)] = &[
    ("token", "GitHub token"),
    ("username", "GitHub username"),
    ("org", "Repository owner"),
    ("repos", "Repositories (comma-separated)"),
    ("teams", "Teams (comma-separated)"),
    ("crew_members", "Crew (comma-separated)"),
    ("anthropic_api_key", "Anthropic API key"),
    ("exclude_prefix", "Excluded prefixes (comma-separated)"),
    ("max_pr_age_days", "Maximum PR age in days"),
];

pub(crate) struct Settings {
    table: toml::Table,
    original: String,
    original_values: Vec<String>,
    pub values: Vec<String>,
    pub selected: usize,
    pub editing: bool,
    before_edit: String,
    pub message: String,
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        let original = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let table: toml::Table = toml::from_str(&original).context("Cannot parse configuration")?;
        if table.get("github").is_some_and(|v| !v.is_table()) {
            anyhow::bail!("Configuration github section must be a table");
        }
        let github = table.get("github").and_then(|v| v.as_table());
        let values: Vec<String> = FIELDS
            .iter()
            .map(|(key, _)| match github.and_then(|t| t.get(*key)) {
                Some(toml::Value::String(s)) => s.clone(),
                Some(toml::Value::Integer(n)) => n.to_string(),
                Some(toml::Value::Array(a)) => a
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => String::new(),
            })
            .collect();
        Ok(Self {
            table,
            original,
            original_values: values.clone(),
            values,
            selected: 0,
            editing: false,
            before_edit: String::new(),
            message: String::new(),
        })
    }

    pub fn begin_edit(&mut self) {
        self.before_edit = self.values[self.selected].clone();
        self.editing = true;
        self.message.clear();
    }

    pub fn cancel_edit(&mut self) {
        self.values[self.selected] = self.before_edit.clone();
        self.editing = false;
    }

    pub fn live_lists(&self) -> (Vec<String>, Vec<String>) {
        let list = |key: &str, primary: &str, fallback: &str| {
            if let Ok(value) = std::env::var(primary).or_else(|_| std::env::var(fallback)) {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            } else {
                self.table
                    .get("github")
                    .and_then(|v| v.get(key))
                    .and_then(|v| v.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            }
        };
        (
            list("repos", "PRCTRL_GITHUB_REPOS", "GITHUB_REPOS"),
            list("crew_members", "PRCTRL_CREW_MEMBERS", "CREW_MEMBERS"),
        )
    }

    pub fn save(&mut self, path: &Path) -> Result<()> {
        let mut table = self.table.clone();
        let github = table
            .entry("github")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .unwrap();
        for (i, ((key, _), value)) in FIELDS.iter().zip(&self.values).enumerate() {
            if value == &self.original_values[i] {
                continue;
            }
            let old = self.table.get("github").and_then(|v| v.get(*key));
            let value = value.trim();
            let parsed = match *key {
                "max_pr_age_days" if !value.is_empty() => toml::Value::Integer(
                    value
                        .parse::<u32>()
                        .context("Maximum PR age must be a non-negative whole number")?
                        as i64,
                ),
                "repos" | "teams" | "crew_members" | "exclude_prefix" => toml::Value::Array(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| toml::Value::String(s.into()))
                        .collect(),
                ),
                _ => toml::Value::String(value.into()),
            };
            if value.is_empty() && old.is_none() {
                continue;
            }
            if *key == "max_pr_age_days" && value.is_empty() {
                github.remove(*key);
            } else {
                github.insert((*key).into(), parsed);
            }
        }
        let current = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        anyhow::ensure!(
            current == self.original,
            "Configuration changed on disk. Close and reopen settings before saving."
        );
        let content = toml::to_string_pretty(&table)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("toml.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .context("Cannot create temporary configuration")?;
        use std::io::Write;
        let result = (|| -> Result<()> {
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
        self.original_values = self.values.clone();
        self.table = table;
        self.original = content;
        self.message =
            "Saved. Repositories and crew apply now; other changes require restarting.".into();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_fields_preserves_unknown_values_and_detects_external_changes() {
        let dir = std::env::temp_dir().join(format!("prctrl-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[other]\nkeep = true\n[github]\nusername = 'alice'\ncustom = 42\n",
        )
        .unwrap();
        let mut settings = Settings::load(&path).unwrap();
        settings.values[3] = "one, two, ,three".into();
        settings.values[8] = "-1".into();
        assert!(settings.save(&path).is_err());
        settings.values[8] = "30".into();
        settings.save(&path).unwrap();
        let table: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(table["other"]["keep"].as_bool(), Some(true));
        assert_eq!(table["github"]["custom"].as_integer(), Some(42));
        assert_eq!(table["github"]["repos"].as_array().unwrap().len(), 3);
        assert!(table["github"].get("token").is_none());
        std::fs::write(&path, "[github]\nusername = 'bob'\n").unwrap();
        assert!(settings.save(&path).is_err());
        assert!(std::fs::read_to_string(&path).unwrap().contains("bob"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
