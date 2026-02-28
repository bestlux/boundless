use super::dashboard_test_support::{
    sample_guided_flow, sample_pairing_challenge, sample_pairing_result, test_app,
};
use super::*;

fn app_with_active_pairing_flow() -> DashboardApp {
    let mut app = test_app();
    app.begin_pairing_flow(sample_guided_flow());
    app
}

fn app_with_code_entry_flow() -> DashboardApp {
    let mut app = app_with_active_pairing_flow();
    let attempt_id = app
        .active_pairing_attempt_id
        .expect("begin_pairing_flow should create an attempt id");
    app.apply_app_msg(AppMsg::PairingChallenge {
        attempt_id,
        challenge: sample_pairing_challenge(),
    });
    app
}

fn assert_flow_matches(app: &DashboardApp, expected: &GuidedPairingFlow) {
    let flow = app
        .pairing_flow
        .as_ref()
        .expect("expected pairing flow to remain active");
    assert_eq!(flow.dialog_title, expected.dialog_title);
    assert_eq!(flow.host, expected.host);
    assert_eq!(flow.pairing_port, expected.pairing_port);
    assert_eq!(flow.default_alias, expected.default_alias);
    assert_eq!(
        flow.orientation_selector_fallback,
        expected.orientation_selector_fallback
    );
}

fn assert_challenge_matches(app: &DashboardApp, expected: &PairingChallengeState) {
    let challenge = app
        .pairing_challenge
        .as_ref()
        .expect("expected pairing challenge to be present");
    assert_eq!(challenge.request_id, expected.request_id);
    assert_eq!(challenge.verification_nonce, expected.verification_nonce);
    assert_eq!(challenge.expires_at, expected.expires_at);
}

#[test]
fn apply_pairing_challenge_clears_progress_sets_challenge_and_preserves_flow() {
    let expected_flow = sample_guided_flow();
    let challenge = sample_pairing_challenge();
    let mut app = app_with_active_pairing_flow();
    let attempt_id = app
        .active_pairing_attempt_id
        .expect("active flow should have an attempt id");

    app.apply_app_msg(AppMsg::PairingChallenge {
        attempt_id,
        challenge: challenge.clone(),
    });

    assert!(
        !app.pairing_in_progress,
        "challenge should stop the in-progress spinner"
    );
    assert_challenge_matches(&app, &challenge);
    assert_flow_matches(&app, &expected_flow);
}

#[test]
fn empty_code_validation_sets_error_and_keeps_current_flow_active() {
    let expected_flow = sample_guided_flow();
    let expected_challenge = sample_pairing_challenge();
    let mut app = app_with_code_entry_flow();
    app.pairing_code = "   ".to_string();
    app.last_error = None;
    app.last_message_is_error = false;

    app.confirm_pairing_code(egui::Context::default());

    assert_eq!(
        app.last_error.as_deref(),
        Some("pairing code cannot be empty")
    );
    assert!(
        app.last_message_is_error,
        "empty code validation should mark the banner as an error"
    );
    assert!(
        !app.pairing_in_progress,
        "validation failure should not start code submission"
    );
    assert_flow_matches(&app, &expected_flow);
    assert_challenge_matches(&app, &expected_challenge);
}

#[test]
fn cancel_pairing_flow_from_code_entry_clears_challenge_and_flow() {
    let mut app = app_with_code_entry_flow();

    app.cancel_pairing_flow();

    assert!(
        app.pairing_flow.is_none(),
        "cancel should clear the active flow"
    );
    assert!(
        app.pairing_challenge.is_none(),
        "cancel should clear the pending challenge"
    );
    assert!(!app.pairing_in_progress, "cancel should leave pairing idle");
}

#[test]
fn apply_pairing_complete_clears_pairing_state_and_shows_success_banner() {
    let result = sample_pairing_result();
    let mut app = app_with_code_entry_flow();
    app.pairing_in_progress = true;
    app.pairing_retry_available = true;
    let attempt_id = app
        .active_pairing_attempt_id
        .expect("active flow should have an attempt id");

    app.apply_app_msg(AppMsg::PairingComplete {
        attempt_id,
        result: result.clone(),
    });

    assert!(
        !app.pairing_in_progress,
        "completion should stop the in-progress state"
    );
    assert!(
        app.pairing_challenge.is_none(),
        "completion should clear the challenge dialog"
    );
    assert!(
        app.pairing_flow.is_none(),
        "completion should close the pairing flow"
    );
    assert_eq!(app.selected_tab, Tab::Layout);
    assert_eq!(
        app.last_error.as_deref(),
        Some("Pairing successful with peer-mac (selector: Office Desktop)")
    );
    assert!(
        !app.last_message_is_error,
        "completion should show a success banner"
    );
    assert!(
        !app.pairing_retry_available,
        "completion should clear retry affordances"
    );
}

#[test]
fn apply_pairing_failed_preserves_flow_and_formats_error_for_retry_state() {
    let flow = sample_guided_flow();
    let raw_error = "timed out waiting for nearby pairing approval request_id=request-1234";
    let expected_error = anyhow::anyhow!(raw_error.to_string());
    let expected_banner = format_error_for_dialog(&expected_error);
    let mut app = app_with_code_entry_flow();
    app.pairing_in_progress = true;
    let attempt_id = app
        .active_pairing_attempt_id
        .expect("active flow should have an attempt id");

    app.apply_app_msg(AppMsg::PairingFailed {
        attempt_id,
        error: raw_error.to_string(),
    });

    assert!(
        !app.pairing_in_progress,
        "failure should stop the in-progress state"
    );
    assert_flow_matches(&app, &flow);
    assert_eq!(app.last_error.as_deref(), Some(expected_banner.as_str()));
    assert!(
        app.last_message_is_error,
        "failure should show an error banner"
    );
    assert_eq!(
        app.pairing_retry_available,
        should_offer_new_request_retry(&expected_error)
    );
}

#[test]
fn recoverable_pairing_failure_offers_retry() {
    let raw_error = "nearby pairing endpoint closed without a response";
    let mut app = app_with_code_entry_flow();
    let attempt_id = app
        .active_pairing_attempt_id
        .expect("active flow should have an attempt id");

    app.apply_app_msg(AppMsg::PairingFailed {
        attempt_id,
        error: raw_error.to_string(),
    });

    assert!(
        app.pairing_retry_available,
        "recoverable transport failures should offer a new request retry"
    );
}

#[test]
fn pairing_lockout_failure_does_not_offer_retry() {
    let raw_error = "verification temporarily locked after repeated invalid attempts; retry later";
    let mut app = app_with_code_entry_flow();
    let attempt_id = app
        .active_pairing_attempt_id
        .expect("active flow should have an attempt id");

    app.apply_app_msg(AppMsg::PairingFailed {
        attempt_id,
        error: raw_error.to_string(),
    });

    assert!(
        !app.pairing_retry_available,
        "lockout failures should not offer immediate retry"
    );
}

#[test]
fn stale_pairing_messages_are_ignored_after_cancel() {
    let challenge = sample_pairing_challenge();
    let result = sample_pairing_result();
    let mut app = app_with_code_entry_flow();
    let canceled_attempt_id = app
        .active_pairing_attempt_id
        .expect("active flow should have an attempt id");
    app.last_error = Some("keep existing banner".to_string());
    app.last_message_is_error = false;
    app.cancel_pairing_flow();

    app.apply_app_msg(AppMsg::PairingChallenge {
        attempt_id: canceled_attempt_id,
        challenge,
    });
    app.apply_app_msg(AppMsg::PairingComplete {
        attempt_id: canceled_attempt_id,
        result,
    });
    app.apply_app_msg(AppMsg::PairingFailed {
        attempt_id: canceled_attempt_id,
        error: "pairing request rejected by target".to_string(),
    });

    assert!(
        app.pairing_flow.is_none(),
        "late messages must not reopen a canceled flow"
    );
    assert!(
        app.pairing_challenge.is_none(),
        "late messages must not restore stale challenge state"
    );
    assert!(
        !app.pairing_in_progress,
        "late messages must not mark pairing as active"
    );
    assert_eq!(app.selected_tab, Tab::Status);
    assert_eq!(app.last_error.as_deref(), Some("keep existing banner"));
    assert!(
        !app.last_message_is_error,
        "ignored late messages must leave the existing banner type unchanged"
    );
    assert!(
        !app.pairing_retry_available,
        "ignored late messages must not surface retry affordances"
    );
}

#[test]
fn stale_messages_from_a_canceled_attempt_are_ignored_after_restart() {
    let stale_challenge = PairingChallengeState {
        request_id: "request-stale".to_string(),
        verification_nonce: "nonce-stale".to_string(),
        expires_at: "2026-02-27T12:05:00Z".to_string(),
    };
    let fresh_challenge = sample_pairing_challenge();
    let mut app = app_with_active_pairing_flow();
    let canceled_attempt_id = app
        .active_pairing_attempt_id
        .expect("active flow should have an attempt id");

    app.cancel_pairing_flow();
    let restarted_attempt_id = app.begin_pairing_flow(sample_guided_flow());

    app.apply_app_msg(AppMsg::PairingChallenge {
        attempt_id: canceled_attempt_id,
        challenge: stale_challenge,
    });

    assert!(
        app.pairing_challenge.is_none(),
        "stale messages from a canceled attempt must not mutate a restarted flow"
    );
    assert!(
        app.pairing_in_progress,
        "stale messages must not stop the restarted attempt spinner"
    );
    assert_eq!(app.active_pairing_attempt_id, Some(restarted_attempt_id));

    app.apply_app_msg(AppMsg::PairingChallenge {
        attempt_id: restarted_attempt_id,
        challenge: fresh_challenge.clone(),
    });

    assert!(
        !app.pairing_in_progress,
        "the restarted attempt should still accept its own challenge"
    );
    assert_challenge_matches(&app, &fresh_challenge);
}
