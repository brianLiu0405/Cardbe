use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameInput {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBoardInput {
    pub workspace_id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateMemberInput {
    pub name: String,
    pub role: String,
    pub device_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberStatusInput {
    pub status: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessInput {
    pub member_id: String,
    #[serde(deserialize_with = "required_role")]
    pub role: Option<String>,
}

fn required_role<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandInput {
    pub operation_id: String,
    pub command: Command,
}

fn nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Command {
    #[serde(rename = "task.create")]
    TaskCreate {
        title: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        assignee_id: Option<String>,
    },
    #[serde(rename = "task.update")]
    TaskUpdate {
        task_id: String,
        base_version: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(
            default,
            deserialize_with = "nullable",
            skip_serializing_if = "Option::is_none"
        )]
        assignee_id: Option<Option<String>>,
    },
    #[serde(rename = "comment.create")]
    CommentCreate { task_id: String, body: String },
}

#[derive(Debug)]
pub enum Action {
    Me,
    ListWorkspaces,
    CreateWorkspace(NameInput),
    ListBoards {
        workspace_id: Option<String>,
    },
    CreateBoard(CreateBoardInput),
    ListMembers,
    CreateMember(CreateMemberInput),
    SetMemberStatus {
        member_id: String,
        input: MemberStatusInput,
    },
    ListDevices,
    CreateDevice {
        member_id: String,
        input: NameInput,
    },
    RevokeDevice {
        device_id: String,
    },
    SetAccess {
        board_id: String,
        input: AccessInput,
    },
    Snapshot {
        board_id: String,
    },
    Command {
        board_id: String,
        input: CommandInput,
    },
    Audit {
        after: i64,
        limit: i64,
    },
}
