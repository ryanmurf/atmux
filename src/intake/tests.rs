#![allow(clippy::too_many_lines, clippy::items_after_statements)]
use super::*;
use crate::herodevs::{AuthProvider, HerodevsConfig, Secret, TokenFuture};
use axum::{Json, Router, extract::State, routing::post};
use std::{collections::BTreeMap, path::PathBuf, sync::Mutex};
struct Bearer;
impl AuthProvider for Bearer {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(async { Secret::new("fixture".into()) })
    }
}
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "atmux-intake-test-{}",
            crate::tmux::new_session_key().unwrap()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Default)]
struct Fake {
    jobs: BTreeMap<String, Value>,
    posts: usize,
    claims: usize,
    messages: usize,
    triage: String,
    routing: String,
    board_status: String,
    board_title: String,
    has_next: bool,
    requests: Vec<Value>,
    file: bool,
    slack: bool,
}
async fn graph(State(fake): State<Arc<Mutex<Fake>>>, Json(v): Json<Value>) -> Json<Value> {
    let mut f = fake.lock().unwrap();
    f.requests.push(v.clone());
    let q = v["query"].as_str().unwrap();
    let vars = &v["variables"];
    let data = if q.contains("organization(") {
        json!({"organization":{"projectV2":{"id":"project","items":{"nodes":[{"id":"item","content":{"title":if f.board_title.is_empty(){"Fix intake"}else{&f.board_title},"body":"Run tests","url":"https://github.com/neverendingsupport/repo/issues/1","repository":{"url":"https://github.com/neverendingsupport/repo"},"assignees":{"nodes":[{"login":"ryanmurf"}]}},"fieldValues":{"nodes":[{"name":if f.board_status.is_empty(){"Todo"}else{&f.board_status},"field":{"name":"Status"}}]}}],"pageInfo":{"endCursor":"board-cursor","hasNextPage":f.has_next}}}}})
    } else if q.contains("postJob(") {
        let input = &vars["input"];
        let request = input["clientRequestId"].as_str().unwrap().to_owned();
        if !f.jobs.contains_key(&request) {
            f.posts += 1;
            let number = f.posts;
            f.jobs.insert(request.clone(),json!({"id":format!("message-{number}"),"jobId":format!("job-{number}"),"jobType":"IMPL","jobState":"PENDING","channelId":input["channelId"],"content":input["content"],"metadata":input["metadata"],"ordinal":number}));
        }
        json!({"channelMutations":{"postJob":f.jobs[&request]}})
    } else if q.contains("listJobs(") {
        let jobs: Vec<_> = f
            .jobs
            .values()
            .filter(|j| {
                j["channelId"] == vars["channel"]
                    && vars["metadata"]
                        .as_object()
                        .unwrap()
                        .iter()
                        .all(|(k, val)| j["metadata"][k] == *val)
            })
            .cloned()
            .collect();
        json!({"tenant":{"listJobs":jobs}})
    } else if q.contains("sendMessage(") {
        f.messages += 1;
        json!({"channelMutations":{"sendMessage":{"id":"assignment-message"}}})
    } else if q.contains("claimJob(") {
        f.claims += 1;
        let message = f.jobs.values_mut().find(|j| j["id"] == vars["id"]).unwrap();
        message["jobState"] = json!("CLAIMED");
        message["fenceToken"] = json!(1);
        json!({"channelMutations":{"claimJob":message}})
    } else if ["renewClaim(", "startJob(", "completeJob(", "escalateJob("]
        .iter()
        .any(|s| q.contains(s))
    {
        let operation = if q.contains("renewClaim(") {
            "renewClaim"
        } else if q.contains("startJob(") {
            "startJob"
        } else if q.contains("completeJob(") {
            "completeJob"
        } else {
            "escalateJob"
        };
        let message = f.jobs.values_mut().find(|j| j["id"] == vars["id"]).unwrap();
        message["jobState"] = json!(match operation {
            "startJob" => "IN_PROGRESS",
            "completeJob" => "COMPLETED",
            "escalateJob" => "ESCALATED",
            _ => "CLAIMED",
        });
        json!({"channelMutations":{operation:message}})
    } else if q.contains("fileUploads(") {
        json!({"tenant":{"fileUploads":{"edges":if f.file{json!([{"cursor":"file-cursor","node":{"id":"file","filename":"meeting_Gather_test.md","indexable":true,"size":40,"contentType":"text/markdown","createdAt":"2026-09-30T00:00:00Z"}}])}else{json!([])},"pageInfo":{"hasNextPage":false,"endCursor":"file-cursor"}}}})
    } else if q.contains("fileUpload(") {
        use base64::Engine as _;
        json!({"tenant":{"fileUpload":{"data":base64::engine::general_purpose::STANDARD.encode("Ryan: implement fixture tests.")}}})
    } else if q.contains("messages(") {
        json!({"tenant":{"channel":{"messages":{"edges":if f.slack{json!([{"cursor":"slack-cursor","node":{"id":"slack-message","content":"Ryan please fix the tests","metadata":{"source_url":"https://slack.test/message"}}}])}else{json!([])},"pageInfo":{"endCursor":"slack-cursor","hasNextPage":false}}}}})
    } else {
        panic!("unexpected fixture query {q}")
    };
    Json(json!({"data":data}))
}
async fn flash(State(fake): State<Arc<Mutex<Fake>>>) -> Json<Value> {
    Json(json!({"choices":[{"message":{"content":fake.lock().unwrap().triage}}]}))
}
async fn routing(State(fake): State<Arc<Mutex<Fake>>>) -> Json<Value> {
    Json(json!({"choices":[{"message":{"content":fake.lock().unwrap().routing}}]}))
}
async fn search() -> Json<Value> {
    Json(
        json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"data\":{\"search\":[{\"entityId\":\"file\",\"extractedContent\":\"fixture\"}]}}"}]}}),
    )
}
#[derive(Default)]
struct PoolState {
    candidates: Vec<Candidate>,
    launches: usize,
    clones: usize,
    sends: Vec<String>,
    events: Vec<String>,
    missing_folder: bool,
    socket: Option<String>,
    output: Option<PathBuf>,
    kill: Option<PathBuf>,
}
#[derive(Default)]
struct FakePool(Mutex<PoolState>);
impl AgentPool for FakePool {
    fn candidates(&self) -> Result<Vec<Candidate>> {
        Ok(self.0.lock().unwrap().candidates.clone())
    }
    fn policy_available(&self, _: &LaunchPolicy) -> bool {
        true
    }
    fn find(&self, _: &str) -> Result<Value> {
        Ok(json!([]))
    }
    fn folder<'a>(&'a self, _: &'a str, _: &'a str) -> PoolFuture<'a, Option<String>> {
        Box::pin(async {
            Ok((!self.0.lock().unwrap().missing_folder).then(|| "/projects/repo".into()))
        })
    }
    fn roots<'a>(&'a self, _: &'a str) -> PoolFuture<'a, Vec<String>> {
        Box::pin(async { Ok(vec!["/projects".into()]) })
    }
    fn clone_repo<'a>(&'a self, _: &'a str, _: &'a str, _: &'a str) -> PoolFuture<'a, String> {
        Box::pin(async {
            self.0.lock().unwrap().clones += 1;
            Ok("/projects/repo".into())
        })
    }
    fn launch<'a>(
        &'a self,
        p: &'a LaunchPolicy,
        name: &'a str,
        folder: &'a str,
    ) -> PoolFuture<'a, Candidate> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            state.launches += 1;
            let pane: String = if let Some(socket) = &state.socket {
                let output = state.output.as_ref().unwrap();
                let command = format!("cat > {}", shell_words::quote(&output.to_string_lossy()));
                let result = std::process::Command::new("tmux")
                    .args([
                        "-L",
                        socket,
                        "-f",
                        "/dev/null",
                        "new-session",
                        "-d",
                        "-P",
                        "-F",
                        "#{pane_id}",
                        "-s",
                        name,
                        &command,
                    ])
                    .output()?;
                ensure!(result.status.success(), "fixture launch failed");
                String::from_utf8(result.stdout)?.trim().into()
            } else {
                "%1".into()
            };
            let mut c = candidate();
            c.pane = format!("{}~{pane}", p.machine);
            c.name = name.into();
            c.folder = folder.into();
            state.candidates.push(c.clone());
            Ok(c)
        })
    }
    fn send<'a>(&'a self, c: &'a Candidate, text: &'a str) -> PoolFuture<'a, ()> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            if let Some(socket) = &state.socket {
                crate::tmux::Tmux::with_socket_for_test(socket, || {
                    crate::tmux::Tmux.send_text(c.pane.split('~').nth(1).unwrap(), text, true)
                })?;
            }
            state.sends.push(text.into());
            Ok(())
        })
    }
    fn audit(&self, kind: &str, _: &str, _: Value) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        state.events.push(kind.into());
        if kind == "intake.decision"
            && let Some(path) = &state.kill
        {
            std::fs::write(path, "")?;
        }
        Ok(())
    }
}
fn candidate() -> Candidate {
    Candidate {
        session_key: crate::tmux::new_session_key().unwrap(),
        pane: "tron~%1".into(),
        instance_id: "generation".into(),
        machine: "tron".into(),
        name: "existing".into(),
        folder: "/projects/repo".into(),
        repo_remote: Some("https://github.com/neverendingsupport/repo".into()),
        status: "waiting".into(),
        digest: "Unfinished: fix tests".into(),
    }
}
struct Fixture {
    intake: Intake,
    fake: Arc<Mutex<Fake>>,
    pool: Arc<FakePool>,
    temp: Temp,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        let state = self.pool.0.lock().unwrap();
        if let Some(socket) = &state.socket {
            let _ = std::process::Command::new("tmux")
                .args(["-L", socket, "kill-server"])
                .status();
        }
    }
}
async fn fixture() -> Fixture {
    let temp = Temp::new();
    let fake=Arc::new(Mutex::new(Fake{triage:json!({"items":[{"owner":"Ryan","what":"Fix fixture","criteria":"Tests green","due_date":null,"repo_remote":"https://github.com/neverendingsupport/repo"}]}).to_string(),routing:json!({"action":"launch","policy_id":"implementation"}).to_string(),..Fake::default()}));
    let router = Router::new()
        .route("/graphql", post(graph))
        .route("/flash/chat/completions", post(flash))
        .route("/route/chat/completions", post(routing))
        .route("/mcp", post(search))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let key = temp.0.join("github");
    std::fs::write(&key, "fixture").unwrap();
    let llm = |name| crate::llm::LlmConfig {
        endpoint: format!("{url}/{name}"),
        allow_http_hosts: vec!["127.0.0.1".into()],
        ..crate::llm::LlmConfig::default()
    };
    let config = IntakeConfig {
        enabled: true,
        dry_run: false,
        store_dir: temp.0.clone(),
        kill_switch_file: temp.0.join("STOP"),
        github: crate::github::GithubConfig {
            endpoint: format!("{url}/graphql"),
            token_file: key,
            allow_http_hosts: vec!["127.0.0.1".into()],
        },
        triage: llm("flash"),
        routing: llm("route"),
        boards: vec![Board {
            org: "neverendingsupport".into(),
            number: 40,
            channel_id: "board-channel".into(),
            done_statuses: vec!["Done".into()],
            assignee: Some("ryanmurf".into()),
        }],
        policies: vec![LaunchPolicy {
            id: "implementation".into(),
            machine: "tron".into(),
            project_root: "/projects".into(),
            profile_id: "profile-0".into(),
            mode_id: "sol61-xhigh".into(),
            repo_remote: Some("https://github.com/neverendingsupport/repo".into()),
            folder: None,
            allow_clone: true,
            job_types: vec!["IMPL".into()],
        }],
        ..IntakeConfig::default()
    };
    let ledger = HerodevsClient::new(
        HerodevsConfig {
            graphql_url: format!("{url}/graphql"),
            mcp_url: format!("{url}/mcp"),
            allow_http_hosts: vec!["127.0.0.1".into()],
            ..HerodevsConfig::default()
        },
        Arc::new(Bearer),
    )
    .unwrap();
    let pool = Arc::new(FakePool::default());
    let intake = Intake::new(config, ledger, pool.clone()).unwrap();
    Fixture {
        intake,
        fake,
        pool,
        temp,
        task,
    }
}
const BOARD_KEY: &str = "github:neverendingsupport:40:item";
#[tokio::test]
async fn board_retry_updates_close_and_cursor_restart() {
    let mut f = fixture().await;
    f.fake.lock().unwrap().has_next = true;
    f.intake.boards().await.unwrap();
    f.intake.boards().await.unwrap();
    assert_eq!(f.fake.lock().unwrap().posts, 1);
    assert_eq!(
        store::read(&f.temp.0).unwrap().cursors["board:neverendingsupport:40"].as_deref(),
        Some("board-cursor")
    );
    f.fake.lock().unwrap().board_title = "Updated title".into();
    f.intake.boards().await.unwrap();
    assert!(
        f.intake.store.state.jobs[BOARD_KEY]
            .item
            .goal
            .contains("Updated title")
    );
    f.fake.lock().unwrap().board_status = "Done".into();
    f.intake.boards().await.unwrap();
    assert_eq!(
        f.intake.store.state.jobs[BOARD_KEY]
            .message
            .job_state
            .as_deref(),
        Some("COMPLETED")
    );
    assert_eq!(f.fake.lock().unwrap().posts, 1);
}
#[tokio::test]
async fn gather_slack_extraction_durable_and_malformed() {
    let mut f = fixture().await;
    f.intake.config.meetings_channel = Some("meetings".into());
    f.fake.lock().unwrap().file = true;
    f.fake.lock().unwrap().triage = "```json {} ```".into();
    assert!(f.intake.meetings().await.is_err());
    assert!(!f.intake.store.state.cursors.contains_key("gather:file"));
    f.fake.lock().unwrap().triage=json!({"items":[{"owner":"Ryan","what":"Implement action","criteria":"Tests green","due_date":"2026-10-01T00:00:00Z","repo_remote":null},{"owner":"Someone else","what":"No Ryan action","criteria":"","due_date":null,"repo_remote":null}]}).to_string();
    f.intake.meetings().await.unwrap();
    f.intake.meetings().await.unwrap();
    assert_eq!(f.fake.lock().unwrap().posts, 1);
    assert!(
        store::read(&f.temp.0)
            .unwrap()
            .cursors
            .contains_key("gather:file")
    );
    f.intake.config.slack_channel = Some("intake-channel".into());
    f.intake.config.slack_jobs_channel = Some("slack-jobs".into());
    f.fake.lock().unwrap().slack = true;
    f.intake.slack().await.unwrap();
    f.intake.slack().await.unwrap();
    assert_eq!(f.fake.lock().unwrap().posts, 2);
    assert_eq!(
        f.intake.store.state.cursors["slack"].as_deref(),
        Some("slack-cursor")
    );
}
#[tokio::test]
async fn existing_route_claims_and_kickoff_protocol() {
    let mut f = fixture().await;
    let c = candidate();
    f.pool.0.lock().unwrap().candidates.push(c.clone());
    f.fake.lock().unwrap().routing =
        json!({"action":"existing","session_key":c.session_key}).to_string();
    f.intake.boards().await.unwrap();
    f.intake.route(BOARD_KEY).await.unwrap();
    f.intake.route(BOARD_KEY).await.unwrap();
    let pool = f.pool.0.lock().unwrap();
    assert_eq!(pool.launches, 0);
    assert_eq!(pool.sends.len(), 1);
    assert!(pool.sends[0].contains("JOB DONE job-1:"));
    assert_eq!(f.fake.lock().unwrap().claims, 1);
    assert!(f.intake.store.state.jobs[BOARD_KEY].dispatched);
}
#[tokio::test]
async fn clone_launch_and_durable_budgets() {
    let mut f = fixture().await;
    f.pool.0.lock().unwrap().missing_folder = true;
    f.intake.boards().await.unwrap();
    f.intake.route(BOARD_KEY).await.unwrap();
    assert_eq!(f.pool.0.lock().unwrap().clones, 1);
    assert_eq!(f.pool.0.lock().unwrap().launches, 1);
    let state = store::read(&f.temp.0).unwrap();
    assert!(state.jobs[BOARD_KEY].assignment.is_some());
    assert_eq!(state.launch_days[&(epoch() / 86400)], 1);
    let mut store = Store::open(&f.temp.0.join("budget")).unwrap();
    assert!(store.reserve_launch(100, 1, 1).unwrap());
    assert!(!store.reserve_launch(200, 1, 1).unwrap());
    assert!(!store.reserve_launch(3601, 1, 1).unwrap());
    assert!(store.reserve_launch(86400, 1, 1).unwrap());
    assert!(store.reserve_llm(10, 1).unwrap());
    drop(store);
    let mut store = Store::open(&f.temp.0.join("budget")).unwrap();
    assert!(!store.reserve_llm(20, 1).unwrap());
}
#[tokio::test]
async fn dry_run_and_kill_switch_act_on_nothing() {
    let mut f = fixture().await;
    f.intake.config.dry_run = true;
    f.intake.boards().await.unwrap();
    f.intake.route(BOARD_KEY).await.unwrap();
    assert_eq!(f.fake.lock().unwrap().posts, 0);
    assert_eq!(f.fake.lock().unwrap().claims, 0);
    assert!(f.intake.store.state.cursors.is_empty());
    assert_eq!(f.pool.0.lock().unwrap().launches, 0);
    assert!(
        f.pool
            .0
            .lock()
            .unwrap()
            .events
            .iter()
            .any(|e| e == "intake.decision")
    );
    f.intake.config.dry_run = false;
    f.intake.store.state.jobs.clear();
    f.intake.boards().await.unwrap();
    f.pool.0.lock().unwrap().kill = Some(f.intake.config.kill_switch_file.clone());
    assert!(f.intake.route(BOARD_KEY).await.is_err());
    assert_eq!(f.fake.lock().unwrap().claims, 0);
    assert_eq!(f.pool.0.lock().unwrap().launches, 0);
}
#[tokio::test]
async fn invalid_decisions_escalate_and_interrupted_kickoff_is_not_replayed() {
    let mut f = fixture().await;
    f.intake.boards().await.unwrap();
    f.fake.lock().unwrap().routing =
        json!({"action":"existing","session_key":"invented"}).to_string();
    f.intake.route(BOARD_KEY).await.unwrap();
    assert_eq!(
        f.intake.store.state.jobs[BOARD_KEY]
            .message
            .job_state
            .as_deref(),
        Some("ESCALATED")
    );
    assert_eq!(f.pool.0.lock().unwrap().launches, 0);
    let mut f = fixture().await;
    f.intake.boards().await.unwrap();
    f.intake.route(BOARD_KEY).await.unwrap();
    let job = f.intake.store.state.jobs.get_mut(BOARD_KEY).unwrap();
    job.dispatched = false;
    job.dispatch_started = true;
    f.intake.store.save().unwrap();
    f.intake.route(BOARD_KEY).await.unwrap();
    assert_eq!(f.pool.0.lock().unwrap().sends.len(), 1);
    assert!(f.intake.store.state.jobs[BOARD_KEY].blocked.is_some());
}
#[tokio::test]
async fn board_to_disposable_tmux_receives_kickoff() {
    let mut f = fixture().await;
    let socket = format!(
        "atmux-test-intake-{}",
        crate::tmux::new_session_key().unwrap()
    );
    let output = f.temp.0.join("kickoff.txt");
    {
        let mut pool = f.pool.0.lock().unwrap();
        pool.socket = Some(socket);
        pool.output = Some(output.clone());
    }
    f.intake.boards().await.unwrap();
    f.intake.route(BOARD_KEY).await.unwrap();
    let mut text = String::new();
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(&output) {
            text = s;
            if text.contains("JOB BLOCKED") {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(text.contains("Fix intake"), "{text}");
    assert!(text.contains("JOB DONE job-1:"), "{text}");
}
#[test]
fn defaults_decisions_and_repo_matching_fail_closed() {
    assert!(!IntakeConfig::default().enabled);
    assert!(IntakeConfig::default().dry_run);
    assert!(
        router::validate_decision(
            &Decision::Launch {
                policy_id: "shell-command".into()
            },
            &[],
            &[]
        )
        .is_err()
    );
    assert_eq!(
        canonical_repo("git@github.com:neverendingsupport/repo.git"),
        canonical_repo("https://github.com/neverendingsupport/repo")
    );
    assert!(canonical_repo("https://token@github.com/repo").is_none());
}

#[tokio::test]
async fn followups_are_idempotent_and_respect_assignments() {
    let mut fixture = fixture().await;
    fixture.intake.config.followups_channel = Some("followups".into());
    fixture.pool.0.lock().unwrap().candidates.push(candidate());
    fixture.intake.followups().await.unwrap();
    fixture.intake.followups().await.unwrap();
    assert_eq!(fixture.fake.lock().unwrap().posts, 1);
    assert!(
        fixture
            .intake
            .store
            .state
            .jobs
            .values()
            .all(|j| j.item.metadata["session_key"].is_string())
    );
    fixture.fake.lock().unwrap().triage = json!({"items":[]}).to_string();
    let items = fixture
        .intake
        .extract(
            "non-action",
            "channel",
            "slack",
            "https://slack.test/message",
            "Just saying hello",
        )
        .await
        .unwrap();
    assert!(items.is_empty());
}
#[test]
fn repository_lookup_uses_real_remote_and_refuses_credentials() {
    let temp = Temp::new();
    let repo = temp.0.join("nested/repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "git@github.com:neverendingsupport/repo.git"
            ])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success()
    );
    let mut config = crate::config::Config::default();
    config.general.project_roots = vec![temp.0.clone()];
    config.general.favorite_dirs.clear();
    assert_eq!(
        pool_lookup(&config, "https://github.com/neverendingsupport/repo").unwrap(),
        Some(repo.to_string_lossy().into())
    );
    assert!(
        pool_lookup(
            &config,
            "https://credential@github.com/neverendingsupport/repo"
        )
        .is_err()
    );
}
#[tokio::test]
async fn launch_budget_and_llm_budget_stop_side_effects() {
    let mut fixture = fixture().await;
    fixture.intake.boards().await.unwrap();
    fixture.intake.config.new_sessions_per_day = 0;
    assert!(fixture.intake.route(BOARD_KEY).await.is_err());
    assert_eq!(fixture.pool.0.lock().unwrap().launches, 0);
    assert!(fixture.pool.0.lock().unwrap().sends.is_empty());
    let mut fixture = super::tests::fixture().await;
    fixture.intake.config.llm_calls_per_hour = 0;
    fixture.intake.boards().await.unwrap();
    assert!(fixture.intake.route(BOARD_KEY).await.is_err());
    assert_eq!(fixture.fake.lock().unwrap().claims, 0);
}

#[tokio::test]
async fn dry_run_reads_unassigned_ledger_without_mutation() {
    let mut fixture = fixture().await;
    fixture.fake.lock().unwrap().jobs.insert("external".into(),json!({"id":"external-message","jobId":"external-job","jobType":"IMPL","jobState":"PENDING","channelId":"board-channel","content":"Implement external job","metadata":{"repo_remote":"https://github.com/neverendingsupport/repo"}}));
    fixture.intake.config.dry_run = true;
    fixture.intake.sync_ledger().await.unwrap();
    fixture
        .intake
        .route("ledger:external-message")
        .await
        .unwrap();
    assert!(fixture.intake.store.state.cursors.is_empty());
    assert_eq!(fixture.fake.lock().unwrap().posts, 0);
    assert_eq!(fixture.fake.lock().unwrap().claims, 0);
    assert_eq!(fixture.pool.0.lock().unwrap().launches, 0);
}
