use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub github_token: String,
    pub github_username: String,
    pub github_org: String,
    pub github_repos: Vec<String>,
    pub github_teams: Vec<String>,
    pub crew_members: Vec<String>,
    #[allow(dead_code)]
    pub anthropic_api_key: Option<String>,
    pub exclude_prefix: Vec<String>,
    pub max_pr_age_days: Option<u32>,
}

fn get_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("prctrl")
        .join("config.toml")
}

fn load_toml_config() -> Option<toml::Table> {
    let path = get_config_path();
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

fn get_toml_str(table: &toml::Table, key: &str) -> Option<String> {
    table
        .get("github")
        .and_then(|s| s.as_table())
        .and_then(|t| t.get(key))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn get_toml_array(table: &toml::Table, key: &str) -> Vec<String> {
    table
        .get("github")
        .and_then(|s| s.as_table())
        .and_then(|t| t.get(key))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        let toml = load_toml_config();

        // Required: token
        let github_token = if let Ok(v) = std::env::var("PRCTRL_GITHUB_TOKEN") {
            v
        } else if let Ok(v) = std::env::var("GITHUB_TOKEN") {
            v
        } else if let Some(ref t) = toml {
            get_toml_str(t, "token")
                .ok_or_else(|| anyhow::anyhow!("missing github.token in config"))?
        } else {
            anyhow::bail!("Set PRCTRL_GITHUB_TOKEN or run `prctrl config init`");
        };

        // Required: username
        let github_username = if let Ok(v) = std::env::var("PRCTRL_GITHUB_USERNAME") {
            v
        } else if let Ok(v) = std::env::var("GITHUB_USERNAME") {
            v
        } else if let Some(ref t) = toml {
            get_toml_str(t, "username")
                .ok_or_else(|| anyhow::anyhow!("missing github.username in config"))?
        } else {
            anyhow::bail!("Set PRCTRL_GITHUB_USERNAME or run `prctrl config init`");
        };

        // Required: org
        let github_org = if let Ok(v) = std::env::var("PRCTRL_GITHUB_ORG") {
            v
        } else if let Ok(v) = std::env::var("GITHUB_ORG") {
            v
        } else if let Some(ref t) = toml {
            get_toml_str(t, "org").ok_or_else(|| anyhow::anyhow!("missing github.org in config"))?
        } else {
            anyhow::bail!("Set PRCTRL_GITHUB_ORG or run `prctrl config init`");
        };

        // Optional: repos
        let github_repos = if let Ok(v) = std::env::var("PRCTRL_GITHUB_REPOS") {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else if let Ok(v) = std::env::var("GITHUB_REPOS") {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else if let Some(ref t) = toml {
            get_toml_array(t, "repos")
        } else {
            vec![]
        };

        // Optional: teams
        let github_teams = if let Ok(v) = std::env::var("PRCTRL_GITHUB_TEAMS") {
            v.split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        } else if let Ok(v) = std::env::var("GITHUB_TEAMS") {
            v.split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        } else if let Some(ref t) = toml {
            get_toml_array(t, "teams")
        } else {
            vec![]
        };

        // Optional: crew
        let crew_members = if let Ok(v) = std::env::var("PRCTRL_CREW_MEMBERS") {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else if let Ok(v) = std::env::var("CREW_MEMBERS") {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else if let Some(ref t) = toml {
            t.get("github")
                .and_then(|s| s.as_table())
                .and_then(|t| t.get("crew_members"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![]
        };

        let anthropic_api_key = if let Ok(v) = std::env::var("PRCTRL_ANTHROPIC_API_KEY") {
            Some(v)
        } else if let Ok(v) = std::env::var("ANTHROPIC_API_KEY") {
            Some(v)
        } else if let Some(ref t) = toml {
            t.get("github")
                .and_then(|s| s.as_table())
                .and_then(|t| t.get("anthropic_api_key"))
                .and_then(|v| v.as_str())
                .map(String::from)
        } else {
            None
        };

        // Load exclude_prefix from config or use default
        let exclude_prefix = if let Some(ref t) = toml {
            let from_config = get_toml_array(t, "exclude_prefix");
            if from_config.is_empty() {
                vec!["chore(deps)".to_string()]
            } else {
                from_config
            }
        } else {
            // Default to filtering out dependency update PRs
            vec!["chore(deps)".to_string()]
        };

        // Load max_pr_age_days from config or use default (60 days)
        let max_pr_age_days = if let Some(ref t) = toml {
            t.get("github")
                .and_then(|s| s.as_table())
                .and_then(|t| t.get("max_pr_age_days"))
                .and_then(|v| v.as_integer())
                .map(|d| d as u32)
        } else {
            None
        }
        .or_else(|| {
            // Check environment variable
            std::env::var("PRCTRL_MAX_PR_AGE_DAYS")
                .ok()
                .and_then(|s| s.parse().ok())
                .or_else(|| {
                    std::env::var("MAX_PR_AGE_DAYS")
                        .ok()
                        .and_then(|s| s.parse().ok())
                })
        })
        .or(Some(60)); // Default to 60 days

        Ok(Self {
            github_token,
            github_username,
            github_org,
            github_repos,
            github_teams,
            crew_members,
            anthropic_api_key,
            exclude_prefix,
            max_pr_age_days,
        })
    }
}
