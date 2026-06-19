use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "prctrl",
    version,
    about = "Terminal-native GitHub PR management. Stay on top of code reviews without leaving your terminal.",
    author = "Jeremy Somsouk <jeremy@somsouk.fr>"
)]
pub struct Cli {
    /// Folder where review files will be written (default: ./reviews)
    #[arg(long, short, global = true)]
    pub output_dir: Option<PathBuf>,

    /// Path to custom instruction file (default: searches in multiple locations)
    #[arg(long, global = true)]
    pub instruction_path: Option<PathBuf>,

    /// Open a new terminal tab in output_dir after running
    #[arg(long, global = true)]
    pub open_terminal: bool,

    /// Include PRs you authored
    #[arg(long, global = true)]
    pub include_mine: bool,

    /// Include draft PRs
    #[arg(long, short = 'd', global = true)]
    pub include_drafts: bool,

    /// Only show open PRs from crew members (CREW_MEMBERS in .env)
    #[arg(long, short = 'c', global = true)]
    pub crew: bool,

    /// Show stacked PRs (detects PRs that build on each other)
    #[arg(long, global = true)]
    pub show_stacks: bool,

    /// Exclude PRs whose title matches these prefixes (comma-separated, default: "chore(deps)")
    #[arg(
        long,
        global = true,
        value_delimiter = ',',
        default_value = "chore(deps)"
    )]
    pub exclude_prefix: Vec<String>,

    /// Target a specific PR by number (bypasses review-request filters)
    #[arg(long, short = 'p', global = true)]
    pub pr: Option<u64>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// List all PRs waiting for your review
    #[command(alias = "ls")]
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show specific PR by number (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Show PRs where the user has already commented
        #[arg(long)]
        commented: bool,
        /// Show specific PR(s) by number (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
    },
    /// Ask Claude to triage each pending review
    #[command(alias = "del")]
    Delegate {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Preview delegation without executing (show what would be delegated)
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Only delegate PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Delegate all matching PRs at once (without prompting)
        #[arg(long, short = 'a')]
        all: bool,
        /// Suppress per-PR progress messages (show only summary)
        #[arg(long, short = 'q')]
        quiet: bool,
        /// PR number(s) to delegate (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
    },
    /// List your own open PRs (draft or not)
    Mine {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show all matching PRs at once (without prompting)
        #[arg(long, short = 'a')]
        all: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Target a specific PR by number
        #[arg(long, short = 'p')]
        pr_number: Option<u64>,
        /// PR number(s) to show mine for (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
    },
    /// Show review statistics (pending count, avg wait time, breakdown by repo)
    #[command(alias = "stat")]
    Stats {
        /// Show stats for all pending reviews (no interactive selection)
        #[arg(long, short = 'a')]
        all: bool,
        /// PR number(s) to show stats for (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// PR number to show stats for (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Preview which PRs would be included without showing stats
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
    },
    /// Show team review summary (how many PRs each crew member has waiting)
    #[command(alias = "team")]
    TeamSummary {
        /// Show team summary for all pending reviews (no interactive selection)
        #[arg(long, short = 'a')]
        all: bool,
        /// Target a specific PR by number (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to show team summary for (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Target a specific PR by number
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
    },
    /// Show review workload distribution across team members (load balance analysis)
    Load {
        /// Minimum number of PRs to be considered "loaded" (default: 3)
        #[arg(long, short)]
        threshold: Option<u32>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Target a specific PR by number
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// PR number(s) to show load for (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove all past review files from the output directory
    Clean {
        /// Preview what would be deleted without actually deleting
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Suppress all output except errors
        #[arg(long, short = 'q')]
        quiet: bool,
    },
    /// Monitor for new PRs and send macOS notifications
    Monitor {
        /// Polling interval in seconds (default: 300)
        #[arg(long, short = 'i', default_value_t = 300)]
        interval: u64,
        /// Send macOS notifications for new PRs
        #[arg(long)]
        notify: bool,
        /// Automatically open PRs in Chrome when notifications appear
        #[arg(long)]
        auto_open: bool,

        /// Disable automatic opening of PRs in Chrome
        #[arg(long)]
        no_auto_open: bool,
        /// Interactive mode - prompt for actions on new PRs
        #[arg(long, default_value_t = true)]
        interactive: bool,
    },
    /// Stop the running monitor process
    MonitorStop,
    /// Check if monitor process is running
    MonitorStatus,
    /// Launch the Terminal User Interface
    Tui {
        /// Refresh interval in seconds (default: 30)
        #[arg(long, short = 'i', default_value_t = 30)]
        interval: u64,
    },
    /// Show diff/stats for a specific PR directly in terminal
    Diff {
        /// PR number to show diff for
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to show diff for (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Show diff for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Show diff/stats for all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
        /// Preview which PRs would be diffed without actually displaying the diff
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
    },
    /// Show full PR information including description, reviewers, and metadata
    Info {
        /// PR number to show info for (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Show info for all pending reviews (no interactive selection)
        #[arg(long, short = 'a')]
        all: bool,
        /// PR number(s) to show info for (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Show info for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
    },
    /// Show the chronological timeline of events on a PR (reviews, comments, labels, CI, etc.)
    Timeline {
        /// PR number to show timeline for (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show timeline for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// PR number(s) to show timeline for (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Show timeline for all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
        /// Preview which PRs would be shown without actually displaying the timeline
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
    },
    /// Assign yourself as a reviewer on a PR
    Assign {
        /// Assign yourself to all pending reviews at once
        #[arg(long, short = 'a')]
        all: bool,
        /// PR number(s) to assign to (comma-separated)
        #[arg(value_name = "PR_NUMBERS")]
        pr_numbers: Option<String>,
        /// PR number to assign yourself to (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Preview what would be assigned without actually assigning
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Suppress per-PR progress messages (show only summary)
        #[arg(long, short = 'q')]
        quiet: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
    },
    /// Remove yourself as a reviewer from a PR
    Unassign {
        /// Unassign yourself from all pending reviews at once
        #[arg(long, short = 'a')]
        all: bool,
        /// PR number(s) to unassign from (comma-separated)
        #[arg(value_name = "PR_NUMBERS")]
        pr_numbers: Option<String>,
        /// PR number to unassign yourself from (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Preview what would be unassigned without actually removing
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Suppress per-PR progress messages (show only summary)
        #[arg(long, short = 'q')]
        quiet: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
    },
    /// Post a comment on a PR directly from the CLI
    Comment {
        /// Comment on all pending reviews at once
        #[arg(long, short = 'a')]
        all: bool,
        /// PR number(s) to comment on (comma-separated)
        #[arg(value_name = "PR_NUMBERS")]
        pr_numbers: Option<String>,
        /// PR number to comment on
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Comment text (supports markdown)
        #[arg(long, short = 't', value_name = "TEXT")]
        text: String,
        /// Preview what would be commented without actually posting
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Suppress per-PR progress messages (show only summary)
        #[arg(long, short = 'q')]
        quiet: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
    },
    /// Approve a PR directly from the CLI
    Approve {
        /// Approve all pending reviews at once
        #[arg(long, short = 'a')]
        all: bool,
        /// PR number(s) to approve (comma-separated)
        #[arg(value_name = "PR_NUMBERS")]
        pr_numbers: Option<String>,
        /// PR number to approve
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Approve with a custom comment
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Only approve PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Preview what would be approved without actually approving
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Suppress per-PR progress messages (show only summary)
        #[arg(long, short = 'q')]
        quiet: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
    },
    /// Claim multiple PRs for review at once
    Claim {
        /// Claim all pending reviews at once
        #[arg(long, short = 'a')]
        all: bool,
        /// PR number(s) to claim (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Preview what would be claimed without taking action
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Suppress per-PR progress messages (show only summary)
        #[arg(long, short = 'q')]
        quiet: bool,
    },
    /// Open one or more PRs in your browser
    Browse {
        /// PR number to open (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to open (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Open specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Open all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
        /// Preview which PRs would be opened without actually opening them
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Suppress per-PR success/failure messages (show only summary)
        #[arg(long, short = 'q')]
        quiet: bool,
        /// Output URLs as JSON (useful for scripting)
        #[arg(long)]
        json: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
    },
    /// Show changed files for one or more PRs
    Files {
        /// PR number to show files for (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to show files for (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Show files for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Show files for all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
    },
    /// Generate a weekly review report from processed review files
    Report {
        /// PR number to show report for (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to include in report (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Include all pending reviews in the report
        #[arg(long, short = 'a')]
        all: bool,
        /// Number of days to look back (default: 7)
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Only show pending PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
    },
    /// Search pending reviews by title keyword
    Search {
        /// Keyword to search for in PR titles
        #[arg(value_name = "KEYWORD")]
        query: String,
        /// PR number(s) to search within (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// PR number to search within (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Sort results by: priority, age, size, or title (default: priority)
        #[arg(long, value_name = "FIELD", default_value = "priority")]
        sort_by: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Filter pending reviews by various criteria
    Filter {
        /// PR number to filter to (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to filter to (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Apply filters to all pending reviews (no interactive selection)
        #[arg(long, short = 'a')]
        all: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Minimum total lines changed (+additions -deletions)
        #[arg(long)]
        min_size: Option<u64>,
        /// Maximum total lines changed
        #[arg(long)]
        max_size: Option<u64>,
        /// Minimum age in days
        #[arg(long)]
        min_age: Option<u32>,
        /// Maximum age in days
        #[arg(long)]
        max_age: Option<u32>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Show only draft PRs
        #[arg(long)]
        drafts_only: bool,
        /// Show only non-draft PRs
        #[arg(long)]
        no_drafts: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show CI/CD pipeline status for pending PRs (GitHub Actions, etc.)
    Ci {
        /// Only show PRs with failing checks
        #[arg(long, short = 'f')]
        failed_only: bool,
        /// Only show PRs with passing checks
        #[arg(long)]
        passing_only: bool,
        /// Show CI status for all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
        /// PR number(s) to check (comma-separated)
        #[arg(value_name = "PR_NUMBERS")]
        pr_numbers: Option<String>,
        /// PR number to check CI for (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Show CI status for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show which pending PRs have merge conflicts
    Conflicts {
        /// Only show PRs with conflicts (hide clean PRs)
        #[arg(long, short = 'c')]
        only_conflicts: bool,
        /// Show conflict status for all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
        /// PR number(s) to check for conflicts (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// PR number to check conflicts for (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Show conflict status for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show labels for one or more PRs
    Labels {
        /// PR number to show labels for (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to show labels for (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Show labels for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Show labels for all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
        /// Filter pending reviews by label name (partial match, case-insensitive)
        #[arg(long, short = 'l')]
        filter_by: Option<String>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show your recent review activity (PRs you reviewed in the last N days)
    Activity {
        /// Number of days to look back (default: 7)
        #[arg(long, short = 'd', default_value_t = 7)]
        days: u32,
        /// PR number(s) to show activity for (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Show activity for all pending reviews (no interactive selection)
        #[arg(long, short = 'a')]
        all: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show activity for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
    },
    /// Show GitHub notifications where you were mentioned or directly involved
    Mentions {
        /// Only show unread notifications
        #[arg(long, short = 'u')]
        unread_only: bool,
        /// Limit the number of results shown (default: 20)
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Filter to specific PR number
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Only show notifications from the last N days
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show a quick one-line summary of pending reviews (total, oldest age, lines, urgency breakdown)
    Summary {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
    },
    /// Analyze which PRs demand your immediate attention based on multiple urgency factors
    Attention {
        /// Only show PRs with attention score >= threshold (1-10, default: 5)
        #[arg(long, short = 't')]
        threshold: Option<u8>,
        /// Show detailed breakdown of why each PR scored high
        #[arg(long, short = 'd')]
        detailed: bool,
        /// Limit the number of results shown (default: 10)
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show the ONE PR you should focus on right now — the most urgent by priority score
    Focus {
        /// Preview which PR would be selected without opening it
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Show all matching PRs sorted by priority (default: show only the top 1)
        #[arg(long, short = 'a')]
        all: bool,
        /// Limit the number of PRs shown (default: 1, use with --all)
        #[arg(long, short = 'l')]
        limit: Option<usize>,
        /// Open the focused PR in your browser instead of printing details
        #[arg(long, short = 'o')]
        open: bool,
        /// PR number to focus on (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to focus on (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Target specific PR by number (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
    },
    /// Show GitHub API health status and rate limits
    Health {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show actionable recommendations based on rate limits
        #[arg(long, short = 'S')]
        suggest: bool,
    },
    /// Show your highest priority pending PRs based on age, size, and urgency
    Top {
        /// Limit the number of results shown (default: 10)
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Minimum priority score threshold (1-5, default: 3)
        #[arg(long)]
        min_score: Option<u8>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long)]
        since_days: Option<u32>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show "quick win" PRs — small, non-draft PRs you can review quickly
    Quick {
        /// Maximum total lines for a "quick" PR (default: 200)
        #[arg(long, short = 'l')]
        max_lines: Option<u64>,
        /// Limit the number of results shown (default: 10)
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show PRs you should catch up on — oldest, longest-ignored, sorted by neglect
    Catchup {
        /// PR number to show catchup for (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Minimum age in days to be considered "catchup" (default: 3)
        #[arg(long, default_value_t = 3)]
        min_age: u32,
        /// Limit the number of results shown (default: 10)
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Show all neglected PRs without limit (no truncation)
        #[arg(long, short = 'a')]
        all: bool,
    },
    /// Categorize pending PRs by age brackets (new/aging/stale/overdue) with visual buckets
    Age {
        /// Show only PRs newer than this many days
        #[arg(long, short = 'n')]
        min_days: Option<u32>,
        /// Show only PRs older than this many days
        #[arg(long, short = 'x')]
        older_than: Option<u32>,
        /// Group output by age bucket instead of flat list
        #[arg(long, short = 'g', default_value_t = true)]
        grouped: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Categorize pending PRs by size (XS/S/M/L/XL) with statistics and visual breakdown
    Size {
        /// Show only PRs of specific size(s) - XS, S, M, L, XL (comma-separated)
        #[arg(long, short = 'f')]
        filter_size: Option<String>,
        /// Group output by size bucket instead of flat list
        #[arg(long, short = 'g', default_value_t = true)]
        grouped: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Fetch and display a PR diff in the terminal with syntax highlighting
    Review {
        /// PR number to review (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to review (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Show diff for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Show diffs for all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
        /// Preview which PRs would be shown without actually displaying the diff
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Number of context lines around changes (default: 3)
        #[arg(long, short = 'C', default_value_t = 3)]
        context: u8,
        /// Output diff to file instead of terminal
        #[arg(long, short = 'o')]
        output_file: Option<PathBuf>,
        /// Language hint for syntax highlighting (auto-detected if not specified)
        #[arg(long, short = 'l')]
        language: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Generate a shareable weekly digest (markdown, perfect for Slack/Teams)
    Digest {
        /// Number of days to include (default: 7)
        #[arg(long, short = 'd', default_value_t = 7)]
        days: u32,
        /// Output as raw Markdown (no preamble)
        #[arg(long)]
        raw: bool,
        /// Preview digest without generating (show what would be included)
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Only show PRs created in the last N days (newer than N days old)
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Only show PRs older than N days (ignored if --since-days is also set)
        #[arg(long)]
        older_than: Option<u32>,
    },
    /// Analyze review trends over time (velocity, avg time to review, top reviewers)
    Trends {
        /// Number of days to analyze (default: 30)
        #[arg(long, short = 'd', default_value_t = 30)]
        days: u32,
        /// Number of top authors/reviewers to show (default: 10)
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
    },
    /// Analyze how quickly PRs get reviewed (avg time-to-first-review, bottleneck detection)
    ReviewVelocity {
        /// Number of days to look back (default: 30)
        #[arg(long, short = 'd', default_value_t = 30)]
        days: u32,
        /// Show bottleneck analysis (which repos/authors take longest)
        #[arg(long, short = 'b')]
        bottlenecks: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show velocity for specific PR (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Show velocity for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
    },
    /// Temporarily hide PRs from the pending list (snooze them)
    Snooze {
        /// Snooze action: add, list, remove, or clear
        #[command(subcommand)]
        action: SnoozeAction,
        /// PR number to snooze (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Snooze specific PR (use with --send to snooze a single PR)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// PR number(s) to snooze (comma-separated)
        #[arg(value_name = "PR_NUMBERS")]
        pr_numbers: Option<String>,
        /// Snooze duration in days (default: 3)
        #[arg(long, short = 'd')]
        days: Option<u32>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
    },
    /// Send a follow-up reminder to authors of stale PRs to get their attention
    Chase {
        /// PR number to chase (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Chase specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// PR number(s) to chase (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Minimum age in days to chase (default: 7)
        #[arg(long, default_value_t = 7)]
        min_age: u32,
        /// Only chase PRs created since this many days ago (newer PRs)
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Preview chase comments without posting them (default behavior shows preview)
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Actually post chase comments to GitHub (default is to preview)
        #[arg(long)]
        send: bool,
        /// Custom message template (use {author} and {title} as placeholders)
        #[arg(long, short = 'm')]
        message: Option<String>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Suppress per-PR progress messages (show only summary)
        #[arg(long, short = 'q')]
        quiet: bool,
    },
    /// Estimate review time for pending PRs based on size and complexity
    ReviewTime {
        /// PR number to estimate review time for (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Show review time for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// PR number(s) to estimate (comma-separated)
        #[arg(value_name = "PR_NUMBERS")]
        pr_numbers: Option<String>,
        /// Show review time for all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
        /// Group output by time category (lightning/quick/moderate/substantial/lengthy)
        #[arg(long, short = 'g')]
        grouped: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Export pending reviews to CSV or Markdown table format
    Export {
        /// Export format: csv or markdown (default: csv)
        #[arg(long, short = 'f', value_name = "FORMAT")]
        format: Option<String>,
        /// Output file path (stdout if not specified)
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
        /// Include only these columns (comma-separated): repo,number,title,author,size,age,draft,url
        #[arg(long, short = 'c')]
        columns: Option<String>,
        /// PR number to export (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to export (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Export specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Export all pending reviews (not just current session)
        #[arg(long, short = 'a')]
        all: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
    },
    /// Search and filter your review history from processed files
    History {
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Filter by review state (APPROVED, CHANGES_REQUESTED, COMMENTED)
        #[arg(long, short = 's')]
        state: Option<String>,
        /// Number of days to look back (default: 30)
        #[arg(long, short = 'd', default_value_t = 30)]
        days: u32,
        /// Limit the number of results shown (default: 50)
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show PRs that are ready to merge (approved, CI passing, no conflicts)
    Ready {
        /// Show PRs ready for specific repo
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show PRs ready for specific PR (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Show PRs ready for specific PR (shorthand for --pr)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// PR number(s) to check (comma-separated)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// Show ready status for all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
    },
    /// Compare two PRs side-by-side to help decide which to review first
    Compare {
        /// First PR number to compare (in format "repo#123" or just "123" for first repo)
        #[arg(value_name = "PR1")]
        pr1: String,
        /// Second PR number to compare (in format "repo#123" or just "123" for first repo)
        #[arg(value_name = "PR2")]
        pr2: String,
        /// Show detailed comparison including file-level breakdown
        #[arg(long, short = 'd')]
        detailed: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Watch PRs for status changes (CI passing, approved, conflicts resolved, etc.)
    Follow {
        /// Follow action: add, list, remove, clear, or status
        #[command(subcommand)]
        action: FollowAction,
        /// PR number to follow (shorthand for --pr-numbers with single value)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// PR number(s) to follow (comma-separated, for add/remove)
        #[arg(long)]
        pr_numbers: Option<String>,
        /// PR number to follow (shorthand for --pr-numbers with single value)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
    },
    /// Show PRs that are blocked from merging (CI failures, conflicts, or other issues)
    Blocked {
        /// Filter to specific repository
        #[arg(long, short = 'r')]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Only show PRs with failing CI
        #[arg(long, short = 'c')]
        ci_only: bool,
        /// Only show PRs with merge conflicts
        #[arg(long, short = 'm')]
        conflicts_only: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
        /// Limit the number of results shown (default: 20)
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Send emoji reactions to PR authors to get their attention (non-intrusive nudge)
    Ping {
        /// Reaction emoji to send: eyes, rocket, heart, +1 (default: eyes)
        #[arg(long, short = 'e', value_name = "EMOJI", default_value = "eyes")]
        emoji: String,
        /// PR number(s) to ping (comma-separated)
        #[arg(value_name = "PR_NUMBERS")]
        pr_numbers: Option<String>,
        /// PR number to ping (shorthand for --pr)
        #[arg(value_name = "PR_NUMBER")]
        pr_number: Option<u64>,
        /// Ping specific PR (use with --send to ping a single PR)
        #[arg(long, short = 'p')]
        pr: Option<u64>,
        /// Preview what would be pinged without actually sending
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Ping all pending reviews
        #[arg(long, short = 'a')]
        all: bool,
        /// Send actual pings instead of previewing
        #[arg(long)]
        send: bool,
        /// Only show PRs created since this many days ago
        #[arg(long, short = 's')]
        since_days: Option<u32>,
        /// Filter by repository name (partial match, case-insensitive)
        #[arg(long)]
        repo: Option<String>,
        /// Filter by author username (partial match, case-insensitive)
        #[arg(long)]
        author: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Show priority scores for each PR (1-5 stars based on age and size)
        #[arg(long, short = 'P')]
        priority: bool,
    },
    /// Manage PRCtrl configuration settings
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Detect and visualize stacked PRs (sequential dependencies)
    Stack {
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Render as unicode tree
        #[arg(long)]
        tree: bool,
        /// Filter by repository name
        #[arg(long, short = 'r')]
        repo: Option<String>,
        /// Filter by author username
        #[arg(long)]
        author: Option<String>,
        /// Limit number of stacks to show
        #[arg(long, short = 'n')]
        limit: Option<u32>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Create a new configuration file interactively
    Init {
        /// Overwrite existing configuration without prompting
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Display the current configuration file contents
    Show,
    /// Update specific configuration fields without full re-initialization
    Update {
        /// GitHub personal access token
        #[arg(long)]
        token: Option<String>,
        /// GitHub username
        #[arg(long)]
        username: Option<String>,
        /// GitHub organization
        #[arg(long)]
        org: Option<String>,
        /// Comma-separated list of repositories (owner/repo format)
        #[arg(long)]
        repos: Option<String>,
        /// Comma-separated list of GitHub team slugs
        #[arg(long)]
        teams: Option<String>,
        /// Comma-separated list of crew member usernames
        #[arg(long, name = "crew-members")]
        crew: Option<String>,
        /// Fetch crew members dynamically from GitHub teams (requires --teams to be set)
        #[arg(long)]
        fetch_crew: bool,
        /// Anthropic API key
        #[arg(long)]
        api_key: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum FollowAction {
    /// Add PR(s) to your follow list
    Add,
    /// List currently followed PRs and their last known status
    List,
    /// Remove PR(s) from your follow list
    Remove,
    /// Clear all followed PRs
    Clear,
    /// Check for status changes since last check
    Status,
}

#[derive(Subcommand, Debug)]
pub enum SnoozeAction {
    /// Add PR(s) to the snooze list
    Add,
    /// List currently snoozed PRs
    List,
    /// Show detailed info about snoozed PRs (author, age, lines changed, priority)
    Review,
    /// Remove PR(s) from the snooze list
    Remove,
    /// Clear all snoozed PRs
    Clear,
    /// Remove expired snooze entries (PRs whose snooze time has passed)
    Expire,
    /// Extend the snooze duration for already-snoozed PRs
    Extend,
}
