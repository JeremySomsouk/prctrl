use anyhow::Result;
use futures::future::join_all;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

static TICKET_KEY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)\b([A-Z][A-Z0-9]*-\d+)\b").expect("valid ticket-key regex")
});
static POSITION_INDEX: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"[\[\(](\d+)/(\d+)[\]\)]").expect("valid position-index regex")
});

#[derive(Clone, Debug, serde::Serialize)]
pub struct StackedPR {
    pub number: u64,
    pub title: String,
    pub repo: String,
    pub head_branch: String,
    pub base_branch: String,
    pub head_sha: String,
    pub base_sha: String,
    pub position: usize,
    pub url: String,
    pub author: String,
    pub draft: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Stack {
    pub base_branch: String,
    pub repo: String,
    pub prs: Vec<StackedPR>,
    pub kind: StackKind,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub enum StackKind {
    /// PRs chained by commit graph: PR B's base SHA == PR A's head SHA
    CommitChain,
    /// PRs chained by branch: PR B targets PR A's head branch
    BranchChain,
    /// PRs grouped by ticket prefix + [N/M] markers in title
    Convention,
}

/// Detect stacked PRs using commit-graph, branch-chaining, and convention-based grouping.
/// Priority: commit-chain (most reliable) > branch-chain > convention.
pub async fn detect_stacks(
    token: &str,
    org: &str,
    repos: &[String],
    author: Option<&str>,
    repo_filter: Option<&str>,
    include_drafts: bool,
    limit: Option<u32>,
) -> Result<Vec<Stack>> {
    let all_prs = fetch_all_prs(token, org, repos, author, repo_filter, include_drafts).await?;

    if all_prs.is_empty() {
        return Ok(Vec::new());
    }

    let mut stacks = Vec::new();
    let mut used_prs: HashSet<(String, u64)> = HashSet::new();

    // 1. Detect commit-chain stacks (primary — uses SHA comparison)
    let commit_stacks = detect_commit_chain_stacks(&all_prs);
    for stack in &commit_stacks {
        for pr in &stack.prs {
            used_prs.insert((pr.repo.clone(), pr.number));
        }
    }
    stacks.extend(commit_stacks);

    // 2. Detect branch-chaining stacks (fallback — uses branch name comparison)
    let branch_stacks = detect_branch_chain_stacks(&all_prs, &used_prs);
    for stack in &branch_stacks {
        for pr in &stack.prs {
            used_prs.insert((pr.repo.clone(), pr.number));
        }
    }
    stacks.extend(branch_stacks);

    // 3. Detect convention-based stacks (ticket prefix + [N/M] markers)
    let convention_stacks = detect_convention_stacks(&all_prs, &used_prs);
    for stack in &convention_stacks {
        for pr in &stack.prs {
            used_prs.insert((pr.repo.clone(), pr.number));
        }
    }
    stacks.extend(convention_stacks);

    // HashMap iteration order must not change the displayed stacks or --limit results.
    sort_stacks(&mut stacks);
    if let Some(l) = limit {
        stacks.truncate(l as usize);
    }

    Ok(stacks)
}

fn sort_stacks(stacks: &mut [Stack]) {
    stacks.sort_by(|a, b| {
        a.repo
            .cmp(&b.repo)
            .then_with(|| a.base_branch.cmp(&b.base_branch))
            .then_with(|| {
                a.prs
                    .first()
                    .map(|p| p.number)
                    .cmp(&b.prs.first().map(|p| p.number))
            })
    });
}

/// Detect stacks where PR B's base SHA equals PR A's head SHA.
/// This is the most reliable method because it uses the actual commit graph.
fn detect_commit_chain_stacks(all_prs: &[StackedPR]) -> Vec<Stack> {
    let mut stacks = Vec::new();

    // Group PRs by repo (chains can't cross repos)
    let mut by_repo: HashMap<&str, Vec<&StackedPR>> = HashMap::new();
    for pr in all_prs {
        if pr.head_sha.is_empty() || pr.base_sha.is_empty() {
            continue;
        }
        by_repo.entry(&pr.repo).or_default().push(pr);
    }

    for (repo, prs) in by_repo {
        // Build lookup: head_sha -> PR
        let mut head_to_pr: HashMap<&str, &StackedPR> = HashMap::new();
        for pr in &prs {
            head_to_pr.insert(&pr.head_sha, pr);
        }

        // Find roots: PRs whose base_sha is NOT any other PR's head_sha
        let mut roots: Vec<&StackedPR> = Vec::new();
        for pr in &prs {
            if !head_to_pr.contains_key(&pr.base_sha.as_str()) {
                roots.push(pr);
            }
        }

        // Sort roots by PR number for deterministic output
        roots.sort_by_key(|p| p.number);

        let mut used_prs: HashSet<u64> = HashSet::new();

        for root in roots {
            let mut chain = vec![(*root).clone()];
            used_prs.insert(root.number);

            // Walk down the chain: find PR whose base_sha == last PR's head_sha
            loop {
                let last = chain.last().unwrap();
                if let Some(child) = prs.iter().find(|p| {
                    p.base_sha == last.head_sha
                        && !used_prs.contains(&p.number)
                        && p.number != last.number
                }) {
                    used_prs.insert(child.number);
                    chain.push((*child).clone());
                } else {
                    break;
                }
            }

            if chain.len() >= 2 {
                let base_branch = chain.first().unwrap().base_branch.clone();
                stacks.push(build_stack(
                    repo,
                    &base_branch,
                    chain,
                    StackKind::CommitChain,
                ));
            }
        }
    }

    stacks
}

/// Fetch all open PRs across configured repos using octocrab
async fn fetch_all_prs(
    token: &str,
    org: &str,
    repos: &[String],
    author: Option<&str>,
    repo_filter: Option<&str>,
    include_drafts: bool,
) -> Result<Vec<StackedPR>> {
    let repo_futures = repos.iter().map(|repo_name| {
        let client = crate::github::new_client(token);
        let repo_name = repo_name.clone();
        async move {
            let mut all_prs = Vec::new();
            let first_page = client
                .pulls(org, &repo_name)
                .list()
                .state(octocrab::params::State::Open)
                .per_page(100)
                .send()
                .await?;

            all_prs.extend(first_page.items);

            let mut next_page = first_page.next;
            while next_page.is_some() {
                match client.get_page(&next_page).await {
                    Ok(Some(page)) => {
                        next_page = page.next.clone();
                        all_prs.extend(page.items);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("Warning: Failed to fetch next page: {}", e);
                        break;
                    }
                }
            }

            Ok::<(String, Vec<_>), octocrab::Error>((repo_name, all_prs))
        }
    });

    let repo_results: Vec<(String, Vec<_>)> = join_all(repo_futures)
        .await
        .into_iter()
        .filter_map(|result| match result {
            Ok((repo, items)) => Some((repo, items)),
            Err(e) => {
                eprintln!("Warning: Failed to fetch PRs from a repo: {}", e);
                None
            }
        })
        .collect();

    let mut all_prs: Vec<StackedPR> = Vec::new();

    for (repo_name, prs) in repo_results {
        if let Some(filter) = repo_filter {
            if !repo_name.to_lowercase().contains(&filter.to_lowercase()) {
                continue;
            }
        }

        for pr in prs {
            let pr_author = pr.user.as_ref().map(|u| u.login.as_str()).unwrap_or("");

            if let Some(author_filter) = author {
                if pr_author.to_lowercase() != author_filter.to_lowercase() {
                    continue;
                }
            }

            if !include_drafts && pr.draft.unwrap_or(false) {
                continue;
            }

            let head_branch = pr.head.ref_field.clone();
            let base_branch = pr.base.ref_field.clone();
            let head_sha = pr.head.sha.clone();
            let base_sha = pr.base.sha.clone();
            let title = pr.title.clone().unwrap_or_default();
            let url = pr.html_url.map(|u| u.to_string()).unwrap_or_default();

            all_prs.push(StackedPR {
                number: pr.number,
                title,
                repo: repo_name.clone(),
                head_branch,
                base_branch,
                head_sha,
                base_sha,
                position: 0,
                url,
                author: pr_author.to_string(),
                draft: pr.draft.unwrap_or(false),
            });
        }
    }

    all_prs.sort_by(|a, b| a.repo.cmp(&b.repo).then_with(|| a.number.cmp(&b.number)));

    Ok(all_prs)
}

/// Detect stacks where a PR's base branch is another PR's head branch.
/// Uses a root-first approach: finds chain roots (PRs whose base is NOT another PR's head),
/// then walks down each chain to build the full stack without overlaps.
fn detect_branch_chain_stacks(
    all_prs: &[StackedPR],
    already_used: &HashSet<(String, u64)>,
) -> Vec<Stack> {
    let mut base_to_children: HashMap<(String, String), Vec<&StackedPR>> = HashMap::new();
    let mut head_to_pr: HashMap<(String, String), &StackedPR> = HashMap::new();

    for pr in all_prs {
        if already_used.contains(&(pr.repo.clone(), pr.number)) {
            continue;
        }
        let head_key = (pr.repo.clone(), pr.head_branch.clone());
        let base_key = (pr.repo.clone(), pr.base_branch.clone());
        head_to_pr.insert(head_key, pr);
        base_to_children.entry(base_key).or_default().push(pr);
    }

    // Find roots: PRs whose base_branch is NOT another PR's head_branch
    let mut roots: Vec<&StackedPR> = Vec::new();
    for pr in all_prs {
        if already_used.contains(&(pr.repo.clone(), pr.number)) {
            continue;
        }
        let base_key = (pr.repo.clone(), pr.base_branch.clone());
        if !head_to_pr.contains_key(&base_key) {
            roots.push(pr);
        }
    }

    roots.sort_by_key(|p| p.number);

    let mut stacks = Vec::new();
    let mut used_prs: HashSet<(String, u64)> = HashSet::new();

    for root in roots {
        let mut chain = vec![(*root).clone()];
        used_prs.insert((root.repo.clone(), root.number));

        loop {
            let last = chain.last().unwrap();
            let head_key = (last.repo.clone(), last.head_branch.clone());
            if let Some(children) = base_to_children.get(&head_key) {
                if let Some(child) = children.iter().find(|c| {
                    !used_prs.contains(&(c.repo.clone(), c.number)) && c.number != last.number
                }) {
                    used_prs.insert((child.repo.clone(), child.number));
                    chain.push((*child).clone());
                    continue;
                }
            }
            break;
        }

        if chain.len() >= 2 {
            let base_branch = chain.first().unwrap().base_branch.clone();
            let repo = chain.first().unwrap().repo.clone();
            stacks.push(build_stack(
                &repo,
                &base_branch,
                chain,
                StackKind::BranchChain,
            ));
        }
    }

    stacks
}

/// Extract a ticket key from a branch name or title.
fn extract_ticket_key(s: &str) -> Option<String> {
    let caps = TICKET_KEY.captures(s)?;
    let key = caps.get(1)?.as_str().to_uppercase();
    Some(key)
}

/// Extract the position index from title markers like [1/3], (2/5), [3/3], etc.
fn extract_position_index(title: &str) -> Option<usize> {
    let caps = POSITION_INDEX.captures(title)?;
    caps.get(1)?.as_str().parse::<usize>().ok()
}

/// Detect convention-based stacks: PRs sharing the same ticket key
/// with [N/M] position markers in the title
fn detect_convention_stacks(
    all_prs: &[StackedPR],
    already_used: &HashSet<(String, u64)>,
) -> Vec<Stack> {
    let mut ticket_groups: HashMap<(String, String), Vec<&StackedPR>> = HashMap::new();

    for pr in all_prs {
        if already_used.contains(&(pr.repo.clone(), pr.number)) {
            continue;
        }

        let ticket_key =
            extract_ticket_key(&pr.head_branch).or_else(|| extract_ticket_key(&pr.title));

        if let Some(key) = ticket_key {
            ticket_groups
                .entry((pr.repo.clone(), key))
                .or_default()
                .push(pr);
        }
    }

    let mut stacks = Vec::new();

    for ((repo, ticket_key), mut prs) in ticket_groups {
        if prs.len() < 2 {
            continue;
        }

        let has_position_marker = prs
            .iter()
            .any(|p| extract_position_index(&p.title).is_some());
        let all_same_base = prs.windows(2).all(|w| w[0].base_branch == w[1].base_branch);

        if !has_position_marker && !all_same_base {
            continue;
        }

        prs.sort_by(|a, b| {
            let pos_a = extract_position_index(&a.title).unwrap_or(0);
            let pos_b = extract_position_index(&b.title).unwrap_or(0);
            pos_a.cmp(&pos_b).then_with(|| a.number.cmp(&b.number))
        });

        let stacked_prs: Vec<StackedPR> = prs.into_iter().cloned().collect();
        let base_branch = stacked_prs
            .first()
            .map(|p| p.base_branch.clone())
            .unwrap_or_default();

        stacks.push(build_stack_with_key(
            &repo,
            &base_branch,
            &ticket_key,
            stacked_prs,
            StackKind::Convention,
        ));
    }

    stacks
}

fn build_stack(repo: &str, base_branch: &str, prs: Vec<StackedPR>, kind: StackKind) -> Stack {
    let mut stack = Stack {
        base_branch: base_branch.to_string(),
        repo: repo.to_string(),
        prs,
        kind,
    };
    for (i, pr) in stack.prs.iter_mut().enumerate() {
        pr.position = i + 1;
    }
    stack
}

fn build_stack_with_key(
    repo: &str,
    base_branch: &str,
    _ticket_key: &str,
    prs: Vec<StackedPR>,
    kind: StackKind,
) -> Stack {
    build_stack(repo, base_branch, prs, kind)
}

pub fn render_stacks(stacks: &[Stack]) -> String {
    use colored::*;

    let mut output = String::new();

    if stacks.is_empty() {
        output.push_str("\n🔍 No stacked PRs detected.\n");
        output.push_str("Tip: Stacks are detected via:\n");
        output.push_str("  • Commit chain: PR B's base SHA == PR A's head SHA\n");
        output.push_str("  • Branch chaining: PR B targets PR A's head branch\n");
        output.push_str("  • Convention: same ticket key (e.g. TAHC-1666) with [N/M] markers\n");
        return output;
    }

    output.push_str(&format!("\n📦 Found {} stack(s)\n\n", stacks.len()));

    for stack in stacks {
        let kind_label = match stack.kind {
            StackKind::CommitChain => "commit-chain",
            StackKind::BranchChain => "branch-chain",
            StackKind::Convention => "convention",
        };
        output.push_str(&format!(
            "┌─ Stack on `{}` ({} PRs, {})\n",
            stack.base_branch,
            stack.prs.len(),
            kind_label
        ));

        for pr in &stack.prs {
            let prefix = if pr.position == 1 { "🔵" } else { "  " };
            let draft_tag = if pr.draft { " [DRAFT]" } else { "" };
            output.push_str(&format!(
                "{} [{}/{}] #{} - {}{}\n  └─ {} \n    {}\n",
                prefix,
                pr.position,
                stack.prs.len(),
                pr.number,
                pr.title.bold(),
                draft_tag,
                pr.head_branch.dimmed(),
                pr.url.blue().underline()
            ));
        }
        output.push('\n');
    }

    output
}

/// Render stacks as a tree with Unicode box-drawing characters.
pub fn render_stacks_tree(stacks: &[Stack]) -> String {
    use colored::*;

    let mut output = String::new();

    if stacks.is_empty() {
        output.push_str("\n🔍 No stacked PRs detected.\n");
        output.push_str("Tip: Stacks are detected via:\n");
        output.push_str("  • Commit chain: PR B's base SHA == PR A's head SHA\n");
        output.push_str("  • Branch chaining: PR B targets PR A's head branch\n");
        output.push_str("  • Convention: same ticket key (e.g. TAHC-1666) with [N/M] markers\n");
        return output;
    }

    output.push_str(&format!("\n📦 Found {} stack(s)\n\n", stacks.len()));

    for (si, stack) in stacks.iter().enumerate() {
        let kind_label = match stack.kind {
            StackKind::CommitChain => "commit-chain",
            StackKind::BranchChain => "branch-chain",
            StackKind::Convention => "convention",
        };

        output.push_str(&format!(
            "┌─ Stack on `{}` ({} PRs, {})\n",
            stack.base_branch,
            stack.prs.len(),
            kind_label
        ));

        for (i, pr) in stack.prs.iter().enumerate() {
            let is_last = i == stack.prs.len() - 1;
            let connector = if is_last { "└─" } else { "├─" };
            let vertical = if is_last { "  " } else { "│ " };
            let draft_tag = if pr.draft { " [DRAFT]" } else { "" };

            output.push_str(&format!(
                "{} #{} — {}{}\n",
                connector,
                pr.number,
                pr.title.bold(),
                draft_tag,
            ));
            output.push_str(&format!(
                "{}  └─ {} {}\n",
                vertical,
                pr.head_branch.dimmed(),
                pr.url.blue().underline(),
            ));

            if !is_last {
                output.push_str(&format!("{}  \n", vertical));
            }
        }

        if si < stacks.len() - 1 {
            output.push('\n');
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convention_extractors_preserve_matching_rules() {
        assert_eq!(
            extract_ticket_key("refactor(proj-123): extract [1/3]"),
            Some("PROJ-123".into())
        );
        assert_eq!(extract_ticket_key("just-a-branch"), None);
        assert_eq!(extract_position_index("part (2/5)"), Some(2));
        assert_eq!(extract_position_index("no marker"), None);
    }

    #[test]
    fn stack_order_does_not_depend_on_group_iteration() {
        let mut stacks = vec![
            build_stack("z-repo", "main", vec![], StackKind::Convention),
            build_stack("a-repo", "release", vec![], StackKind::BranchChain),
            build_stack("a-repo", "main", vec![], StackKind::CommitChain),
        ];
        sort_stacks(&mut stacks);
        let expected = render_stacks(&stacks);
        stacks.reverse();
        sort_stacks(&mut stacks);
        assert_eq!(render_stacks(&stacks), expected);
        assert_eq!(
            (&*stacks[0].repo, &*stacks[0].base_branch),
            ("a-repo", "main")
        );
    }
}
