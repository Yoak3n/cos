//! 树状会话下的不变量口径：**支线内容对主干不可见是设计，不是漏记**。
//!
//! 回归点：`model-visible-iff-logged` 原先拿「全会话 surface 事件数」去比
//! `derive_messages()`（已按分支游标收窄）的长度，于是只要存在支线，主干就恒报违规。
//! 现在两者共用 `cos_session::visible_events` 的取样范围。

use cos_core::Context;
use cos_invariants::{InvariantRegistry, register_defaults};
use cos_llm::UserMessage;
use cos_session::{Session, SessionEventData, TurnEndReason, visible_events};

/// 主干跑完 turn 1，再开一条支线跑 turn 2，游标交还主干。
fn session_with_a_branch() -> Session {
    let session = Session::new("inv-1");
    session.append(SessionEventData::TurnStart { turn: 1 });
    session.append(SessionEventData::UserMessage(UserMessage::new("主干问")));
    session.append(SessionEventData::TurnEnd {
        turn: 1,
        reason: TurnEndReason::Completed,
    });

    session.open_branch("装饰器", session.last_seq()).unwrap(); // br_1
    session.append(SessionEventData::TurnStart { turn: 2 });
    session.append(SessionEventData::UserMessage(UserMessage::new("支线追问")));
    session.append(SessionEventData::TurnEnd {
        turn: 2,
        reason: TurnEndReason::Completed,
    });

    session.enter_branch(None).unwrap();
    session
}

fn registry() -> InvariantRegistry {
    let registry = InvariantRegistry::new(&Context::root());
    register_defaults(&registry);
    registry
}

#[test]
fn trunk_is_clean_while_a_branch_holds_its_own_messages() {
    let session = session_with_a_branch();
    assert!(session.current_branch().is_none(), "游标应已交还主干");
    let violations = registry().verify(&session);
    assert!(
        violations.is_empty(),
        "主干不该因为存在支线而报违规: {violations:?}"
    );
}

#[test]
fn branch_scope_is_clean_too() {
    let session = session_with_a_branch();
    session.enter_branch(Some("br_1")).unwrap();
    let violations = registry().verify(&session);
    assert!(
        violations.is_empty(),
        "支线视野（祖先 + 自身）同样应自洽: {violations:?}"
    );
}

#[test]
fn visible_events_keeps_sibling_branches_apart() {
    let session = Session::new("inv-2");
    session.append(SessionEventData::UserMessage(UserMessage::new("主干问"))); // seq 1
    session.open_branch("甲", 1).unwrap(); // seq 2 → br_1
    session.append(SessionEventData::UserMessage(UserMessage::new("甲的问题"))); // seq 3
    session.enter_branch(None).unwrap();
    session.open_branch("乙", 1).unwrap(); // seq 4 → br_2
    session.append(SessionEventData::UserMessage(UserMessage::new("乙的问题"))); // seq 5

    let events = session.events();
    let texts = |branch: Option<&str>| -> Vec<String> {
        visible_events(&events, branch)
            .into_iter()
            .filter_map(|event| match &event.data {
                SessionEventData::UserMessage(message) => Some(message.content.clone()),
                _ => None,
            })
            .collect()
    };

    assert_eq!(texts(Some("br_1")), ["主干问", "甲的问题"]);
    assert_eq!(texts(Some("br_2")), ["主干问", "乙的问题"]);
    assert_eq!(texts(None), ["主干问"]);
}
