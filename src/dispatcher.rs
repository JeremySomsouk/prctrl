use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::github::PendingReview;
use crate::notifications;
use crate::writer;

const INSTRUCTION_FILE: &str = "instruction.md";

pub const PID_FILE: &str = ".prctrl-monitor.pid";

pub fn delegate_to_claude(
    review: &PendingReview,
    custom_instruction_path: Option<PathBuf>,
) -> Result<String> {
    // Use custom path if provided, otherwise use default search
    let (custom_instructions, source_path) = if let Some(path) = custom_instruction_path {
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                (content, path.to_str().map(|s| s.to_string()))
            } else {
                read_custom_instructions()
            }
        } else {
            read_custom_instructions()
        }
    } else {
        read_custom_instructions()
    };

    // Log which instruction file was loaded
    if let Some(path) = source_path {
        println!("📖 Using custom instructions from: {}", path);
    } else {
        println!("📖 Using default review instructions");
    }

    let prompt = format!(
        "You are a senior engineer doing a code review pre-screening.\n\
         PR: {title} (#{number}) by {author}\n\
         Repo: {repo}\n\
         Link: {url}\n\
         Size: +{add} / -{del} lines\n\
         \n\
         CUSTOM REVIEW INSTRUCTIONS:\n         {instructions}\n\
         Based on this context and the custom instructions above, provide:\n\
         1. A 2-sentence summary of what this PR likely does\n\
         2. Key things the reviewer should pay attention to (considering the custom instructions)\n\
         3. A recommendation: [REVIEW NOW] if urgent/small, [DELEGATE] if large/low priority",
        title = review.pr_title,
        number = review.pr_number,
        author = review.pr_author,
        repo = review.repo,
        url = review.pr_url,
        add = review.additions,
        del = review.deletions,
        instructions = custom_instructions
    );

    // Note: To track actual token usage, we would need to:
    // 1. Count tokens in the prompt and response
    // 2. Multiply by model's price per token
    // 3. Display the actual cost

    // For now, we show Claude's effort estimate
    // Future enhancement: Add actual token counting

    let output = Command::new("claude")
        .args(["--print", "--model", "opus", &prompt])
        .env_remove("CLAUDECODE")
        .output()
        .context("Failed to run `claude` CLI — is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude CLI failed: {}", stderr.trim());
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(text)
}

/// Check if a monitor process is already running
pub fn is_monitor_running() -> bool {
    if let Ok(pid_str) = fs::read_to_string(PID_FILE) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            // Check if process exists (simple check - process might have died)
            #[cfg(unix)]
            {
                match Command::new("kill").arg("-0").arg(pid.to_string()).status() {
                    Ok(_) => return true, // Process exists
                    Err(_) => {
                        // Process doesn't exist, clean up stale PID file
                        let _ = fs::remove_file(PID_FILE);
                        return false;
                    }
                }
            }
            #[cfg(not(unix))]
            return true; // On non-Unix, assume it's running
        }
    }
    false
}

/// Write current process ID to PID file
pub fn write_pid_file() -> Result<()> {
    let pid = std::process::id();
    fs::write(PID_FILE, pid.to_string())?;
    Ok(())
}

/// Remove PID file
pub fn remove_pid_file() -> Result<()> {
    if PathBuf::from(PID_FILE).exists() {
        fs::remove_file(PID_FILE)?;
    }
    Ok(())
}

/// Open URL in default browser
pub fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(url).status()?;
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd").args(["/c", "start", url]).status()?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open").arg(url).status()?;
    }
    Ok(())
}

/// Read custom instructions from instruction.md file
/// Looks in multiple locations:
/// 1. Current directory (./instruction.md)
/// 2. Config directory (~/.prctrl/instruction.md)
/// 3. Environment variable (RD_INSTRUCTION_PATH)
fn read_custom_instructions() -> (String, Option<String>) {
    // Check environment variable first
    if let Ok(path) = std::env::var("RD_INSTRUCTION_PATH") {
        let path_buf = PathBuf::from(&path);
        if path_buf.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                return (content, Some(path));
            }
        }
    }

    // Check current directory
    let current_path = PathBuf::from(INSTRUCTION_FILE);
    if current_path.exists() {
        if let Ok(content) = fs::read_to_string(&current_path) {
            if let Some(path) = current_path.to_str() {
                return (content, Some(path.to_string()));
            }
        }
    }

    // Check user config directory
    if let Some(home_dir) = dirs::home_dir() {
        let config_path = home_dir.join(".prctrl").join(INSTRUCTION_FILE);
        if config_path.exists() {
            if let Ok(content) = fs::read_to_string(&config_path) {
                if let Some(path) = config_path.to_str() {
                    return (content, Some(path.to_string()));
                }
            }
        }
    }

    (
        String::from("Follow the standard code review process."),
        None,
    )
}

/// Open file in IntelliJ IDEA
pub fn open_in_intellij(path: &PathBuf) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        // Try different IntelliJ variants
        let ideas = [
            "IntelliJ IDEA",
            "Goland",
            "CLion",
            "PhpStorm",
            "WebStorm",
            "PyCharm",
            "RubyMine",
            "Rider",
        ];

        for idea in ideas {
            if Command::new("open")
                .arg("-a")
                .arg(idea)
                .arg(path)
                .status()
                .is_ok()
            {
                return Ok(());
            }
        }

        // Fallback to default open
        Command::new("open").arg(path).status()?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        // On other platforms, just open with default application
        #[cfg(target_os = "windows")]
        Command::new("cmd")
            .args(["/c", "start", "", path.to_str().unwrap()])
            .status()?;

        #[cfg(target_os = "linux")]
        Command::new("xdg-open").arg(path).status()?;
    }
    Ok(())
}

/// Kill existing monitor process if running
pub fn kill_existing_monitor() -> Result<bool> {
    if let Ok(pid_str) = fs::read_to_string(PID_FILE) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            #[cfg(unix)]
            {
                match Command::new("kill").arg(pid.to_string()).status() {
                    Ok(_) => {
                        remove_pid_file()?;
                        return Ok(true);
                    }
                    Err(_) => {
                        remove_pid_file()?; // Clean up stale PID file
                        return Ok(false);
                    }
                }
            }
            #[cfg(not(unix))]
            {
                remove_pid_file()?;
                return Ok(false);
            }
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
pub async fn monitor_new_prs(
    token: &str,
    org: &str,
    repos: &[String],
    username: &str,
    teams: &[String],
    include_mine: bool,
    include_drafts: bool,
    exclude_prefixes: &[String],
    crew_members: &[String],
    interval_seconds: u64,
    send_notifications: bool,
    auto_open_browser: bool,
    interactive_mode: bool,
    output_dir: Option<PathBuf>,
) -> Result<()> {
    // Check if another monitor is already running
    if is_monitor_running() {
        return Err(anyhow::anyhow!(
            "A monitor process is already running. Use 'prctrl monitor-stop' to stop it first."
        ));
    }

    // Write PID file
    write_pid_file()?;

    // Set up Ctrl+C handler to clean up PID file
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();

    ctrlc::set_handler(move || {
        running_clone.store(false, Ordering::SeqCst);
        println!("\n🛑 Monitor stopped by user");
        let _ = remove_pid_file();
        std::process::exit(0);
    })?;

    // Show comprehensive configuration info
    println!("\n📋 Monitor Configuration:");

    // Show working directory
    if let Ok(current_dir) = std::env::current_dir() {
        println!("  📁 Working directory: {}", current_dir.display());
    }

    // Show instruction file
    let (_, instruction_path) = read_custom_instructions();
    match instruction_path {
        Some(path) => println!("  📖 Instructions: {}", path),
        None => println!("  📖 Instructions: Using built-in defaults"),
    }

    // Show environment info
    println!("  🔧 Environment:");
    println!("    - Interval: {} seconds", interval_seconds);
    println!(
        "    - Notifications: {}",
        if send_notifications {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "    - Interactive: {}",
        if interactive_mode {
            "enabled"
        } else {
            "disabled"
        }
    );

    // Show filter configuration
    println!("  🎯 Filters:");
    println!("    - Include mine: {}", include_mine);
    println!("    - Include drafts: {}", include_drafts);
    println!("    - Crew mode: {}", !crew_members.is_empty());
    if !exclude_prefixes.is_empty() {
        println!("    - Exclude prefixes: {:?}", exclude_prefixes);
    } else {
        println!("    - Exclude prefixes: none");
    }

    let mut last_pr_count = 0;

    loop {
        // Fetch current pending reviews
        let pending = crate::github::fetch_pending_reviews(
            token,
            org,
            repos,
            username,
            teams,
            include_mine,
            include_drafts,
            exclude_prefixes,
            crew_members,
            None, // max_age_days - not configured in dispatcher
        )
        .await?;

        let current_count = pending.len();

        // Check for new PRs
        if current_count > last_pr_count {
            let new_prs: Vec<_> = pending
                .iter()
                .rev()
                .take(current_count - last_pr_count)
                .collect();

            for pr in new_prs {
                println!("🔔 New PR detected: #{} - {}", pr.pr_number, pr.pr_title);

                if send_notifications {
                    let notification_title = format!("New PR: #{}", pr.pr_number);
                    let notification_message =
                        format!("{} by {} in {}", pr.pr_title, pr.pr_author, pr.repo);

                    if notifications::send_mac_notification(
                        &notification_title,
                        &notification_message,
                        Some(&pr.pr_url),
                        auto_open_browser,
                    ) {
                        if auto_open_browser {
                            println!("✓ macOS notification sent (Chrome will open automatically)");
                        } else {
                            println!("✓ macOS notification sent (URL included in message)");
                        }
                    } else {
                        println!("⚠ Failed to send macOS notification");
                    }
                }

                // Interactive mode - prompt for actions
                if interactive_mode {
                    println!("\n🎯 Quick Actions for PR #{}:", pr.pr_number);
                    println!("  [d] Delegate to Claude for review");
                    println!("  [o] Open PR in browser");
                    println!("  [i] Open in IntelliJ");
                    println!("  [s] Skip this PR");
                    println!("  [q] Quit interactive mode");

                    print!("\nChoose action: ");
                    std::io::stdout().flush()?;

                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    let choice = input.trim().to_lowercase();

                    match choice.as_str() {
                        "d" | "delegate" => {
                            println!("⏳ Delegating PR #{} to Claude...", pr.pr_number);
                            match crate::dispatcher::delegate_to_claude(pr, None) {
                                Ok(summary) => {
                                    println!("✅ Claude review completed:");
                                    println!(
                                        "   {}",
                                        summary.lines().next().unwrap_or("No summary")
                                    );

                                    // Write review to file
                                    if let Some(ref dir) = output_dir {
                                        let path = writer::write_review(dir, pr, Some(&summary))?;
                                        println!("   💾 Saved to {}", path.display());

                                        // Automatically open in IntelliJ (like auto-open for PRs)
                                        println!("   🎯 Opening Claude review in IntelliJ...");
                                        open_in_intellij(&path)?;
                                    }
                                }
                                Err(e) => println!("❌ Failed to delegate: {}", e),
                            }
                        }
                        "o" | "open" => {
                            open_in_browser(&pr.pr_url)?;
                        }
                        "i" | "intellij" => {
                            // We'll create a temporary file and open it
                            let temp_dir = PathBuf::from("./reviews");
                            std::fs::create_dir_all(&temp_dir)?;
                            let temp_path = temp_dir.join(format!("temp-pr-{}.md", pr.pr_number));
                            writer::write_review(&temp_dir, pr, None)?;
                            open_in_intellij(&temp_path)?;
                        }
                        "s" | "skip" => {
                            println!("⏭️  Skipping PR #{}", pr.pr_number);
                        }
                        "q" | "quit" => {
                            println!("👋 Exiting interactive mode, continuing monitoring...");
                            break;
                        }
                        _ => {
                            println!("❓ Unknown choice: {}", choice);
                        }
                    }
                }
            }
        }

        last_pr_count = current_count;

        // Check if we should continue running
        if !running.load(Ordering::SeqCst) {
            println!("🛑 Monitor stopped");
            break;
        }

        // Wait for the specified interval
        println!("Waiting {} seconds before next check...", interval_seconds);
        tokio::time::sleep(Duration::from_secs(interval_seconds)).await;
    }

    // Clean up when exiting normally
    let _ = remove_pid_file();
    Ok(())
}
