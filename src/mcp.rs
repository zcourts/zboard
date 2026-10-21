use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::schemars::JsonSchema;
use rmcp::{Json, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{Client, ReplyRequest, SendRequest};
use crate::model::Agent;
use crate::query::{self, AgentQuery, GroupQuery, HistoryQuery};
use crate::storage::{create_group, group_members, join_group, touch_presence};

pub struct McpOptions {
    pub root: PathBuf,
    pub project: String,
    pub session: String,
    pub project_path: PathBuf,
    pub poll_interval: Duration,
}

/// MCP reads are snapshots. They never own or acknowledge the wake listener's
/// durable delivery cursor, even when both interfaces share an agent identity.
#[derive(Clone)]
pub struct ZboardServer {
    root: PathBuf,
    agent: Agent,
    poll_interval: Duration,
    inbox_seen: Arc<Mutex<BTreeSet<String>>>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendInput {
    #[serde(default)]
    to: Vec<String>,
    group: Option<String>,
    message: String,
    #[serde(default)]
    tags: Vec<String>,
    meta: Option<Value>,
    ttl_seconds: Option<u64>,
    /// A caller-generated ULID reused with identical content on uncertain retries.
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReplyInput {
    message_id: String,
    message: String,
    tags: Option<Vec<String>>,
    meta: Option<Value>,
    ttl_seconds: Option<u64>,
    /// A caller-generated ULID reused with identical content on uncertain retries.
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GroupInput {
    name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetInput {
    ids: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ThreadInput {
    message_id: String,
    before: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct PollInput {
    /// Explicit resume cursor. Omit to read a recent relevant window.
    cursor: Option<String>,
    timeout_ms: Option<u64>,
    limit: Option<usize>,
}

fn output(value: impl serde::Serialize) -> Result<Json<Value>, String> {
    serde_json::to_value(value)
        .map(Json)
        .map_err(|error| error.to_string())
}

fn error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

impl ZboardServer {
    fn new(options: McpOptions) -> Result<Self> {
        let client = Client::open(
            &options.root,
            &options.project,
            &options.session,
            &options.project_path,
        )?;
        Ok(Self {
            root: client.root,
            agent: client.agent,
            poll_interval: options.poll_interval,
            inbox_seen: Arc::new(Mutex::new(BTreeSet::new())),
            tool_router: Self::tool_router(),
        })
    }

    fn client(&self) -> Result<Client, String> {
        touch_presence(&self.root.join("v1"), &self.agent.id).map_err(error)?;
        self.read_client()
    }

    fn read_client(&self) -> Result<Client, String> {
        Ok(Client {
            root: self.root.clone(),
            agent: self.agent.clone(),
            state: query::snapshot(&self.root).map_err(error)?,
            notifications: Vec::new(),
        })
    }

    fn history(&self, input: &HistoryQuery) -> Result<Json<Value>, String> {
        let client = self.client()?;
        output(query::history(&self.root, &client.state, &self.agent, input).map_err(error)?)
    }
}

#[tool_router]
impl ZboardServer {
    #[tool(
        name = "identity",
        description = "Return this conversation's registered identity"
    )]
    fn identity(&self) -> Result<Json<Value>, String> {
        touch_presence(&self.root.join("v1"), &self.agent.id).map_err(error)?;
        Ok(Json(json!({"agent": self.agent})))
    }

    #[tool(
        name = "ping",
        description = "Renew this agent's one-minute online presence"
    )]
    fn ping(&self) -> Result<Json<Value>, String> {
        self.identity()
    }

    #[tool(
        name = "agents_list",
        description = "Discover agents filtered by project, online status, or text"
    )]
    fn agents_list(
        &self,
        Parameters(input): Parameters<AgentQuery>,
    ) -> Result<Json<Value>, String> {
        let client = self.client()?;
        output(query::agents(&client.state, &input).map_err(error)?)
    }

    #[tool(
        name = "groups_list",
        description = "Discover groups, optionally filtering joined groups or names"
    )]
    fn groups_list(
        &self,
        Parameters(input): Parameters<GroupQuery>,
    ) -> Result<Json<Value>, String> {
        let client = self.client()?;
        output(query::groups(&client.state, &self.agent, &input).map_err(error)?)
    }

    #[tool(
        name = "tags_list",
        description = "List every tag historically used on this board"
    )]
    fn tags_list(&self) -> Result<Json<Value>, String> {
        self.identity()?;
        Ok(Json(
            json!({"tags": crate::routed::tags(&self.root).map_err(error)?}),
        ))
    }

    #[tool(
        name = "group_create",
        description = "Create an idempotent collaboration group"
    )]
    fn group_create(
        &self,
        Parameters(input): Parameters<GroupInput>,
    ) -> Result<Json<Value>, String> {
        self.identity()?;
        let group =
            create_group(&self.root.join("v1"), &input.name, &self.agent.id).map_err(error)?;
        Ok(Json(
            json!({"name": group.name, "created_by": group.created_by, "members": []}),
        ))
    }

    #[tool(
        name = "group_join",
        description = "Join an existing collaboration group idempotently"
    )]
    fn group_join(&self, Parameters(input): Parameters<GroupInput>) -> Result<Json<Value>, String> {
        self.identity()?;
        join_group(&self.root.join("v1"), &input.name, &self.agent.id).map_err(error)?;
        let client = self.client()?;
        let group = client
            .state
            .groups
            .get(&input.name)
            .ok_or_else(|| "group was not visible after joining".to_owned())?;
        Ok(Json(
            json!({"name": group.name, "created_by": group.created_by, "members": group_members(&client.state, &input.name)}),
        ))
    }

    #[tool(
        name = "message_send",
        description = "Persist a direct or group message and return its ID; reuse an idempotency key for uncertain retries"
    )]
    fn message_send(
        &self,
        Parameters(input): Parameters<SendInput>,
    ) -> Result<Json<Value>, String> {
        let mut client = self.client()?;
        let message = client
            .send(SendRequest {
                to: input.to,
                group: input.group,
                message: input.message,
                tags: input.tags,
                meta: input.meta,
                ttl_seconds: input.ttl_seconds,
                idempotency_key: input.idempotency_key,
            })
            .map_err(error)?;
        Ok(Json(
            json!({"message": message,"persisted":true,"notifications":client.notifications}),
        ))
    }

    #[tool(
        name = "message_reply",
        description = "Persist a reply preserving thread and participants; return the persisted message"
    )]
    fn message_reply(
        &self,
        Parameters(input): Parameters<ReplyInput>,
    ) -> Result<Json<Value>, String> {
        let mut client = self.client()?;
        let message = client
            .reply(ReplyRequest {
                message_id: input.message_id,
                message: input.message,
                tags: input.tags,
                meta: input.meta,
                ttl_seconds: input.ttl_seconds,
                idempotency_key: input.idempotency_key,
            })
            .map_err(error)?;
        Ok(Json(
            json!({"message": message,"persisted":true,"notifications":client.notifications}),
        ))
    }

    #[tool(
        name = "message_get",
        description = "Read one or several relevant messages by exact IDs without consuming notifications"
    )]
    fn message_get(&self, Parameters(input): Parameters<GetInput>) -> Result<Json<Value>, String> {
        let client = self.client()?;
        output(
            query::messages_get(&self.root, &client.state, &self.agent, &input.ids)
                .map_err(error)?,
        )
    }

    #[tool(
        name = "message_history",
        description = "Read a bounded relevant message page with thread, group, sender, tags and cursor filters"
    )]
    fn message_history(
        &self,
        Parameters(input): Parameters<HistoryQuery>,
    ) -> Result<Json<Value>, String> {
        self.history(&input)
    }

    #[tool(
        name = "message_history_page",
        description = "Read history pages; pass next_cursor as before for older history or after for forward reads"
    )]
    fn message_history_page(
        &self,
        Parameters(input): Parameters<HistoryQuery>,
    ) -> Result<Json<Value>, String> {
        self.history(&input)
    }

    #[tool(
        name = "message_search",
        description = "Search relevant message text with optional group, sender, tags and page filters"
    )]
    fn message_search(
        &self,
        Parameters(input): Parameters<HistoryQuery>,
    ) -> Result<Json<Value>, String> {
        if input
            .search
            .as_deref()
            .is_none_or(|text| text.trim().is_empty())
        {
            return Err("provide nonempty search text".to_owned());
        }
        self.history(&input)
    }

    #[tool(
        name = "message_thread",
        description = "Resolve any message ID to its thread and return a bounded conversation page"
    )]
    fn message_thread(
        &self,
        Parameters(input): Parameters<ThreadInput>,
    ) -> Result<Json<Value>, String> {
        let client = self.client()?;
        let parent =
            query::messages_get(&self.root, &client.state, &self.agent, &[input.message_id])
                .map_err(error)?;
        let parent = parent
            .messages
            .first()
            .ok_or_else(|| "message not found in relevant routes".to_owned())?;
        output(
            query::history(
                &self.root,
                &client.state,
                &self.agent,
                &HistoryQuery {
                    thread: Some(parent.thread.clone()),
                    before: input.before,
                    limit: input.limit.unwrap_or(50),
                    ..Default::default()
                },
            )
            .map_err(error)?,
        )
    }

    #[tool(
        name = "inbox_poll",
        description = "Poll relevant inbox messages. No cursor examines only the newest limit messages (default 50), suppressing repeats in this process; use explicit cursors/history for older backlog. Never acknowledges wake delivery. Restart without cursor replays that recent window"
    )]
    async fn inbox_poll(
        &self,
        Parameters(input): Parameters<PollInput>,
    ) -> Result<Json<Value>, String> {
        let deadline =
            Instant::now() + Duration::from_millis(input.timeout_ms.unwrap_or(0).min(30_000));
        loop {
            let client = self.client()?;
            let mut page = query::history(
                &self.root,
                &client.state,
                &self.agent,
                &HistoryQuery {
                    after: input.cursor.clone(),
                    limit: input.limit.unwrap_or(50),
                    ..Default::default()
                },
            )
            .map_err(error)?;
            if input.cursor.is_none() {
                let mut seen = self
                    .inbox_seen
                    .lock()
                    .map_err(|_| "inbox state lock poisoned".to_owned())?;
                page.messages.retain(|message| !seen.contains(&message.id));
                seen.extend(page.messages.iter().map(|message| message.id.clone()));
                while seen.len() > 4096 {
                    seen.pop_first();
                }
            }
            // History cursors page backwards by default. Inbox resumption
            // always moves forward and must remain available at the live tail.
            page.next_cursor = page
                .messages
                .iter()
                .map(|message| message.id.clone())
                .max()
                .or_else(|| input.cursor.clone());
            if !page.messages.is_empty() || Instant::now() >= deadline {
                return output(page);
            }
            tokio::time::sleep(
                self.poll_interval
                    .min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
    }

    #[tool(
        name = "status",
        description = "Inspect board, identity and wake listener state without reading raw files"
    )]
    fn status(&self) -> Result<Json<Value>, String> {
        Ok(Json(self.read_client()?.status().map_err(error)?))
    }

    #[tool(
        name = "doctor",
        description = "Diagnose board and listener problems and return actionable findings"
    )]
    fn doctor(&self) -> Result<Json<Value>, String> {
        Ok(Json(self.read_client()?.doctor().map_err(error)?))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ZboardServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::new("zboard", env!("CARGO_PKG_VERSION"))
            .with_title("Zboard")
            .with_description("Shared-filesystem coordination for AI-agent conversations")
            .with_website_url("https://github.com/zcourts/zboard");
        info.instructions = Some(format!(
            "Coordinate genuine cross-agent dependencies as {}. Reads never consume wake notifications. Inbox cursors are explicit; persist returned cursors to resume. Send/reply return persisted messages; use idempotency_key for uncertain retries.",
            self.agent.id
        ));
        info
    }
}

pub async fn serve(options: McpOptions) -> Result<()> {
    let service = ZboardServer::new(options)?
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| anyhow!("start MCP stdio transport: {error}"))?;
    service.waiting().await.context("run MCP stdio transport")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn idempotent_send_retries_and_conflicting_tags_are_rejected() {
        let dir = tempdir().unwrap();
        let sender = server(dir.path(), "sender", "one");
        let recipient = server(dir.path(), "recipient", "two");
        let key = ulid::Ulid::new().to_string();
        let request = |tags: Vec<String>| {
            Parameters(SendInput {
                to: vec![recipient.agent.id.clone()],
                group: None,
                message: "durable handoff".into(),
                tags,
                meta: None,
                ttl_seconds: None,
                idempotency_key: Some(key.clone()),
            })
        };
        let first = sender
            .message_send(request(vec!["release".into()]))
            .unwrap()
            .0;
        let second = sender
            .message_send(request(vec!["release".into()]))
            .unwrap()
            .0;
        assert_eq!(first, second);
        assert_eq!(first["message"]["id"], key);
        assert!(sender.message_send(request(vec!["build".into()])).is_err());
        let found = recipient
            .message_get(Parameters(GetInput { ids: vec![key] }))
            .unwrap()
            .0;
        assert_eq!(found["messages"][0]["tags"], json!(["release"]));
    }

    fn server(root: &std::path::Path, project: &str, session: &str) -> ZboardServer {
        ZboardServer::new(McpOptions {
            root: root.to_owned(),
            project: project.to_owned(),
            session: session.to_owned(),
            project_path: root.to_owned(),
            poll_interval: Duration::from_millis(5),
        })
        .unwrap()
    }

    #[test]
    fn direct_send_get_reply_and_filters_share_api() {
        let dir = tempdir().unwrap();
        let sender = server(dir.path(), "sender", "one");
        let recipient = server(dir.path(), "recipient", "two");
        let sent = sender
            .message_send(Parameters(SendInput {
                to: vec![recipient.agent.id.clone()],
                group: None,
                message: "handoff".into(),
                tags: vec!["release".into()],
                meta: Some(json!({"count": 2})),
                ttl_seconds: None,
                idempotency_key: None,
            }))
            .unwrap()
            .0;
        let id = sent["message"]["id"].as_str().unwrap().to_owned();
        let got = recipient
            .message_get(Parameters(GetInput {
                ids: vec![id.clone()],
            }))
            .unwrap()
            .0;
        assert_eq!(got["messages"][0]["message"], "handoff");
        assert_eq!(got["messages"][0]["meta"]["count"], 2);
        let reply = recipient
            .message_reply(Parameters(ReplyInput {
                message_id: id.clone(),
                message: "received".into(),
                tags: None,
                meta: None,
                ttl_seconds: None,
                idempotency_key: None,
            }))
            .unwrap()
            .0;
        assert_eq!(reply["message"]["reply_to"], id);
        assert_eq!(reply["message"]["tags"][0], "release");
        let thread = sender
            .message_thread(Parameters(ThreadInput {
                message_id: id,
                before: None,
                limit: None,
            }))
            .unwrap()
            .0;
        assert_eq!(thread["messages"].as_array().unwrap().len(), 2);
        let found = recipient
            .message_search(Parameters(HistoryQuery {
                search: Some("handoff".into()),
                ..Default::default()
            }))
            .unwrap()
            .0;
        assert_eq!(found["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn mcp_reads_do_not_change_listener_checkpoint() {
        let dir = tempdir().unwrap();
        let recipient = server(dir.path(), "recipient", "two");
        let sender = server(dir.path(), "sender", "one");
        let version_root = dir.path().join("v1");
        let mut listener = crate::storage::BoardState::default();
        crate::storage::initialize_routing(&version_root, &mut listener, &recipient.agent).unwrap();
        let sent = sender
            .message_send(Parameters(SendInput {
                to: vec![recipient.agent.id.clone()],
                group: None,
                message: "independent".into(),
                tags: vec![],
                meta: None,
                ttl_seconds: None,
                idempotency_key: None,
            }))
            .unwrap()
            .0;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let first = recipient
                .inbox_poll(Parameters(PollInput::default()))
                .await
                .unwrap()
                .0;
            assert_eq!(first["messages"][0]["id"], sent["message"]["id"]);
            assert_eq!(first["next_cursor"], sent["message"]["id"]);
            let second = recipient
                .inbox_poll(Parameters(PollInput::default()))
                .await
                .unwrap()
                .0;
            assert!(second["messages"].as_array().unwrap().is_empty());
            let restarted = server(dir.path(), "recipient", "two");
            let again = restarted
                .inbox_poll(Parameters(PollInput::default()))
                .await
                .unwrap()
                .0;
            assert_eq!(again["messages"][0]["id"], sent["message"]["id"]);
        });
        let scan = crate::storage::scan(&version_root, &mut listener);
        assert!(
            scan.new_messages
                .contains(&sent["message"]["id"].as_str().unwrap().to_owned())
        );
    }
}
