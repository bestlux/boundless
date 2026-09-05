use super::dashboard_test_support::sample_paired_snapshot;
use super::*;

fn transfer_with_state(state: &str) -> UiFileTransfer {
    UiFileTransfer {
        transfer_id: format!("transfer-{state}"),
        previous_transfer_id: String::new(),
        direction: "outgoing".to_string(),
        peer_id: "peer-1234".to_string(),
        file_name: "notes.txt".to_string(),
        state: state.to_string(),
        transferred_bytes: 0,
        total_bytes: 100,
        failure_reason: String::new(),
        source_path: r"C:\Temp\notes.txt".to_string(),
        final_path: String::new(),
        queued_at: "2026-06-20T00:00:00Z".to_string(),
        updated_at: "2026-06-20T00:00:01Z".to_string(),
    }
}

#[test]
fn transfer_center_action_availability_tracks_state() {
    let queued = transfer_with_state("queued");
    assert!(queued.can_cancel());
    assert!(!queued.can_retry());
    assert!(!queued.is_terminal());

    let failed = transfer_with_state("failed");
    assert!(!failed.can_cancel());
    assert!(failed.can_retry());
    assert!(failed.is_terminal());

    let completed = UiFileTransfer {
        direction: "incoming".to_string(),
        state: "completed".to_string(),
        final_path: r"C:\Downloads\notes.txt".to_string(),
        ..transfer_with_state("completed")
    };
    assert!(completed.can_open_location());
    assert!(completed.is_terminal());
}

#[test]
fn transfer_progress_fraction_clamps_and_handles_zero_byte_completion() {
    let over_reported = UiFileTransfer {
        transferred_bytes: 125,
        total_bytes: 100,
        ..transfer_with_state("active")
    };
    assert_eq!(over_reported.progress_fraction(), 1.0);

    let zero_complete = UiFileTransfer {
        state: "completed".to_string(),
        transferred_bytes: 0,
        total_bytes: 0,
        ..transfer_with_state("completed")
    };
    assert_eq!(zero_complete.progress_fraction(), 1.0);
}

#[test]
fn transfer_summary_counts_visible_states() {
    let mut snapshot = sample_paired_snapshot();
    snapshot.file_transfers = vec![
        transfer_with_state("queued"),
        transfer_with_state("active"),
        transfer_with_state("completed"),
        transfer_with_state("failed"),
        transfer_with_state("cancelled"),
    ];

    assert_eq!(
        dashboard_transfer_center::transfer_summary(&snapshot.file_transfers),
        "2 in progress; 1 needs attention"
    );
}
