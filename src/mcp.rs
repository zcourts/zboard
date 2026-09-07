use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::schemars::JsonSchema;
use rmcp::{Json, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Agent, AgentStatus, Message};
use crate::storage::{
    BoardState, MessageDraft, acknowledge_messages, agent_statuses, create_group,
    direct_reply_recipients, ensure_layout, group_members, initialize_routing, join_group,
    message_is_relevant, publish_message, register_agent, relevant_history, scan, touch_presence,
};

pub struct McpOptions {
    pub root: PathBuf,
    pub project: String,
    pub session: String,
    pub project_path: PathBuf,
    pub poll_interval: Duration,
}

struct Inner {
    board: BoardState,
    inbox: VecDeque<Message>,
    pending_ack: Vec<Message>,
}

#[derive(Clone)]
pub struct ZboardServer {
    version_root: PathBuf,
    agent: Agent,
    poll_interval: Duration,
    inner: Arc<Mutex<Inner>>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendInput {
    #[serde(default)]
    to: Vec<String>,
    group: Option<String>,
    message: String,
    meta: Option<Value>,
    ttl_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReplyInput {
    message_id: String,
    message: String,
    meta: Option<Value>,
    ttl_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GroupInput {
    name: String,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct HistoryInput {
    thread: Option<String>,
    group: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct PollInput {
    timeout_ms: Option<u64>,
    limit: Option<usize>,
}

#[derive(Debug, JsonSchema, Serialize)]
struct IdentityOutput {
    agent: Agent,
}

#[derive(Debug, JsonSchema, Serialize)]
struct AgentsOutput {
    agents: Vec<AgentStatus>,
}

#[derive(Debug, JsonSchema, Serialize)]
struct GroupOutput {
    name: String,
    created_by: String,
    members: Vec<String>,
}

#[derive(Debug, JsonSchema, Serialize)]
struct GroupsOutput {
    groups: Vec<GroupOutput>,
}

#[derive(Debug, JsonSchema, Serialize)]
struct MessageOutput {
    message: Message,
}

#[derive(Debug, JsonSchema, Serialize)]
struct MessagesOutput {
    messages: Vec<Message>,
    warnings: Vec<String>,
}

impl ZboardServer {
    fn new(options: McpOptions) -> Result<Self> {
        let version_root = ensure_layout(&options.root)?;
        let agent = register_agent(
            &version_root,
            &options.project,
            &options.session,
            std::env::consts::OS,
            &options.project_path,
        )?;
        touch_presence(&version_root, &agent.id)?;
        let mut board = BoardState::default();
        let resumed = initialize_routing(&version_root, &mut board, &agent)?;
        let initial = scan(&version_root, &mut board);
        let initial_messages: Vec<_> = initial
            .new_messages
            .iter()
            .filter_map(|id| board.messages.get(id).cloned())
            .collect();
        let mut inbox = VecDeque::new();
        if resumed {
            inbox.extend(
                initial_messages
                    .iter()
                    .filter(|message| message_is_relevant(&board, &agent, message))
                    .cloned(),
            );
        } else {
            acknowledge_messages(&version_root, &mut board, &initial_messages)?;
        }
        if !initial.warnings.is_empty() {
            eprintln!(
                "zboard: initial scan warnings: {}",
                initial.warnings.join("; ")
            );
        }
        Ok(Self {
            version_root,
            agent,
            poll_interval: options.poll_interval,
            inner: Arc::new(Mutex::new(Inner {
                board,
                inbox,
                pending_ack: Vec::new(),
            })),
            tool_router: Self::tool_router(),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, String> {
        self.inner
            .lock()
            .map_err(|_| "Zboard state lock is poisoned".to_owned())
    }

    fn refresh(&self, inner: &mut Inner) -> Vec<String> {
        let result = scan(&self.version_root, &mut inner.board);
        for id in result.new_messages {
            if let Some(message) = inner.board.messages.get(&id)
                && message_is_relevant(&inner.board, &self.agent, message)
            {
                inner.inbox.push_back(message.clone());
            }
        }
        result.warnings
    }

    fn touch(&self, inner: &mut Inner) -> Result<(), String> {
        let presence = touch_presence(&self.version_root, &self.agent.id)
            .map_err(|error| format!("{error:#}"))?;
        inner.board.presence.insert(self.agent.id.clone(), presence);
        Ok(())
    }

    fn validate_send(&self, board: &BoardState, input: &SendInput) -> Result<()> {
        if let Some(group) = input.group.as_deref() {
            if group != "global" && group != format!("project:{}", self.agent.project) {
                if !board.groups.contains_key(group) {
                    bail!("unknown group '{group}'");
                }
                if !board
                    .memberships
                    .contains(&(group.to_owned(), self.agent.id.clone()))
                {
                    bail!("join group '{group}' before sending to it");
                }
            }
        } else if input.to.is_empty() {
            bail!("provide at least one recipient or a group");
        } else {
            let unknown: Vec<_> = input
                .to
                .iter()
                .filter(|recipient| !board.agents.contains_key(recipient.as_str()))
                .collect();
            if !unknown.is_empty() {
                bail!("unknown recipient(s): {unknown:?}");
            }
        }
        Ok(())
    }
}

#[tool_router]
impl ZboardServer {
    #[tool(
        name = "identity",
        description = "Return this Zboard agent's registered identity"
    )]
    fn identity(&self) -> Result<Json<IdentityOutput>, String> {
        let mut inner = self.lock()?;
        self.touch(&mut inner)?;
        Ok(Json(IdentityOutput {
            agent: self.agent.clone(),
        }))
    }

    #[tool(
        name = "ping",
        description = "Renew this agent's one-minute online presence"
    )]
    fn ping(&self) -> Result<Json<IdentityOutput>, String> {
        self.identity()
    }

    #[tool(
        name = "agents_list",
        description = "List agents registered on the shared board"
    )]
    fn agents_list(&self) -> Result<Json<AgentsOutput>, String> {
        let mut inner = self.lock()?;
        self.touch(&mut inner)?;
        self.refresh(&mut inner);
        Ok(Json(AgentsOutput {
            agents: agent_statuses(&inner.board),
        }))
    }

    #[tool(
        name = "groups_list",
        description = "List groups and their registered members"
    )]
    fn groups_list(&self) -> Result<Json<GroupsOutput>, String> {
        let mut inner = self.lock()?;
        self.touch(&mut inner)?;
        self.refresh(&mut inner);
        let groups = inner
            .board
            .groups
            .values()
            .map(|group| GroupOutput {
                name: group.name.clone(),
                created_by: group.created_by.clone(),
                members: group_members(&inner.board, &group.name)
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            })
            .collect();
        Ok(Json(GroupsOutput { groups }))
    }

    #[tool(
        name = "group_create",
        description = "Create an idempotent collaboration group"
    )]
    fn group_create(
        &self,
        Parameters(GroupInput { name }): Parameters<GroupInput>,
    ) -> Result<Json<GroupOutput>, String> {
        let mut inner = self.lock()?;
        self.touch(&mut inner)?;
        self.refresh(&mut inner);
        let group = create_group(&self.version_root, &name, &self.agent.id)
            .map_err(|error| format!("{error:#}"))?;
        inner.board.groups.insert(group.name.clone(), group.clone());
        Ok(Json(GroupOutput {
            name: group.name,
            created_by: group.created_by,
            members: Vec::new(),
        }))
    }

    #[tool(
        name = "group_join",
        description = "Join an existing collaboration group idempotently"
    )]
    fn group_join(
        &self,
        Parameters(GroupInput { name }): Parameters<GroupInput>,
    ) -> Result<Json<GroupOutput>, String> {
        let mut inner = self.lock()?;
        self.touch(&mut inner)?;
        self.refresh(&mut inner);
        let membership = join_group(&self.version_root, &name, &self.agent.id)
            .map_err(|error| format!("{error:#}"))?;
        inner
            .board
            .memberships
            .insert((membership.group.clone(), membership.agent));
        let group = inner
            .board
            .groups
            .get(&name)
            .ok_or_else(|| format!("unknown group '{name}'"))?;
        Ok(Json(GroupOutput {
            name: group.name.clone(),
            created_by: group.created_by.clone(),
            members: group_members(&inner.board, &name)
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }))
    }

    #[tool(
        name = "message_send",
        description = "Send a direct or group message; provide exactly one target form"
    )]
    fn message_send(
        &self,
        Parameters(input): Parameters<SendInput>,
    ) -> Result<Json<MessageOutput>, String> {
        let mut inner = self.lock()?;
        self.touch(&mut inner)?;
        self.refresh(&mut inner);
        self.validate_send(&inner.board, &input)
            .map_err(|error| format!("{error:#}"))?;
        let message = publish_message(
            &self.version_root,
            &self.agent.id,
            MessageDraft {
                to: input.to,
                group: input.group,
                thread: None,
                reply_to: None,
                body: input.message,
                meta: input.meta,
                ttl_seconds: input.ttl_seconds,
            },
        )
        .map_err(|error| format!("{error:#}"))?;
        acknowledge_messages(
            &self.version_root,
            &mut inner.board,
            std::slice::from_ref(&message),
        )
        .map_err(|error| format!("{error:#}"))?;
        inner
            .board
            .messages
            .insert(message.id.clone(), message.clone());
        Ok(Json(MessageOutput { message }))
    }

    #[tool(
        name = "message_reply",
        description = "Reply to a message while preserving its thread and participants"
    )]
    fn message_reply(
        &self,
        Parameters(input): Parameters<ReplyInput>,
    ) -> Result<Json<MessageOutput>, String> {
        let mut inner = self.lock()?;
        self.touch(&mut inner)?;
        self.refresh(&mut inner);
        let parent = inner
            .board
            .messages
            .get(&input.message_id)
            .cloned()
            .ok_or_else(|| format!("unknown message '{}'", input.message_id))?;
        let group = parent.group.clone();
        let recipients = if group.is_some() {
            Vec::new()
        } else {
            direct_reply_recipients(&parent, &self.agent.id)
        };
        if group.is_none() && recipients.is_empty() {
            return Err("reply has no participant other than the current agent".to_owned());
        }
        let message = publish_message(
            &self.version_root,
            &self.agent.id,
            MessageDraft {
                to: recipients,
                group,
                thread: Some(parent.thread),
                reply_to: Some(parent.id),
                body: input.message,
                meta: input.meta,
                ttl_seconds: input.ttl_seconds,
            },
        )
        .map_err(|error| format!("{error:#}"))?;
        acknowledge_messages(
            &self.version_root,
            &mut inner.board,
            std::slice::from_ref(&message),
        )
        .map_err(|error| format!("{error:#}"))?;
        inner
            .board
            .messages
            .insert(message.id.clone(), message.clone());
        Ok(Json(MessageOutput { message }))
    }

    #[tool(
        name = "message_history",
        description = "Read relevant messages, optionally from one thread or joined group, newest window last"
    )]
    fn message_history(
        &self,
        Parameters(input): Parameters<HistoryInput>,
    ) -> Result<Json<MessagesOutput>, String> {
        let mut inner = self.lock()?;
        self.touch(&mut inner)?;
        let mut warnings = self.refresh(&mut inner);
        let limit = input.limit.unwrap_or(50).min(1_000);
        let (messages, history_warnings) = relevant_history(
            &self.version_root,
            &inner.board,
            &self.agent,
            input.thread.as_deref(),
            input.group.as_deref(),
            limit,
        );
        warnings.extend(history_warnings);
        Ok(Json(MessagesOutput { messages, warnings }))
    }

    #[tool(
        name = "inbox_poll",
        description = "Wait briefly for newly discovered relevant messages"
    )]
    async fn inbox_poll(
        &self,
        Parameters(input): Parameters<PollInput>,
    ) -> Result<Json<MessagesOutput>, String> {
        let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(0).min(30_000));
        let limit = input.limit.unwrap_or(50).clamp(1, 1_000);
        let deadline = Instant::now() + timeout;
        let mut warnings = Vec::new();
        {
            let mut inner = self.lock()?;
            self.touch(&mut inner)?;
            let delivered = std::mem::take(&mut inner.pending_ack);
            if let Err(error) =
                acknowledge_messages(&self.version_root, &mut inner.board, &delivered)
            {
                inner.pending_ack = delivered;
                return Err(format!("{error:#}"));
            }
        }
        loop {
            {
                let mut inner = self.lock()?;
                warnings.extend(self.refresh(&mut inner));
                if !inner.inbox.is_empty() || Instant::now() >= deadline {
                    let count = limit.min(inner.inbox.len());
                    let messages: Vec<_> = inner.inbox.drain(..count).collect();
                    inner.pending_ack.clone_from(&messages);
                    return Ok(Json(MessagesOutput { messages, warnings }));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(self.poll_interval.min(remaining)).await;
        }
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
            "Coordinate only genuine cross-agent dependencies. This server is registered as {}.",
            self.agent.id
        ));
        info
    }
}

pub async fn serve(options: McpOptions) -> Result<()> {
    let server = ZboardServer::new(options)?;
    let service = server
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

    fn options(root: PathBuf, project: &str, session: &str) -> McpOptions {
        McpOptions {
            root,
            project: project.to_owned(),
            session: session.to_owned(),
            project_path: PathBuf::from(format!("/{project}")),
            poll_interval: Duration::from_millis(10),
        }
    }

    #[test]
    fn direct_message_reaches_recipient_inbox() {
        let directory = tempdir().unwrap();
        let sender =
            ZboardServer::new(options(directory.path().to_owned(), "worka", "one")).unwrap();
        let recipient =
            ZboardServer::new(options(directory.path().to_owned(), "infra", "two")).unwrap();

        sender
            .message_send(Parameters(SendInput {
                to: vec![recipient.agent.id.clone()],
                group: None,
                message: "handoff".to_owned(),
                meta: Some(serde_json::json!({"schema":"example.v1","count":2})),
                ttl_seconds: None,
            }))
            .unwrap();

        let mut inner = recipient.lock().unwrap();
        assert!(recipient.refresh(&mut inner).is_empty());
        let received = inner.inbox.pop_front().unwrap();
        assert_eq!(received.message, "handoff");
        assert_eq!(
            received.meta,
            Some(serde_json::json!({"schema":"example.v1","count":2}))
        );
        assert_eq!(received.from, sender.agent.id);
    }

    #[test]
    fn group_send_requires_membership() {
        let directory = tempdir().unwrap();
        let server =
            ZboardServer::new(options(directory.path().to_owned(), "infra", "one")).unwrap();
        let mut inner = server.lock().unwrap();
        let group = create_group(&server.version_root, "release", &server.agent.id).unwrap();
        inner.board.groups.insert(group.name.clone(), group);
        let input = SendInput {
            to: Vec::new(),
            group: Some("release".to_owned()),
            message: "ready".to_owned(),
            meta: None,
            ttl_seconds: None,
        };
        assert!(server.validate_send(&inner.board, &input).is_err());
    }

    #[test]
    fn restart_delivers_messages_received_while_offline() {
        let directory = tempdir().unwrap();
        let recipient_options = options(directory.path().to_owned(), "infra", "two");
        let recipient = ZboardServer::new(recipient_options).unwrap();
        let recipient_id = recipient.agent.id.clone();
        drop(recipient);
        let sender =
            ZboardServer::new(options(directory.path().to_owned(), "worka", "one")).unwrap();
        sender
            .message_send(Parameters(SendInput {
                to: vec![recipient_id],
                group: None,
                message: "while offline".to_owned(),
                meta: None,
                ttl_seconds: None,
            }))
            .unwrap();

        let resumed =
            ZboardServer::new(options(directory.path().to_owned(), "infra", "two")).unwrap();
        let inner = resumed.lock().unwrap();
        assert_eq!(inner.inbox.len(), 1);
        assert_eq!(inner.inbox[0].message, "while offline");
    }

    #[test]
    fn mcp_poll_acknowledges_only_after_the_next_poll() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let directory = tempdir().unwrap();
            let recipient_options = options(directory.path().to_owned(), "infra", "two");
            let recipient = ZboardServer::new(recipient_options).unwrap();
            let recipient_id = recipient.agent.id.clone();
            let sender =
                ZboardServer::new(options(directory.path().to_owned(), "worka", "one")).unwrap();
            sender
                .message_send(Parameters(SendInput {
                    to: vec![recipient_id],
                    group: None,
                    message: "at least once".to_owned(),
                    meta: None,
                    ttl_seconds: None,
                }))
                .unwrap();

            let first = recipient
                .inbox_poll(Parameters(PollInput::default()))
                .await
                .unwrap();
            assert_eq!(first.0.messages.len(), 1);
            drop(recipient);

            let resumed =
                ZboardServer::new(options(directory.path().to_owned(), "infra", "two")).unwrap();
            let replayed = resumed
                .inbox_poll(Parameters(PollInput::default()))
                .await
                .unwrap();
            assert_eq!(replayed.0.messages.len(), 1);
            let acknowledged = resumed
                .inbox_poll(Parameters(PollInput::default()))
                .await
                .unwrap();
            assert!(acknowledged.0.messages.is_empty());
            drop(resumed);

            let final_restart =
                ZboardServer::new(options(directory.path().to_owned(), "infra", "two")).unwrap();
            assert!(final_restart.lock().unwrap().inbox.is_empty());
        });
    }
}
