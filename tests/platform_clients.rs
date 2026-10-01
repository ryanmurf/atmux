#![allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::verbose_bit_mask
)]
//! All endpoints in these tests are disposable loopback servers.
use atmux::{
    github::{GithubClient, GithubConfig},
    herodevs::{
        AuthConfig, AuthProvider, DeviceAuth, HerodevsClient, HerodevsConfig, PostJob, Secret,
        TokenFuture,
    },
    llm::{LlmClient, LlmConfig},
};
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct Bearer;
impl AuthProvider for Bearer {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(async { Secret::new("fixture-bearer".into()) })
    }
}
fn temp() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "atmux-client-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}
async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), task)
}
fn hd(url: &str) -> HerodevsClient {
    HerodevsClient::new(
        HerodevsConfig {
            graphql_url: format!("{url}/graphql"),
            mcp_url: format!("{url}/mcp"),
            allow_http_hosts: vec!["127.0.0.1".into()],
            ..HerodevsConfig::default()
        },
        Arc::new(Bearer),
    )
    .unwrap()
}

#[tokio::test]
async fn job_retry_filters_fence_and_error_redaction() {
    async fn handler(
        State(calls): State<Arc<Mutex<Vec<Value>>>>,
        Json(v): Json<Value>,
    ) -> Json<Value> {
        calls.lock().unwrap().push(v.clone());
        let q = v["query"].as_str().unwrap();
        let job = json!({"id":"message-id","jobId":"job-id","jobState":"PENDING","fenceToken":7,"metadata":{"source_url":"fixture"}});
        Json(if q.contains("postJob") {
            json!({"data":{"channelMutations":{"postJob":job}}})
        } else if q.contains("listJobs") {
            json!({"data":{"tenant":{"listJobs":[job]}}})
        } else if q.contains("startJob") {
            json!({"data":{"channelMutations":{"startJob":job}}})
        } else {
            json!({"errors":[{"message":"fixture-bearer secret"}]})
        })
    }
    let calls = Arc::new(Mutex::new(vec![]));
    let (url, task) = serve(
        Router::new()
            .route("/graphql", post(handler))
            .with_state(calls.clone()),
    )
    .await;
    let client = hd(&url);
    let input = PostJob {
        channel_id: "channel".into(),
        content: "Implement fixture".into(),
        job_type: "IMPL".into(),
        client_request_id: "stable-key".into(),
        metadata: json!({"source_url":"fixture"}),
        priority: 5,
    };
    assert_eq!(
        client.post_job(&input).await.unwrap().job_id,
        client.post_job(&input).await.unwrap().job_id
    );
    client
        .list_jobs("channel", &[], json!({"source_url":"fixture"}), 10)
        .await
        .unwrap();
    client
        .transition("startJob", "message-id", 7, Value::Null)
        .await
        .unwrap();
    assert_eq!(
        calls.lock().unwrap()[0]["variables"]["input"]["clientRequestId"],
        "stable-key"
    );
    assert_eq!(
        calls.lock().unwrap()[2]["variables"]["metadata"],
        json!({"source_url":"fixture"})
    );
    assert_eq!(calls.lock().unwrap()[3]["variables"]["fence"], 7);
    assert!(
        !client
            .graphql("fail", Value::Null)
            .await
            .unwrap_err()
            .to_string()
            .contains("fixture-bearer")
    );
    assert_eq!(
        format!("{:?}", Secret::new("private".into()).unwrap()),
        "[REDACTED]"
    );
    task.abort();
}
#[tokio::test]
async fn github_and_strict_llm_fixture() {
    async fn graph() -> Json<Value> {
        Json(
            json!({"data":{"organization":{"projectV2":{"id":"project","items":{"pageInfo":{"endCursor":"cursor-1","hasNextPage":true},"nodes":[{"id":"item","content":{"title":"Fix tests","body":"criteria","url":"https://example.test/issue","repository":{"url":"https://github.com/test/repo"},"assignees":{"nodes":[{"login":"ryanmurf"}]}},"fieldValues":{"nodes":[{"name":"Todo","field":{"name":"Status"}}]}}]}}}}}),
        )
    }
    async fn llm(Json(v): Json<Value>) -> Json<Value> {
        let prompt = v["messages"][1]["content"].as_str().unwrap();
        Json(
            json!({"choices":[{"message":{"content":if prompt=="bad" {"```json {} ```"}else{"{\"action\":\"existing\"}"}}}]}),
        )
    }
    let (url, task) = serve(
        Router::new()
            .route("/graphql", post(graph))
            .route("/v1/chat/completions", post(llm)),
    )
    .await;
    let directory = temp();
    let key = directory.join("github");
    std::fs::write(&key, "fixture").unwrap();
    let client = GithubClient::new(GithubConfig {
        endpoint: format!("{url}/graphql"),
        token_file: key,
        allow_http_hosts: vec!["127.0.0.1".into()],
    })
    .unwrap();
    let page = client
        .project_items("neverendingsupport", 40, None, 20)
        .await
        .unwrap();
    assert!(page.has_next);
    assert_eq!(page.items[0].assignees, vec!["ryanmurf"]);
    assert_eq!(page.cursor.as_deref(), Some("cursor-1"));
    let llm = LlmClient::new(LlmConfig {
        endpoint: format!("{url}/v1"),
        allow_http_hosts: vec!["127.0.0.1".into()],
        ..LlmConfig::default()
    })
    .unwrap();
    assert_eq!(
        llm.json::<Value>("data", "ok").await.unwrap()["action"],
        "existing"
    );
    assert!(llm.json::<Value>("data", "bad").await.is_err());
    task.abort();
    std::fs::remove_dir_all(directory).unwrap();
}
#[tokio::test]
async fn refresh_rotates_verifies_and_fails_closed() {
    #[derive(Clone)]
    struct Fixture {
        url: Arc<Mutex<String>>,
        subject: Arc<Mutex<String>>,
        calls: Arc<Mutex<Vec<String>>>,
    }
    async fn discovery(State(s): State<Fixture>) -> Json<Value> {
        let url = s.url.lock().unwrap().clone();
        Json(
            json!({"issuer":url,"token_endpoint":format!("{url}/token"),"device_authorization_endpoint":format!("{url}/device"),"jwks_uri":format!("{url}/keys")}),
        )
    }
    async fn keys() -> Json<Value> {
        Json(serde_json::from_str(include_str!("fixtures/intake/jwks.json")).unwrap())
    }
    async fn token(State(s): State<Fixture>, body: String) -> Json<Value> {
        s.calls.lock().unwrap().push(body);
        let issuer = s.url.lock().unwrap().clone();
        let subject = s.subject.lock().unwrap().clone();
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("fixture".into());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let jwt=jsonwebtoken::encode(&header,&json!({"iss":issuer,"sub":subject,"azp":"hd-atmux","tenant_id":"hq","identity_type":"USER","aud":["hd-subgraphs","https://hq.herodevs.dev/hd-mcp"],"exp":now+300}),&jsonwebtoken::EncodingKey::from_rsa_der(include_bytes!("fixtures/intake/test-only-key.der"))).unwrap();
        Json(json!({"token_type":"Bearer","access_token":jwt,"refresh_token":"rotated-secret"}))
    }
    let s = Fixture {
        url: Arc::new(Mutex::new(String::new())),
        subject: Arc::new(Mutex::new("ryan".into())),
        calls: Arc::new(Mutex::new(vec![])),
    };
    let (url, task) = serve(
        Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/keys", get(keys))
            .route("/token", post(token))
            .with_state(s.clone()),
    )
    .await;
    *s.url.lock().unwrap() = url.clone();
    let directory = temp();
    let secret = directory.join("client");
    let refresh = directory.join("refresh");
    std::fs::write(&secret, "client-secret").unwrap();
    std::fs::write(&refresh, "offline-secret").unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&refresh, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = AuthConfig {
        issuer: url,
        client_secret_file: secret,
        refresh_token_file: refresh.clone(),
        expected_subject: "ryan".into(),
        allow_http_hosts: vec!["127.0.0.1".into()],
        ..AuthConfig::default()
    };
    let auth = DeviceAuth::new(config.clone()).unwrap();
    let token = auth.access_token().await.unwrap();
    assert!(token.expose().contains('.'));
    auth.access_token().await.unwrap();
    assert_eq!(s.calls.lock().unwrap().len(), 1);
    assert!(s.calls.lock().unwrap()[0].contains("tenant_slug=hq"));
    assert_eq!(std::fs::read_to_string(&refresh).unwrap(), "rotated-secret");
    assert_eq!(
        std::fs::metadata(&refresh).unwrap().permissions().mode() & 0o077,
        0
    );
    *s.subject.lock().unwrap() = "wrong-user".into();
    let error = DeviceAuth::new(config)
        .unwrap()
        .access_token()
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("rotated-secret"));
    assert_eq!(std::fs::read_to_string(refresh).unwrap(), "LOGIN_REQUIRED");
    task.abort();
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn endpoints_and_metadata_fail_closed() {
    assert!(LlmClient::new(LlmConfig::default()).is_err());
    assert!(atmux::herodevs::validate_metadata(&json!({"nested":{}}), true).is_err());
    assert!(atmux::herodevs::validate_metadata(&json!({"text":"x".repeat(8193)}), false).is_err());
}
