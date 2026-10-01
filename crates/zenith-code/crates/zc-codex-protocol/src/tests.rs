use serde_json::{json, Value};

use crate::*;

#[test]
fn numbers_keep_their_json_form() {
    assert_eq!(serde_json::to_string(&Number(5.0)).unwrap(), "5");
    assert_eq!(serde_json::to_string(&Number(12.5)).unwrap(), "12.5");
    assert_eq!(serde_json::from_str::<Number>("7").unwrap(), Number(7.0));
}

#[test]
fn unknown_enum_values_are_tolerated_and_written_back() {
    let status: TurnStatus = serde_json::from_value(json!("someNewStatus")).unwrap();
    assert_eq!(status, TurnStatus::Unknown("someNewStatus".into()));
    assert_eq!(serde_json::to_value(&status).unwrap(), json!("someNewStatus"));
    assert_eq!(serde_json::from_value::<TurnStatus>(json!("completed")).unwrap(), TurnStatus::Completed);
}

#[test]
fn plan_type_is_a_plain_string() {
    let account: GetAccountResponse =
        serde_json::from_value(json!({"account": {"type": "chatgpt", "email": null, "planType": "a-plan-from-the-future"}, "requiresOpenaiAuth": false}))
            .unwrap();
    let Some(Some(Account::Chatgpt(chatgpt))) = account.account else {
        panic!("chatgpt account expected")
    };
    assert_eq!(chatgpt.plan_type, "a-plan-from-the-future");
}

#[test]
fn tagged_unions_dispatch_on_their_tag_and_keep_unknown_members() {
    let item: ThreadItem = serde_json::from_value(json!({"type": "agentMessage", "id": "m", "text": "hi"})).unwrap();
    assert!(matches!(item, ThreadItem::AgentMessage(_)));
    assert_eq!(item.tag(), Some("agentMessage"));
    let unknown = json!({"type": "brandNewItem", "id": "x", "anything": [1, 2]});
    let item: ThreadItem = serde_json::from_value(unknown.clone()).unwrap();
    assert_eq!(item, ThreadItem::Unknown(unknown.clone()));
    assert_eq!(serde_json::to_value(&item).unwrap(), unknown);
    // A known member with a missing required field does not decode.
    assert!(serde_json::from_value::<ThreadItem>(json!({"type": "agentMessage", "id": "m"})).is_err());
}

#[test]
fn optional_nullable_keys_keep_absent_and_null_apart() {
    let with_null: ConfigWarningNotification = serde_json::from_value(json!({"details": null, "summary": "s"})).unwrap();
    assert_eq!(with_null.details, Some(None));
    assert_eq!(serde_json::to_value(&with_null).unwrap(), json!({"details": null, "summary": "s"}));
    let absent: ConfigWarningNotification = serde_json::from_value(json!({"summary": "s"})).unwrap();
    assert_eq!(absent.details, None);
    assert_eq!(serde_json::to_value(&absent).unwrap(), json!({"summary": "s"}));
}

#[test]
fn required_keys_must_be_present_and_extra_keys_are_dropped() {
    assert!(serde_json::from_value::<ErrorNotification>(json!({"error": {"message": "x"}, "turnId": "t", "willRetry": false})).is_err());
    let decoded: ErrorNotification =
        serde_json::from_value(json!({"error": {"message": "x"}, "threadId": "a", "turnId": "t", "willRetry": false, "extra": 1})).unwrap();
    assert!(serde_json::to_value(decoded).unwrap().get("extra").is_none());
}

#[test]
fn notifications_and_requests_decode_by_method() {
    assert!(ServerNotification::decode("no/such/method", Some(json!({}))).is_none());
    assert!(matches!(
        ServerNotification::decode("turn/started", Some(json!({"threadId": "t"}))),
        Some(Err(_))
    ));
    let decoded = ServerNotification::decode(
        "item/agentMessage/delta",
        Some(json!({"delta": "Hi", "itemId": "i", "threadId": "t", "turnId": "u", "unknownKey": true})),
    )
    .unwrap()
    .unwrap();
    assert_eq!(decoded.method(), "item/agentMessage/delta");
    assert_eq!(decoded.params_value(), json!({"delta": "Hi", "itemId": "i", "threadId": "t", "turnId": "u"}));
    let request = ServerRequest::decode(
        "item/tool/requestUserInput",
        Some(json!({"isBlocking": true, "itemId": "i", "threadId": "t", "turnId": "u", "questions": [{"id": "q", "header": "H", "question": "Q?"}]})),
    )
    .unwrap()
    .unwrap();
    assert_eq!(request.method(), "item/tool/requestUserInput");
    assert!(ServerRequest::decode("item/unknown", None).is_none());
}

#[test]
fn method_tables_cover_what_zenith_code_uses() {
    for method in [
        "thread/started",
        "turn/started",
        "turn/completed",
        "item/started",
        "item/completed",
        "item/agentMessage/delta",
        "thread/tokenUsage/updated",
        "account/rateLimits/updated",
        "serverRequest/resolved",
        "error",
        "rawResponseItem/completed",
    ] {
        assert!(methods::SERVER_NOTIFICATION_METHODS.contains(&method), "{method}");
    }
    for method in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
        "item/permissions/requestApproval",
        "mcpServer/elicitation/request",
        "item/tool/requestUserInput",
        "applyPatchApproval",
        "execCommandApproval",
    ] {
        assert!(methods::SERVER_REQUEST_METHODS.contains(&method), "{method}");
    }
    assert_eq!(methods::SERVER_NOTIFICATION_METHODS.len(), 84);
    const { assert!(!<client_requests::ConfigMcpServerReload as ClientRequest>::HAS_PARAMS) };
    assert_eq!(<client_requests::TurnStart as ClientRequest>::METHOD, "turn/start");
    let _: Value = serde_json::to_value(InitializeParams::default()).unwrap();
}
