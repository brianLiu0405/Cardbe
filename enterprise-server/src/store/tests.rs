use super::*;

struct Fixture {
    store: Store,
    owner: Value,
    workspace: String,
    board: String,
}

fn field(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap().to_owned()
}
fn token(value: &Value) -> String {
    field(value, "token")
}
fn create_workspace(store: &mut Store, token: &str) -> String {
    field(
        &store
            .execute(
                token,
                Action::CreateWorkspace(NameInput {
                    name: "研究工作區".into(),
                }),
            )
            .unwrap(),
        "id",
    )
}
fn create_board(store: &mut Store, token: &str, workspace: &str) -> String {
    field(
        &store
            .execute(
                token,
                Action::CreateBoard(CreateBoardInput {
                    workspace_id: workspace.into(),
                    name: "試點看板".into(),
                }),
            )
            .unwrap(),
        "id",
    )
}
fn create_member(store: &mut Store, token: &str, role: &str) -> Value {
    store
        .execute(
            token,
            Action::CreateMember(CreateMemberInput {
                name: format!("{role} 成員"),
                role: role.into(),
                device_name: "測試裝置".into(),
            }),
        )
        .unwrap()
}
fn grant(
    store: &mut Store,
    owner: &str,
    board: &str,
    user: &str,
    role: Option<&str>,
) -> Result<Value, ApiError> {
    store.execute(
        owner,
        Action::SetAccess {
            board_id: board.into(),
            input: AccessInput {
                member_id: user.into(),
                role: role.map(str::to_owned),
            },
        },
    )
}
fn create_task() -> CommandInput {
    CommandInput {
        operation_id: id(),
        command: Command::TaskCreate {
            title: "中文任務 ✓".into(),
            description: "不寫入稽核的私人描述".into(),
            assignee_id: None,
        },
    }
}
fn submit(
    store: &mut Store,
    token: &str,
    board: &str,
    input: CommandInput,
) -> Result<Value, ApiError> {
    store.execute(
        token,
        Action::Command {
            board_id: board.into(),
            input,
        },
    )
}
impl Fixture {
    fn new() -> Self {
        let mut store = Store::open(":memory:").unwrap();
        let owner = store.bootstrap("公司甲", "擁有者", "主裝置").unwrap();
        let workspace = create_workspace(&mut store, &token(&owner));
        let board = create_board(&mut store, &token(&owner), &workspace);
        Self {
            store,
            owner,
            workspace,
            board,
        }
    }
    fn member(&mut self, role: &str, access: Option<&str>) -> Value {
        let member = create_member(&mut self.store, &token(&self.owner), role);
        if let Some(access) = access {
            grant(
                &mut self.store,
                &token(&self.owner),
                &self.board,
                &field(&member["member"], "id"),
                Some(access),
            )
            .unwrap();
        }
        member
    }
    fn snapshot(&mut self) -> Value {
        self.store
            .execute(
                &token(&self.owner),
                Action::Snapshot {
                    board_id: self.board.clone(),
                },
            )
            .unwrap()
    }
}

#[test]
fn organization_scope_covers_content_members_assignment_and_audit() {
    let mut f = Fixture::new();
    let other = f.store.bootstrap("公司乙", "乙擁有者", "乙裝置").unwrap();
    let other_workspace = create_workspace(&mut f.store, &token(&other));
    let other_board = create_board(&mut f.store, &token(&other), &other_workspace);
    let owner = token(&f.owner);
    assert_eq!(
        f.store.execute(
            &owner,
            Action::Snapshot {
                board_id: other_board.clone()
            }
        ),
        Err(ApiError::NotFound)
    );
    assert_eq!(
        f.store.execute(
            &owner,
            Action::ListBoards {
                workspace_id: Some(other_workspace.clone())
            }
        ),
        Err(ApiError::NotFound)
    );
    assert_eq!(
        f.store.execute(
            &owner,
            Action::CreateBoard(CreateBoardInput {
                workspace_id: other_workspace,
                name: "不可建".into()
            })
        ),
        Err(ApiError::NotFound)
    );
    assert_eq!(
        grant(
            &mut f.store,
            &owner,
            &f.board,
            &field(&other["user"], "id"),
            Some("editor")
        ),
        Err(ApiError::NotFound)
    );
    let input = CommandInput {
        operation_id: id(),
        command: Command::TaskCreate {
            title: "外部指派".into(),
            description: String::new(),
            assignee_id: Some(field(&other["user"], "id")),
        },
    };
    assert_eq!(
        submit(&mut f.store, &owner, &f.board, input),
        Err(ApiError::NotFound)
    );
    let boards = f
        .store
        .execute(&owner, Action::ListBoards { workspace_id: None })
        .unwrap();
    assert_eq!(boards["boards"].as_array().unwrap().len(), 1);
    let audit = f
        .store
        .execute(
            &owner,
            Action::Audit {
                after: 0,
                limit: 200,
            },
        )
        .unwrap();
    assert!(!audit.to_string().contains(&other_board));
    assert!(!audit.to_string().contains(&field(&other["user"], "id")));
    assert!(f.snapshot()["tasks"].as_array().unwrap().is_empty());
}

#[test]
fn viewer_cannot_write_and_admin_does_not_implicitly_read_boards() {
    let mut f = Fixture::new();
    let viewer = f.member("member", Some("viewer"));
    let admin = f.member("admin", None);
    assert!(f
        .store
        .execute(
            &token(&viewer),
            Action::Snapshot {
                board_id: f.board.clone()
            }
        )
        .is_ok());
    assert_eq!(
        submit(&mut f.store, &token(&viewer), &f.board, create_task()),
        Err(ApiError::PermissionDenied)
    );
    assert_eq!(
        f.store.execute(&token(&viewer), Action::ListMembers),
        Err(ApiError::PermissionDenied)
    );
    assert_eq!(
        f.store.execute(&token(&viewer), Action::ListDevices),
        Err(ApiError::PermissionDenied)
    );
    assert_eq!(
        f.store.execute(
            &token(&viewer),
            Action::Audit {
                after: 0,
                limit: 10
            }
        ),
        Err(ApiError::PermissionDenied)
    );
    assert_eq!(
        f.store.execute(
            &token(&admin),
            Action::Snapshot {
                board_id: f.board.clone()
            }
        ),
        Err(ApiError::NotFound)
    );
    assert_eq!(
        f.store
            .execute(&token(&admin), Action::ListBoards { workspace_id: None })
            .unwrap()["boards"],
        json!([])
    );
    assert_eq!(
        f.store
            .execute(&token(&viewer), Action::ListWorkspaces)
            .unwrap()["workspaces"][0]["id"],
        f.workspace
    );
}

#[test]
fn receipt_replay_rechecks_current_board_and_device_permissions() {
    let mut f = Fixture::new();
    let editor = f.member("member", Some("editor"));
    let input = create_task();
    let accepted = submit(&mut f.store, &token(&editor), &f.board, input.clone()).unwrap();
    assert_eq!(accepted["replayed"], false);
    grant(
        &mut f.store,
        &token(&f.owner),
        &f.board,
        &field(&editor["member"], "id"),
        Some("viewer"),
    )
    .unwrap();
    assert_eq!(
        submit(&mut f.store, &token(&editor), &f.board, input.clone()),
        Err(ApiError::PermissionDenied)
    );
    grant(
        &mut f.store,
        &token(&f.owner),
        &f.board,
        &field(&editor["member"], "id"),
        None,
    )
    .unwrap();
    assert_eq!(
        submit(&mut f.store, &token(&editor), &f.board, input.clone()),
        Err(ApiError::NotFound)
    );
    grant(
        &mut f.store,
        &token(&f.owner),
        &f.board,
        &field(&editor["member"], "id"),
        Some("editor"),
    )
    .unwrap();
    f.store
        .execute(
            &token(&f.owner),
            Action::RevokeDevice {
                device_id: field(&editor["device"], "id"),
            },
        )
        .unwrap();
    assert_eq!(
        submit(&mut f.store, &token(&editor), &f.board, input),
        Err(ApiError::Unauthenticated)
    );
    assert_eq!(f.snapshot()["tasks"].as_array().unwrap().len(), 1);
}

struct TestDatabase(std::path::PathBuf);
impl TestDatabase {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("cardbe-enterprise-{}.sqlite", id())))
    }
}
impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

#[test]
fn durable_receipts_survive_reopen_and_do_not_duplicate_task_or_audit() {
    let path = TestDatabase::new();
    let mut store = Store::open(&path.0).unwrap();
    let owner = store.bootstrap("持久公司", "owner", "desktop").unwrap();
    let workspace = create_workspace(&mut store, &token(&owner));
    let board = create_board(&mut store, &token(&owner), &workspace);
    let input = create_task();
    let accepted = submit(&mut store, &token(&owner), &board, input.clone()).unwrap();
    drop(store);
    let mut reopened = Store::open(&path.0).unwrap();
    let replay = submit(&mut reopened, &token(&owner), &board, input).unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(accepted["result"], replay["result"]);
    assert_eq!(accepted["revision"], replay["revision"]);
    let snapshot = reopened
        .execute(&token(&owner), Action::Snapshot { board_id: board })
        .unwrap();
    assert_eq!(snapshot["tasks"].as_array().unwrap().len(), 1);
    let audit = reopened
        .execute(
            &token(&owner),
            Action::Audit {
                after: 0,
                limit: 200,
            },
        )
        .unwrap();
    assert_eq!(
        audit["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["action"] == "task.create" && e["outcome"] == "accepted")
            .count(),
        1
    );
    assert!(!audit.to_string().contains("私人描述"));
}

#[test]
fn operation_ids_are_scoped_to_actor_and_reject_changed_payload_or_board() {
    let mut f = Fixture::new();
    let input = create_task();
    submit(&mut f.store, &token(&f.owner), &f.board, input.clone()).unwrap();
    let other_board = create_board(&mut f.store, &token(&f.owner), &f.workspace);
    assert_eq!(
        submit(&mut f.store, &token(&f.owner), &other_board, input.clone()),
        Err(ApiError::IdempotencyConflict)
    );
    let changed = CommandInput {
        operation_id: input.operation_id.clone(),
        command: Command::TaskCreate {
            title: "不同內容".into(),
            description: String::new(),
            assignee_id: None,
        },
    };
    assert_eq!(
        submit(&mut f.store, &token(&f.owner), &f.board, changed),
        Err(ApiError::IdempotencyConflict)
    );
    let editor = f.member("member", Some("editor"));
    assert!(submit(&mut f.store, &token(&editor), &f.board, input).is_ok());
    assert_eq!(f.snapshot()["tasks"].as_array().unwrap().len(), 2);
}

#[test]
fn stale_update_and_invalid_inputs_do_not_change_accepted_state() {
    let mut f = Fixture::new();
    let created = submit(&mut f.store, &token(&f.owner), &f.board, create_task()).unwrap();
    let task_id = field(&created["result"]["task"], "id");
    let update = |version, title| CommandInput {
        operation_id: id(),
        command: Command::TaskUpdate {
            task_id: task_id.clone(),
            base_version: version,
            title: Some(title),
            description: None,
            status: None,
            assignee_id: None,
        },
    };
    submit(
        &mut f.store,
        &token(&f.owner),
        &f.board,
        update(1, "修改成功".into()),
    )
    .unwrap();
    assert_eq!(
        submit(
            &mut f.store,
            &token(&f.owner),
            &f.board,
            update(1, "過期覆寫".into())
        ),
        Err(ApiError::Conflict)
    );
    assert_eq!(
        submit(
            &mut f.store,
            &token(&f.owner),
            &f.board,
            update(2, "界".repeat(501))
        ),
        Err(ApiError::InvalidArgument)
    );
    let snapshot = f.snapshot();
    assert_eq!(snapshot["board"]["revision"], 2);
    assert_eq!(snapshot["tasks"][0]["title"], "修改成功");
    assert_eq!(snapshot["tasks"][0]["version"], 2);
}

#[test]
fn receipt_failure_rolls_back_content_and_revision_but_records_rejection() {
    let mut f = Fixture::new();
    f.store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_receipt BEFORE INSERT ON operations
         BEGIN SELECT RAISE(FAIL,'synthetic receipt failure'); END;",
        )
        .unwrap();
    let input = create_task();
    assert_eq!(
        submit(&mut f.store, &token(&f.owner), &f.board, input.clone()),
        Err(ApiError::InternalError)
    );
    let snapshot = f.snapshot();
    assert_eq!(snapshot["board"]["revision"], 0);
    assert_eq!(snapshot["tasks"], json!([]));
    let events = f
        .store
        .execute(
            &token(&f.owner),
            Action::Audit {
                after: 0,
                limit: 200,
            },
        )
        .unwrap();
    assert!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["operation_id"] == input.operation_id
                && event["outcome"] == "rejected")
    );
    f.store
        .connection
        .execute_batch("DROP TRIGGER fail_receipt")
        .unwrap();
    assert_eq!(
        submit(&mut f.store, &token(&f.owner), &f.board, input).unwrap()["replayed"],
        false
    );
}

#[test]
fn audit_failure_rolls_back_content_revision_and_receipt() {
    let mut f = Fixture::new();
    f.store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_audit BEFORE INSERT ON audit
         WHEN NEW.action='task.create' AND NEW.outcome='accepted'
         BEGIN SELECT RAISE(ABORT,'synthetic audit failure'); END;",
        )
        .unwrap();
    let input = create_task();
    assert_eq!(
        submit(&mut f.store, &token(&f.owner), &f.board, input.clone()),
        Err(ApiError::InternalError)
    );
    let snapshot = f.snapshot();
    assert_eq!(snapshot["board"]["revision"], 0);
    assert_eq!(snapshot["tasks"], json!([]));
    let receipts: i64 = f
        .store
        .connection
        .query_row("SELECT count(*) FROM operations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(receipts, 0);
    f.store
        .connection
        .execute_batch("DROP TRIGGER fail_audit")
        .unwrap();
    let accepted = submit(&mut f.store, &token(&f.owner), &f.board, input).unwrap();
    assert_eq!(accepted["replayed"], false);
    assert_eq!(f.snapshot()["tasks"].as_array().unwrap().len(), 1);
}

#[test]
fn last_owner_last_board_manager_and_privilege_escalations_are_rejected() {
    let mut f = Fixture::new();
    let admin = f.member("admin", None);
    let another_admin = f.member("admin", None);
    let owner_id = field(&f.owner["user"], "id");
    let suspend = |id: String| Action::SetMemberStatus {
        member_id: id,
        input: MemberStatusInput {
            status: "suspended".into(),
        },
    };
    assert_eq!(
        f.store.execute(&token(&f.owner), suspend(owner_id.clone())),
        Err(ApiError::LastManager)
    );
    assert_eq!(
        f.store.execute(&token(&admin), suspend(owner_id.clone())),
        Err(ApiError::PermissionDenied)
    );
    for status in ["suspended", "active"] {
        assert_eq!(
            f.store.execute(
                &token(&admin),
                Action::SetMemberStatus {
                    member_id: field(&another_admin["member"], "id"),
                    input: MemberStatusInput {
                        status: status.into()
                    }
                }
            ),
            Err(ApiError::PermissionDenied)
        );
    }
    assert_eq!(
        f.store.execute(
            &token(&admin),
            Action::CreateMember(CreateMemberInput {
                name: "越權管理員".into(),
                role: "admin".into(),
                device_name: "裝置".into()
            })
        ),
        Err(ApiError::PermissionDenied)
    );
    assert_eq!(
        f.store.execute(
            &token(&admin),
            Action::CreateDevice {
                member_id: owner_id.clone(),
                input: NameInput {
                    name: "竊取擁有者身分".into()
                }
            }
        ),
        Err(ApiError::PermissionDenied)
    );
    assert_eq!(
        grant(&mut f.store, &token(&f.owner), &f.board, &owner_id, None),
        Err(ApiError::LastManager)
    );
    let manager = f.member("member", Some("manager"));
    grant(
        &mut f.store,
        &token(&f.owner),
        &f.board,
        &owner_id,
        Some("editor"),
    )
    .unwrap();
    assert_eq!(
        f.store
            .execute(&token(&f.owner), suspend(field(&manager["member"], "id"))),
        Err(ApiError::LastManager)
    );
    grant(
        &mut f.store,
        &token(&manager),
        &f.board,
        &owner_id,
        Some("manager"),
    )
    .unwrap();
    f.store
        .execute(&token(&f.owner), suspend(field(&manager["member"], "id")))
        .unwrap();
}

#[test]
fn suspension_does_not_reactivate_old_tokens_and_expired_tokens_fail() {
    let mut f = Fixture::new();
    let editor = f.member("member", Some("editor"));
    let member_id = field(&editor["member"], "id");
    for status in ["suspended", "active"] {
        f.store
            .execute(
                &token(&f.owner),
                Action::SetMemberStatus {
                    member_id: member_id.clone(),
                    input: MemberStatusInput {
                        status: status.into(),
                    },
                },
            )
            .unwrap();
        assert_eq!(
            f.store.execute(&token(&editor), Action::Me),
            Err(ApiError::Unauthenticated)
        );
    }
    let issued = f
        .store
        .execute(
            &token(&f.owner),
            Action::CreateDevice {
                member_id,
                input: NameInput {
                    name: "新裝置".into(),
                },
            },
        )
        .unwrap();
    assert!(f.store.execute(&token(&issued), Action::Me).is_ok());
    f.store
        .connection
        .execute(
            "UPDATE devices SET expires_at=0 WHERE id=?1",
            [field(&issued["device"], "id")],
        )
        .unwrap();
    assert_eq!(
        f.store.execute(&token(&issued), Action::Me),
        Err(ApiError::Unauthenticated)
    );
    let stored: Vec<u8> = f
        .store
        .connection
        .query_row(
            "SELECT token_hash FROM devices WHERE id=?1",
            [field(&issued["device"], "id")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored.len(), 32);
    assert_ne!(stored, token(&issued).as_bytes());
}

#[test]
fn assignment_needs_current_board_access_and_explicit_null_clears_it() {
    let mut f = Fixture::new();
    let member = f.member("member", None);
    let member_id = field(&member["member"], "id");
    let assigned = || CommandInput {
        operation_id: id(),
        command: Command::TaskCreate {
            title: "指派".into(),
            description: String::new(),
            assignee_id: Some(member_id.clone()),
        },
    };
    assert_eq!(
        submit(&mut f.store, &token(&f.owner), &f.board, assigned()),
        Err(ApiError::NotFound)
    );
    grant(
        &mut f.store,
        &token(&f.owner),
        &f.board,
        &member_id,
        Some("viewer"),
    )
    .unwrap();
    let accepted = submit(&mut f.store, &token(&f.owner), &f.board, assigned()).unwrap();
    let task_id = field(&accepted["result"]["task"], "id");
    let clear:CommandInput=serde_json::from_value(json!({"operation_id":id(),"command":{"type":"task.update","task_id":task_id,"base_version":1,"assignee_id":null}})).unwrap();
    let updated = submit(&mut f.store, &token(&f.owner), &f.board, clear).unwrap();
    assert!(updated["result"]["task"]["assignee_id"].is_null());
}

#[test]
fn comment_author_comes_from_token_and_lengths_count_unicode_characters() {
    let mut f = Fixture::new();
    let editor = f.member("member", Some("editor"));
    let accepted = submit(
        &mut f.store,
        &token(&f.owner),
        &f.board,
        CommandInput {
            operation_id: id(),
            command: Command::TaskCreate {
                title: "中".repeat(500),
                description: String::new(),
                assignee_id: None,
            },
        },
    )
    .unwrap();
    let task_id = field(&accepted["result"]["task"], "id");
    let comment = |body| CommandInput {
        operation_id: id(),
        command: Command::CommentCreate {
            task_id: task_id.clone(),
            body,
        },
    };
    let accepted = submit(
        &mut f.store,
        &token(&editor),
        &f.board,
        comment("留言 ✓".into()),
    )
    .unwrap();
    assert_eq!(
        accepted["result"]["comment"]["author_id"],
        editor["member"]["id"]
    );
    assert!(chrono::DateTime::parse_from_rfc3339(
        accepted["result"]["comment"]["created_at"]
            .as_str()
            .unwrap()
    )
    .is_ok());
    assert_eq!(
        submit(
            &mut f.store,
            &token(&editor),
            &f.board,
            comment("中".repeat(10_001))
        ),
        Err(ApiError::InvalidArgument)
    );
    assert_eq!(f.snapshot()["comments"].as_array().unwrap().len(), 1);
    assert!(serde_json::from_value::<CommandInput>(
        json!({"operation_id":id(),"actor_id":id(),"command":{"type":"task.create","title":"偽造"}})
    )
    .is_err());
}

#[test]
fn revoke_commit_precedes_waiting_request_authentication_on_other_connection() {
    let path = TestDatabase::new();
    let mut store = Store::open(&path.0).unwrap();
    let owner = store.bootstrap("競爭測試", "owner", "device").unwrap();
    let mut other = Store::open(&path.0).unwrap();
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute(
        "UPDATE devices SET revoked=1 WHERE id=?1",
        [field(&owner["device"], "id")],
    )
    .unwrap();
    let credential = token(&owner);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let pending = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        other.execute(&credential, Action::Me)
    });
    started_rx.recv().unwrap();
    tx.commit().unwrap();
    assert_eq!(pending.join().unwrap(), Err(ApiError::Unauthenticated));
}

#[test]
fn open_refuses_unrelated_database_without_adding_enterprise_tables() {
    let path = TestDatabase::new();
    let unrelated = Connection::open(&path.0).unwrap();
    unrelated
        .execute("CREATE TABLE cardbe_boards(id INTEGER PRIMARY KEY)", [])
        .unwrap();
    assert!(Store::open(&path.0).is_err());
    let count: i64 = unrelated
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn last_owner_device_cannot_be_revoked_until_a_replacement_is_issued() {
    let mut f = Fixture::new();
    let owner_id = field(&f.owner["user"], "id");
    let old_device = field(&f.owner["device"], "id");
    assert_eq!(
        f.store.execute(
            &token(&f.owner),
            Action::RevokeDevice {
                device_id: old_device.clone()
            }
        ),
        Err(ApiError::LastManager)
    );
    assert!(f.store.execute(&token(&f.owner), Action::Me).is_ok());
    let expired = f
        .store
        .execute(
            &token(&f.owner),
            Action::CreateDevice {
                member_id: owner_id.clone(),
                input: NameInput {
                    name: "已到期替代裝置".into(),
                },
            },
        )
        .unwrap();
    f.store
        .connection
        .execute(
            "UPDATE devices SET expires_at=0 WHERE id=?1",
            [field(&expired["device"], "id")],
        )
        .unwrap();
    assert_eq!(
        f.store.execute(
            &token(&f.owner),
            Action::RevokeDevice {
                device_id: old_device.clone()
            }
        ),
        Err(ApiError::LastManager)
    );
    let replacement = f
        .store
        .execute(
            &token(&f.owner),
            Action::CreateDevice {
                member_id: owner_id,
                input: NameInput {
                    name: "有效替代裝置".into(),
                },
            },
        )
        .unwrap();
    f.store
        .execute(
            &token(&replacement),
            Action::RevokeDevice {
                device_id: old_device,
            },
        )
        .unwrap();
    assert_eq!(
        f.store.execute(&token(&f.owner), Action::Me),
        Err(ApiError::Unauthenticated)
    );
    assert!(f.store.execute(&token(&replacement), Action::Me).is_ok());
}

#[test]
fn host_recovery_issues_only_for_active_same_organization_owner() {
    let mut f = Fixture::new();
    let org = field(&f.owner["organization"], "id");
    let owner = field(&f.owner["user"], "id");
    let admin = f.member("admin", None);
    let member = f.member("member", None);
    let other = f
        .store
        .bootstrap("另一組織", "另一擁有者", "另一裝置")
        .unwrap();
    for target in [&admin, &member] {
        assert_eq!(
            f.store
                .recover_owner(&org, &field(&target["member"], "id"), "不允許的恢復"),
            Err(ApiError::PermissionDenied)
        );
    }
    assert_eq!(
        f.store
            .recover_owner(&org, &field(&other["user"], "id"), "跨組織恢復"),
        Err(ApiError::NotFound)
    );
    f.store
        .connection
        .execute("UPDATE devices SET expires_at=0 WHERE org_id=?1", [&org])
        .unwrap();
    assert_eq!(
        f.store.execute(&token(&f.owner), Action::Me),
        Err(ApiError::Unauthenticated)
    );
    let recovered = f
        .store
        .recover_owner(&org, &owner, "主機管理員恢復")
        .unwrap();
    assert_eq!(recovered["organization"], f.owner["organization"]);
    assert_eq!(recovered["user"], f.owner["user"]);
    assert!(f.store.execute(&token(&recovered), Action::Me).is_ok());
    assert_eq!(
        f.store.execute(&token(&f.owner), Action::Me),
        Err(ApiError::Unauthenticated)
    );
    let events = f
        .store
        .execute(
            &token(&recovered),
            Action::Audit {
                after: 0,
                limit: 200,
            },
        )
        .unwrap();
    assert!(events["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["action"] == "device.owner_recovery"
            && event["device_id"] == recovered["device"]["id"]));
    f.store
        .connection
        .execute("UPDATE users SET status='suspended' WHERE id=?1", [&owner])
        .unwrap();
    assert_eq!(
        f.store.recover_owner(&org, &owner, "停權擁有者"),
        Err(ApiError::PermissionDenied)
    );
}

#[test]
fn recovery_audit_failure_does_not_issue_an_untracked_credential() {
    let mut f = Fixture::new();
    f.store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_recovery_audit BEFORE INSERT ON audit
         WHEN NEW.action='device.owner_recovery'
         BEGIN SELECT RAISE(ABORT,'synthetic recovery audit failure'); END;",
        )
        .unwrap();
    let before: i64 = f
        .store
        .connection
        .query_row("SELECT count(*) FROM devices", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        f.store.recover_owner(
            &field(&f.owner["organization"], "id"),
            &field(&f.owner["user"], "id"),
            "恢復"
        ),
        Err(ApiError::InternalError)
    );
    let after: i64 = f
        .store
        .connection
        .query_row("SELECT count(*) FROM devices", [], |row| row.get(0))
        .unwrap();
    assert_eq!(before, after);
}
