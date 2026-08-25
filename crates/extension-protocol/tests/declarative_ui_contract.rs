use std::collections::BTreeMap;

use extension_protocol::{
    declarative_ui::{
        UiActionRequest, UiContractError, UiDialogKind, UiDialogRequest, UiDialogResult,
        UiEventSubscriptionOperation, UiStateOperation, UiStatePatch, UiWindowOperation,
        UiWindowRequest, validate_ui_dialog_request, validate_ui_state_patch,
        validate_ui_window_request,
    },
    method,
};

#[test]
fn declarative_ui_action_method_is_a_stable_public_contract() {
    assert_eq!("ui/action", method::UI_ACTION);
    assert!(method::is_known(method::UI_ACTION));
}

#[test]
fn ui_action_request_round_trips_source_path_payload_and_revision() {
    let request = UiActionRequest {
        request_id: "request-1".into(),
        action: "search".into(),
        source_id: "button:search".into(),
        source_path: vec![0, 2, 1],
        payload: BTreeMap::from([
            ("query".into(), "orders".into()),
            ("index".into(), "production".into()),
        ]),
        expected_revision: Some(7),
    };

    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(7, value["expected_revision"]);
    assert_eq!(
        request,
        serde_json::from_value::<UiActionRequest>(value).unwrap()
    );
}

#[test]
fn ui_state_patch_has_explicit_set_and_remove_operations() {
    let patch = UiStatePatch {
        expected_revision: Some(7),
        operations: vec![
            UiStateOperation::Set {
                key: "status".into(),
                value: "ready".into(),
            },
            UiStateOperation::Remove {
                key: "error".into(),
            },
        ],
        event_subscriptions: Vec::new(),
    };

    let value = serde_json::to_value(&patch).unwrap();
    assert_eq!(
        serde_json::json!({
            "expected_revision": 7,
            "operations": [
                {"operation": "set", "key": "status", "value": "ready"},
                {"operation": "remove", "key": "error"}
            ]
        }),
        value
    );
    assert_eq!(
        patch,
        serde_json::from_value::<UiStatePatch>(value).unwrap()
    );
}

#[test]
fn ui_state_patch_event_subscriptions_round_trip_and_validate() {
    let legacy: UiStatePatch =
        serde_json::from_value(serde_json::json!({"operations": []})).unwrap();
    assert!(legacy.event_subscriptions.is_empty());

    let patch = UiStatePatch {
        expected_revision: None,
        operations: Vec::new(),
        event_subscriptions: vec![
            UiEventSubscriptionOperation::Subscribe {
                subscription_id: "pods".into(),
                kind: "kubernetes/pod/watch".into(),
                conn_id: Some(7),
                capacity: Some(64),
                max_events: Some(32),
                wait_ms: Some(1_000),
                state_key: "pods.events".into(),
            },
            UiEventSubscriptionOperation::Unsubscribe {
                subscription_id: "old-pods".into(),
            },
        ],
    };
    validate_ui_state_patch(&patch).unwrap();
    let value = serde_json::to_value(&patch).unwrap();
    assert_eq!("subscribe", value["event_subscriptions"][0]["operation"]);
    assert_eq!(patch, serde_json::from_value(value).unwrap());
}

#[test]
fn dialog_wire_contract_has_explicit_terminal_results() {
    let request = UiDialogRequest {
        request_id: "request-1".into(),
        dialog_id: "delete-topic".into(),
        kind: UiDialogKind::Confirm,
        title: "Delete topic".into(),
        message: Some("This operation cannot be undone.".into()),
        confirm_label: Some("Delete".into()),
        cancel_label: Some("Cancel".into()),
        danger: true,
        expected_revision: Some(7),
    };

    let value = serde_json::to_value(&request).unwrap();
    assert_eq!("confirm", value["kind"]);
    assert_eq!(true, value["danger"]);
    assert_eq!(
        request,
        serde_json::from_value::<UiDialogRequest>(value).unwrap()
    );

    let prompt = UiDialogResult::Prompt {
        value: "orders".into(),
    };
    assert_eq!(
        serde_json::json!({"result": "prompt", "value": "orders"}),
        serde_json::to_value(&prompt).unwrap()
    );
    assert_eq!(
        UiDialogResult::Dismissed,
        serde_json::from_value(serde_json::json!({"result": "dismissed"})).unwrap()
    );
}

#[test]
fn window_wire_contract_uses_tagged_operations() {
    let open = UiWindowRequest {
        request_id: "request-1".into(),
        window_id: "topic-detail".into(),
        operation: UiWindowOperation::Open {
            title: "Topic detail".into(),
            width: 1024,
            height: 768,
            panel_id: "kafka.topic-detail".into(),
            modal: false,
        },
    };
    let value = serde_json::to_value(&open).unwrap();
    assert_eq!(
        serde_json::json!({
            "operation": "open",
            "title": "Topic detail",
            "width": 1024,
            "height": 768,
            "panel_id": "kafka.topic-detail",
            "modal": false
        }),
        value["operation"]
    );
    assert_eq!(
        open,
        serde_json::from_value::<UiWindowRequest>(serde_json::to_value(&open).unwrap()).unwrap()
    );

    let close = UiWindowRequest {
        request_id: "request-2".into(),
        window_id: "topic-detail".into(),
        operation: UiWindowOperation::Close,
    };
    assert_eq!(
        serde_json::json!({"operation": "close"}),
        serde_json::to_value(&close).unwrap()["operation"]
    );

    assert_eq!("ui/dialog", method::UI_DIALOG);
    assert_eq!("ui/window", method::UI_WINDOW);
    assert!(method::is_known(method::UI_DIALOG));
    assert!(method::is_known(method::UI_WINDOW));
}

#[test]
fn dialog_and_window_requests_accept_the_wire_contract_limits() {
    let mut dialog = valid_dialog_request();
    assert_eq!(Ok(()), validate_ui_dialog_request(&dialog));

    dialog.message = Some("First line\nsecond line\r\n\ttabbed".into());
    assert_eq!(Ok(()), validate_ui_dialog_request(&dialog));

    let mut window = valid_window_request();
    assert_eq!(Ok(()), validate_ui_window_request(&window));

    if let UiWindowOperation::Open { width, height, .. } = &mut window.operation {
        *width = 200;
        *height = 16_384;
    }
    assert_eq!(Ok(()), validate_ui_window_request(&window));
}

#[test]
fn dialog_validation_rejects_invalid_ids_titles_messages_and_labels() {
    for invalid_id in ["", "bad id!"] {
        let mut request = valid_dialog_request();
        request.request_id = invalid_id.into();
        assert_eq!(
            Err(UiContractError::InvalidId),
            validate_ui_dialog_request(&request)
        );
        request.request_id = "request-1".into();
        request.dialog_id = invalid_id.into();
        assert_eq!(
            Err(UiContractError::InvalidId),
            validate_ui_dialog_request(&request)
        );
    }

    let mut request = valid_dialog_request();
    request.title = String::new();
    assert_eq!(
        Err(UiContractError::InvalidTitle),
        validate_ui_dialog_request(&request)
    );
    request.title = "bad\n title".into();
    assert_eq!(
        Err(UiContractError::InvalidTitle),
        validate_ui_dialog_request(&request)
    );

    request = valid_dialog_request();
    request.message = Some("bad\u{0}message".into());
    assert_eq!(
        Err(UiContractError::InvalidMessage),
        validate_ui_dialog_request(&request)
    );

    request = valid_dialog_request();
    request.cancel_label = Some(String::new());
    assert_eq!(
        Err(UiContractError::InvalidLabel),
        validate_ui_dialog_request(&request)
    );
    request.confirm_label = Some("bad\nlabel".into());
    assert_eq!(
        Err(UiContractError::InvalidLabel),
        validate_ui_dialog_request(&request)
    );
}

#[test]
fn window_validation_rejects_invalid_titles_and_sizes() {
    let mut request = valid_window_request();
    if let UiWindowOperation::Open { title, .. } = &mut request.operation {
        *title = String::new();
    }
    assert_eq!(
        Err(UiContractError::InvalidTitle),
        validate_ui_window_request(&request)
    );

    let mut request = valid_window_request();
    if let UiWindowOperation::Open { panel_id, .. } = &mut request.operation {
        *panel_id = "bad panel".into();
    }
    assert_eq!(
        Err(UiContractError::InvalidId),
        validate_ui_window_request(&request)
    );

    let mut request = valid_window_request();
    request.operation = UiWindowOperation::SetTitle {
        title: "bad\n title".into(),
    };
    assert_eq!(
        Err(UiContractError::InvalidTitle),
        validate_ui_window_request(&request)
    );

    let mut request = valid_window_request();
    if let UiWindowOperation::Open { width, height, .. } = &mut request.operation {
        *width = 199;
        *height = 16_385;
    }
    assert_eq!(
        Err(UiContractError::InvalidWindowSize),
        validate_ui_window_request(&request)
    );
}

fn valid_dialog_request() -> UiDialogRequest {
    UiDialogRequest {
        request_id: "request-1".into(),
        dialog_id: "delete-topic".into(),
        kind: UiDialogKind::Confirm,
        title: "Delete topic".into(),
        message: Some("This operation cannot be undone.".into()),
        confirm_label: Some("Delete".into()),
        cancel_label: Some("Cancel".into()),
        danger: true,
        expected_revision: Some(7),
    }
}

fn valid_window_request() -> UiWindowRequest {
    UiWindowRequest {
        request_id: "request-1".into(),
        window_id: "topic-detail".into(),
        operation: UiWindowOperation::Open {
            title: "Topic detail".into(),
            width: 1_024,
            height: 768,
            panel_id: "kafka.topic-detail".into(),
            modal: false,
        },
    }
}
