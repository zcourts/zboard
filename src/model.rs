use serde::{Deserialize, Serialize};

pub const AGENT_SCHEMA: &str = "aiboard.agent.v1";
pub const MESSAGE_SCHEMA: &str = "aiboard.message.v1";
pub const GROUP_SCHEMA: &str = "aiboard.group.v1";
pub const MEMBERSHIP_SCHEMA: &str = "aiboard.membership.v1";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Agent {
    pub schema: String,
    pub id: String,
    pub project: String,
    pub session_id: String,
    pub platform: String,
    pub path: String,
    pub registered_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Message {
    pub schema: String,
    pub id: String,
    pub timestamp: String,
    pub from: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub thread: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Group {
    pub schema: String,
    pub name: String,
    pub created_by: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
pub struct Membership {
    pub schema: String,
    pub group: String,
    pub agent: String,
    pub joined_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op")]
pub enum Command {
    #[serde(rename = "send")]
    Send {
        #[serde(default)]
        to: Vec<String>,
        group: Option<String>,
        message: String,
    },
    #[serde(rename = "reply")]
    Reply { to: String, message: String },
    #[serde(rename = "group.create")]
    GroupCreate { name: String },
    #[serde(rename = "group.join")]
    GroupJoin { name: String },
    #[serde(rename = "agents")]
    Agents,
    #[serde(rename = "groups")]
    Groups,
    #[serde(rename = "history")]
    History {
        thread: Option<String>,
        limit: Option<usize>,
    },
    #[serde(rename = "ping")]
    Ping,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum Output<'a> {
    #[serde(rename = "ready")]
    Ready {
        agent: &'a str,
        latest: Option<&'a str>,
    },
    #[serde(rename = "sent")]
    Sent { id: &'a str },
    #[serde(rename = "message")]
    Incoming { message: &'a Message },
    #[serde(rename = "agents")]
    Agents { agents: Vec<&'a Agent> },
    #[serde(rename = "groups")]
    Groups { groups: Vec<GroupSummary<'a>> },
    #[serde(rename = "history")]
    History { messages: Vec<&'a Message> },
    #[serde(rename = "group.created")]
    GroupCreated { name: &'a str },
    #[serde(rename = "group.joined")]
    GroupJoined { name: &'a str },
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "warning")]
    Warning { message: &'a str },
    #[serde(rename = "error")]
    Error {
        op: Option<&'a str>,
        message: &'a str,
    },
}

#[derive(Debug, Serialize)]
pub struct GroupSummary<'a> {
    pub name: &'a str,
    pub created_by: &'a str,
    pub members: Vec<&'a str>,
}
