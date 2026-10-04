//! Conservative, read-only readiness evidence shared by the CLI and review desk.
use crate::github::ReviewClient;
use chrono::{DateTime, Utc};
use octocrab::FromResponse;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};

const QUERY: &str = r#"query Readiness($owner:String!,$repo:String!,$number:Int!) {
  rateLimit { remaining resetAt }
  repository(owner:$owner,name:$repo) {
    pullRequest(number:$number) {
      headRefOid isDraft state mergeable mergeStateStatus reviewDecision
      commits(last:1) { nodes { commit { oid statusCheckRollup {
        state contexts(first:100) { totalCount pageInfo { hasNextPage } nodes {
          __typename
          ... on CheckRun { name status conclusion }
          ... on StatusContext { context state }
        } }
      } } } }
    }
  }
}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    Ready,
    Blocked,
    Unknown,
}
impl ReadinessState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "READY",
            Self::Blocked => "BLOCKED",
            Self::Unknown => "UNKNOWN",
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub status: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct Readiness {
    pub state: ReadinessState,
    pub head_sha: Option<String>,
    pub observed_at: DateTime<Utc>,
    pub ci_status: String,
    pub review_decision: String,
    pub merge_state: String,
    pub draft: Option<bool>,
    pub blockers: Vec<String>,
    pub checks: Vec<CheckResult>,
}
impl Readiness {
    pub(crate) fn unknown(reason: &str) -> Self {
        Self {
            state: ReadinessState::Unknown,
            head_sha: None,
            observed_at: Utc::now(),
            ci_status: "unknown".into(),
            review_decision: "unknown".into(),
            merge_state: "unknown".into(),
            draft: None,
            blockers: vec![reason.into()],
            checks: vec![],
        }
    }
}

#[derive(Clone)]
pub(crate) struct ReadinessClient {
    transport: ReviewClient,
    limiter: Arc<Semaphore>,
    cooldown: Arc<Mutex<Option<Instant>>>,
}
impl ReadinessClient {
    pub(crate) fn new(transport: ReviewClient) -> Self {
        Self {
            transport,
            limiter: Arc::new(Semaphore::new(4)),
            cooldown: Arc::new(Mutex::new(None)),
        }
    }
    async fn defer(&self, seconds: u64) {
        let until = Instant::now() + Duration::from_secs(seconds.clamp(60, 86_400));
        let mut cooldown = self.cooldown.lock().await;
        if cooldown.is_none_or(|previous| until > previous) {
            *cooldown = Some(until);
        }
    }
    pub(crate) async fn fetch(&self, owner: &str, repo: &str, number: u64) -> Readiness {
        if number == 0 || number > i32::MAX as u64 {
            return Readiness::unknown("PR number is outside GitHub's supported range");
        }
        let _permit = self
            .limiter
            .acquire()
            .await
            .expect("readiness budget is open");
        if self
            .cooldown
            .lock()
            .await
            .is_some_and(|until| until > Instant::now())
        {
            return Readiness::unknown("GitHub access or rate limit cooldown; retry later");
        }
        let payload =
            json!({"query": QUERY, "variables": {"owner": owner, "repo": repo, "number": number}});
        let request = self.transport.request(async {
            let response = self
                .transport
                .client
                ._post("/graphql", Some(&payload))
                .await?;
            let status = response.status().as_u16();
            let headers = response.headers();
            let retry = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60);
            let exhausted = headers
                .get("x-ratelimit-remaining")
                .is_some_and(|v| v == "0");
            if exhausted || matches!(status, 403 | 429) {
                let reset = headers
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0);
                self.defer(retry.max((reset - Utc::now().timestamp()).max(0) as u64))
                    .await;
            }
            let response = octocrab::map_github_error(response).await?;
            Value::from_response(response).await
        });
        match tokio::time::timeout(Duration::from_secs(30), request).await {
            Ok(Ok(value)) => {
                if value
                    .pointer("/data/rateLimit/remaining")
                    .and_then(Value::as_u64)
                    == Some(0)
                {
                    let seconds = value
                        .pointer("/data/rateLimit/resetAt")
                        .and_then(Value::as_str)
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|date| (date.timestamp() - Utc::now().timestamp()).max(60) as u64)
                        .unwrap_or(60);
                    self.defer(seconds).await;
                }
                if value
                    .get("errors")
                    .and_then(Value::as_array)
                    .is_some_and(|errors| !errors.is_empty())
                {
                    self.defer(60).await;
                }
                parse_response(&value)
            }
            Ok(Err(octocrab::Error::GitHub { source, .. }))
                if matches!(source.status_code.as_u16(), 403 | 429) =>
            {
                self.defer(60).await;
                Readiness::unknown("GitHub denied access or rate limited readiness; retry later")
            }
            Ok(Err(_)) => {
                Readiness::unknown("Could not fetch GitHub readiness; check access and retry")
            }
            Err(_) => Readiness::unknown("GitHub readiness timed out; retry later"),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    head_ref_oid: String,
    is_draft: bool,
    state: String,
    mergeable: String,
    merge_state_status: String,
    review_decision: Option<String>,
    commits: Commits,
}
#[derive(Deserialize)]
struct Commits {
    nodes: Vec<CommitNode>,
}
#[derive(Deserialize)]
struct CommitNode {
    commit: Commit,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    oid: String,
    status_check_rollup: Option<Rollup>,
}
#[derive(Deserialize)]
struct Rollup {
    state: String,
    contexts: Contexts,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Contexts {
    total_count: usize,
    page_info: PageInfo,
    nodes: Vec<Check>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
}
#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum Check {
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
    },
    StatusContext {
        context: String,
        state: String,
    },
}

fn parse_response(value: &Value) -> Readiness {
    if value
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        return Readiness::unknown(
            "GitHub returned partial readiness data; check permissions and retry",
        );
    }
    let Some(pr) = value
        .pointer("/data/repository/pullRequest")
        .filter(|pr| !pr.is_null())
    else {
        return Readiness::unknown(
            "PR readiness unavailable; check repository access and permissions",
        );
    };
    if pr.get("reviewDecision").is_none() {
        return Readiness::unknown("GitHub did not report review policy");
    }
    match serde_json::from_value::<Snapshot>(pr.clone()) {
        Ok(snapshot) => evaluate(snapshot),
        Err(_) => Readiness::unknown("GitHub readiness response is incomplete or unsupported"),
    }
}

pub(crate) fn safe_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(256).collect()
}
fn evaluate(pr: Snapshot) -> Readiness {
    if !matches!(pr.head_ref_oid.len(), 40 | 64)
        || !pr.head_ref_oid.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Readiness::unknown("GitHub did not report a valid head commit");
    }
    let mut result = Readiness::unknown("");
    result.blockers.clear();
    result.head_sha = Some(pr.head_ref_oid.clone());
    result.draft = Some(pr.is_draft);
    result.merge_state = safe_text(&pr.merge_state_status);
    result.review_decision = pr
        .review_decision
        .as_deref()
        .map(safe_text)
        .unwrap_or_else(|| "unknown".into());
    let mut blocked = false;
    let mut unknown = false;
    let mut reason = |certain: bool, message: String| {
        if certain {
            blocked = true;
        } else {
            unknown = true;
        }
        result.blockers.push(message);
    };
    match pr.state.as_str() {
        "OPEN" => {}
        "CLOSED" | "MERGED" => reason(true, "PR is no longer open".into()),
        _ => reason(false, "PR state is unknown".into()),
    }
    if pr.is_draft {
        reason(true, "PR is a draft".into());
    }
    match pr.mergeable.as_str() {
        "MERGEABLE" => {}
        "CONFLICTING" => reason(true, "Merge conflicts".into()),
        _ => reason(false, "GitHub has not established mergeability".into()),
    }
    match pr.merge_state_status.as_str() {
        "CLEAN" => {}
        "BEHIND" => reason(true, "Branch is behind its base".into()),
        "BLOCKED" => reason(true, "GitHub merge policy blocks this PR".into()),
        "DIRTY" => reason(true, "Merge conflicts".into()),
        "DRAFT" => reason(true, "GitHub reports draft merge state".into()),
        "UNSTABLE" => reason(true, "GitHub reports unsuccessful checks".into()),
        _ => reason(
            false,
            "GitHub merge policy is unknown or unsupported".into(),
        ),
    }
    match pr.review_decision.as_deref() {
        Some("APPROVED") => {}
        Some("CHANGES_REQUESTED") => reason(true, "Changes requested in review".into()),
        Some("REVIEW_REQUIRED") => reason(true, "Required reviews are outstanding".into()),
        None if pr.merge_state_status == "CLEAN" => result.review_decision = "not_required".into(),
        _ => reason(false, "Review policy is unknown".into()),
    }
    match pr.commits.nodes.as_slice() {
        [node] if node.commit.oid == pr.head_ref_oid => match &node.commit.status_check_rollup {
            None => reason(false, "No CI evidence reported for the head commit".into()),
            Some(rollup) => {
                let contexts = &rollup.contexts;
                if contexts.page_info.has_next_page || contexts.total_count != contexts.nodes.len()
                {
                    reason(
                        false,
                        "CI evidence is truncated or incomplete (maximum 100 contexts)".into(),
                    );
                }
                if contexts.nodes.is_empty() {
                    reason(false, "No CI evidence reported for the head commit".into());
                }
                match rollup.state.as_str() {
                    "SUCCESS" => result.ci_status = "success".into(),
                    "FAILURE" | "ERROR" => {
                        result.ci_status = "failure".into();
                        reason(true, "CI reports failure".into());
                    }
                    "PENDING" | "EXPECTED" => {
                        result.ci_status = "pending".into();
                        reason(true, "CI has not finished".into());
                    }
                    _ => reason(false, "CI rollup state is unknown".into()),
                }
                for check in &contexts.nodes {
                    let (name, status, passing, certain) = match check {
                        Check::CheckRun {
                            name,
                            status,
                            conclusion,
                        } => {
                            let conclusion = conclusion.as_deref().unwrap_or("UNKNOWN");
                            let passing = status == "COMPLETED"
                                && matches!(conclusion, "SUCCESS" | "NEUTRAL" | "SKIPPED");
                            let certain = matches!(
                                status.as_str(),
                                "IN_PROGRESS"
                                    | "QUEUED"
                                    | "PENDING"
                                    | "WAITING"
                                    | "REQUESTED"
                                    | "EXPECTED"
                            ) || (status == "COMPLETED"
                                && matches!(
                                    conclusion,
                                    "FAILURE"
                                        | "TIMED_OUT"
                                        | "CANCELLED"
                                        | "ACTION_REQUIRED"
                                        | "STALE"
                                        | "STARTUP_FAILURE"
                                ));
                            (name, format!("{status}/{conclusion}"), passing, certain)
                        }
                        Check::StatusContext { context, state } => (
                            context,
                            state.clone(),
                            state == "SUCCESS",
                            matches!(state.as_str(), "FAILURE" | "ERROR" | "PENDING" | "EXPECTED"),
                        ),
                    };
                    let name = safe_text(name);
                    let status = safe_text(&status);
                    if !passing {
                        reason(certain, format!("{name}: {status}"));
                    }
                    result.checks.push(CheckResult { name, status });
                }
            }
        },
        _ => reason(
            false,
            "CI evidence does not match the current head commit".into(),
        ),
    }
    result.state = if blocked {
        ReadinessState::Blocked
    } else if unknown {
        ReadinessState::Unknown
    } else {
        ReadinessState::Ready
    };
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        json!({"data":{"repository":{"pullRequest":{
            "headRefOid":"a".repeat(40),"isDraft":false,"state":"OPEN","mergeable":"MERGEABLE",
            "mergeStateStatus":"CLEAN","reviewDecision":"APPROVED",
            "commits":{"nodes":[{"commit":{"oid":"a".repeat(40),"statusCheckRollup":{
                "state":"SUCCESS","contexts":{"totalCount":1,"pageInfo":{"hasNextPage":false},"nodes":[{
                    "__typename":"CheckRun","name":"Tests","status":"COMPLETED","conclusion":"SUCCESS"
                }]}
            }}}]}
        }}}})
    }
    fn pr(value: &mut Value) -> &mut Value {
        &mut value["data"]["repository"]["pullRequest"]
    }
    #[test]
    fn successful_evidence_and_actual_review_decision() {
        let mut value = fixture();
        let evidence = parse_response(&value);
        assert_eq!(evidence.state, ReadinessState::Ready);
        assert_eq!(evidence.head_sha.as_deref(), Some("a".repeat(40).as_str()));
        assert_eq!(evidence.checks.len(), 1);
        pr(&mut value)["reviewDecision"] = Value::Null;
        let evidence = parse_response(&value);
        assert_eq!(evidence.state, ReadinessState::Ready);
        assert_eq!(evidence.review_decision, "not_required");
    }
    #[test]
    fn policy_gates_block() {
        for (field, value) in [
            ("isDraft", json!(true)),
            ("state", json!("CLOSED")),
            ("mergeable", json!("CONFLICTING")),
            ("mergeStateStatus", json!("BEHIND")),
            ("mergeStateStatus", json!("BLOCKED")),
            ("reviewDecision", json!("CHANGES_REQUESTED")),
            ("reviewDecision", json!("REVIEW_REQUIRED")),
        ] {
            let mut response = fixture();
            pr(&mut response)[field] = value;
            assert_eq!(
                parse_response(&response).state,
                ReadinessState::Blocked,
                "{field}"
            );
        }
    }
    #[test]
    fn checks_never_hide_pending_or_failure() {
        for conclusion in [
            "FAILURE",
            "TIMED_OUT",
            "CANCELLED",
            "ACTION_REQUIRED",
            "STALE",
            "STARTUP_FAILURE",
        ] {
            let mut response = fixture();
            pr(&mut response)["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]
                ["nodes"][0]["conclusion"] = json!(conclusion);
            assert_eq!(parse_response(&response).state, ReadinessState::Blocked);
        }
        for state in ["PENDING", "FAILURE", "ERROR", "EXPECTED"] {
            let mut response = fixture();
            pr(&mut response)["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]
                ["nodes"][0] =
                json!({"__typename":"StatusContext","context":"Legacy CI","state":state});
            assert_eq!(parse_response(&response).state, ReadinessState::Blocked);
        }
        for conclusion in ["SUCCESS", "NEUTRAL", "SKIPPED"] {
            let mut response = fixture();
            pr(&mut response)["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]
                ["nodes"][0]["conclusion"] = json!(conclusion);
            assert_eq!(parse_response(&response).state, ReadinessState::Ready);
        }
    }
    #[test]
    fn incomplete_data_is_unknown() {
        let mut responses = vec![json!({}), json!({"data":{"repository":null}})];
        let mut partial = fixture();
        partial["errors"] = json!([{"message":"permission denied"}]);
        responses.push(partial);
        let mut missing = fixture();
        pr(&mut missing)
            .as_object_mut()
            .unwrap()
            .remove("reviewDecision");
        responses.push(missing);
        let mut unknown = fixture();
        pr(&mut unknown)["mergeable"] = json!("UNKNOWN");
        responses.push(unknown);
        let mut different = fixture();
        pr(&mut different)["commits"]["nodes"][0]["commit"]["oid"] = json!("b".repeat(40));
        responses.push(different);
        let mut truncated = fixture();
        pr(&mut truncated)["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]
            ["pageInfo"]["hasNextPage"] = json!(true);
        responses.push(truncated);
        let mut empty = fixture();
        pr(&mut empty)["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = Value::Null;
        responses.push(empty);
        for response in responses {
            assert_eq!(parse_response(&response).state, ReadinessState::Unknown);
        }
    }
    #[test]
    fn untrusted_strings_are_bounded_and_sanitized() {
        assert_eq!(safe_text("test\n\x1b[31m"), "test[31m");
        assert_eq!(safe_text(&"🦀".repeat(1000)).chars().count(), 256);
    }
    #[tokio::test]
    async fn cooldown_cannot_be_shortened() {
        let client = ReadinessClient::new(ReviewClient::new("test-token", true));
        client.defer(120).await;
        client.defer(60).await;
        assert!(
            client
                .cooldown
                .lock()
                .await
                .unwrap()
                .duration_since(Instant::now())
                > Duration::from_secs(110)
        );
        assert_eq!(
            client.fetch("example", "demo", 1).await.state,
            ReadinessState::Unknown
        );
    }
    async fn server_response(
        status: &str,
        headers: &str,
        body: Value,
    ) -> (ReadinessClient, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = ReadinessClient::new(ReviewClient::for_test(format!(
            "http://{}/",
            listener.local_addr().unwrap()
        )));
        let status = status.to_string();
        let headers = headers.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let (start, size) = loop {
                let mut buffer = [0u8; 1024];
                let len = socket.read(&mut buffer).await.unwrap();
                assert!(len > 0);
                request.extend_from_slice(&buffer[..len]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]);
                    assert!(header.starts_with("POST /graphql "));
                    let size: usize = header
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    break (end + 4, size);
                }
            };
            while request.len() < start + size {
                let mut buffer = [0u8; 1024];
                let len = socket.read(&mut buffer).await.unwrap();
                assert!(len > 0);
                request.extend_from_slice(&buffer[..len]);
            }
            let query: Value = serde_json::from_slice(&request[start..start + size]).unwrap();
            assert_eq!(query["query"], QUERY);
            assert_eq!(
                query["variables"],
                json!({"owner":"example","repo":"demo","number":1})
            );
            let body = body.to_string();
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        (client, server)
    }
    #[tokio::test]
    async fn graphql_snapshot_and_exhausted_budget_stop_future_network_calls() {
        let mut value = fixture();
        value["data"]["rateLimit"] = json!({"remaining":0,"resetAt":(Utc::now() + chrono::Duration::seconds(120)).to_rfc3339()});
        let (client, server) = server_response("200 OK", "", value).await;
        assert_eq!(
            client.fetch("example", "demo", 1).await.state,
            ReadinessState::Ready
        );
        server.await.unwrap();
        assert_eq!(
            client.fetch("example", "demo", 1).await.state,
            ReadinessState::Unknown
        );
    }
    #[tokio::test]
    async fn http_rate_limit_respects_retry_after_without_hidden_retries() {
        let (client, server) = server_response(
            "429 Too Many Requests",
            "Retry-After: 120\r\n",
            json!({"message":"rate limited"}),
        )
        .await;
        assert_eq!(
            client.fetch("example", "demo", 1).await.state,
            ReadinessState::Unknown
        );
        server.await.unwrap();
        assert!(
            client
                .cooldown
                .lock()
                .await
                .unwrap()
                .duration_since(Instant::now())
                > Duration::from_secs(110)
        );
        assert_eq!(
            client.fetch("example", "demo", 1).await.state,
            ReadinessState::Unknown
        );
    }
}
