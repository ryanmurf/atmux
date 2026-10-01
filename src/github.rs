//! Bounded GitHub GraphQL `ProjectsV2` and pull-request client.
#![allow(clippy::missing_errors_doc)]
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubConfig {
    pub endpoint: String,
    pub token_file: PathBuf,
    pub allow_http_hosts: Vec<String>,
}
impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://api.github.com/graphql".into(),
            token_file: PathBuf::new(),
            allow_http_hosts: vec![],
        }
    }
}
#[derive(Clone)]
pub struct GithubClient {
    config: GithubConfig,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectItem {
    pub id: String,
    pub title: String,
    pub body: String,
    pub url: String,
    pub repo_remote: Option<String>,
    pub status: Option<String>,
    pub assignees: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct ProjectPage {
    pub project_id: String,
    pub items: Vec<ProjectItem>,
    pub cursor: Option<String>,
    pub has_next: bool,
}
impl GithubClient {
    pub fn new(config: GithubConfig) -> Result<Self> {
        crate::platform_http::endpoint(&config.endpoint, &config.allow_http_hosts)?;
        Ok(Self { config })
    }
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let key = crate::herodevs::read_secret(&self.config.token_file)?;
        let value = crate::platform_http::json(
            &crate::platform_http::endpoint(&self.config.endpoint, &self.config.allow_http_hosts)?,
            &[
                ("Authorization", format!("Bearer {}", key.expose())),
                ("User-Agent", "atmux-intake".into()),
            ],
            &json!({"query":query,"variables":variables}),
            60,
        )
        .await?;
        ensure!(
            value
                .get("errors")
                .is_none_or(|e| e.as_array().is_some_and(Vec::is_empty)),
            "GitHub GraphQL rejected operation"
        );
        value
            .get("data")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("GitHub data missing"))
    }
    pub async fn project_items(
        &self,
        org: &str,
        number: u32,
        after: Option<&str>,
        limit: usize,
    ) -> Result<ProjectPage> {
        ensure!((1..=100).contains(&limit), "invalid project page limit");
        let data = self.graphql("query($org:String!,$number:Int!,$after:String,$first:Int!){organization(login:$org){projectV2(number:$number){id items(first:$first,after:$after){pageInfo{hasNextPage endCursor} nodes{id fieldValues(first:40){nodes{... on ProjectV2ItemFieldSingleSelectValue{name field{... on ProjectV2SingleSelectField{name}}}}} content{... on Issue{title body url repository{url} assignees(first:20){nodes{login}}} ... on PullRequest{title body url repository{url} assignees(first:20){nodes{login}}} ... on DraftIssue{title body assignees(first:20){nodes{login}}}}}}}}}", json!({"org":org,"number":number,"after":after,"first":limit})).await?;
        let project = data
            .pointer("/organization/projectV2")
            .ok_or_else(|| anyhow::anyhow!("GitHub project missing"))?;
        let nodes = project
            .pointer("/items/nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("GitHub items missing"))?;
        let mut items = vec![];
        for node in nodes {
            if node["content"].is_null() {
                continue;
            }
            let content = &node["content"];
            let text = |v: &Value| v.as_str().unwrap_or("").to_owned();
            items.push(ProjectItem {
                id: text(&node["id"]),
                title: text(&content["title"]),
                body: text(&content["body"]),
                url: content["url"].as_str().map_or_else(
                    || {
                        format!(
                            "https://github.com/orgs/{org}/projects/{number}?pane=issue&itemId={}",
                            text(&node["id"])
                        )
                    },
                    str::to_owned,
                ),
                repo_remote: content
                    .pointer("/repository/url")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                status: node
                    .pointer("/fieldValues/nodes")
                    .and_then(Value::as_array)
                    .and_then(|fields| {
                        fields.iter().find(|f| {
                            f.pointer("/field/name").and_then(Value::as_str) == Some("Status")
                        })
                    })
                    .and_then(|f| f["name"].as_str())
                    .map(str::to_owned),
                assignees: content
                    .pointer("/assignees/nodes")
                    .and_then(Value::as_array)
                    .map(|nodes| {
                        nodes
                            .iter()
                            .filter_map(|n| n["login"].as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
        Ok(ProjectPage {
            project_id: project["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("project id missing"))?
                .into(),
            items,
            cursor: project
                .pointer("/items/pageInfo/endCursor")
                .and_then(Value::as_str)
                .map(str::to_owned),
            has_next: project
                .pointer("/items/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
    pub async fn update_project_status(
        &self,
        project: &str,
        item: &str,
        field: &str,
        option: &str,
    ) -> Result<()> {
        self.graphql("mutation($input:UpdateProjectV2ItemFieldValueInput!){updateProjectV2ItemFieldValue(input:$input){projectV2Item{id}}}",json!({"input":{"projectId":project,"itemId":item,"fieldId":field,"value":{"singleSelectOptionId":option}}})).await?;
        Ok(())
    }
    pub async fn pull_request(&self, owner: &str, repo: &str, number: u32) -> Result<Value> {
        self.graphql("query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){pullRequest(number:$number){url state merged isDraft mergeable reviewDecision closingIssuesReferences(first:20){nodes{url title}} commits(last:1){nodes{commit{statusCheckRollup{state}}}}}}}",json!({"owner":owner,"repo":repo,"number":number})).await
    }
    pub async fn issue(&self, owner: &str, repo: &str, number: u32) -> Result<Value> {
        self.graphql("query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){issue(number:$number){url title body state timelineItems(last:20,itemTypes:[CONNECTED_EVENT,CROSS_REFERENCED_EVENT]){nodes{... on CrossReferencedEvent{source{... on PullRequest{url state merged}}}}}}}}",json!({"owner":owner,"repo":repo,"number":number})).await
    }
}
