use super::*;
use std::sync::atomic::AtomicUsize;

fn sample_info(label: &str) -> AccountDisplayInfo {
    AccountDisplayInfo {
        account: format!("email {label}"),
        quota_lines: vec!["80%".to_string()],
    }
}

#[test]
fn split_overflow_keeps_first_visible_entries() {
    let entries = vec!["a", "b", "c", "d", "e", "f", "g", "h"];
    let (visible, overflow) =
        split_tray_menu_visible_overflow(entries, TRAY_PLATFORM_MAX_VISIBLE);
    assert_eq!(visible, vec!["a", "b", "c", "d", "e", "f"]);
    assert_eq!(overflow, vec!["g", "h"]);
}

#[test]
fn split_overflow_is_empty_when_under_limit() {
    let (visible, overflow) = split_tray_menu_visible_overflow(vec!["a", "b"], 6);
    assert_eq!(visible, vec!["a", "b"]);
    assert!(overflow.is_empty());
}

#[test]
fn platform_entry_keeps_platform_submenu_id() {
    let entry = TrayMenuEntry::Platform(PlatformId::Codex);
    let snapshot = map_tray_entry_to_snapshot(&entry, |_| sample_info("codex@example.com"));
    match snapshot {
        TrayMenuSnapshotEntry::Platform(platform) => {
            assert_eq!(platform.submenu_id, "platform:codex:submenu");
            assert_eq!(platform.title, "Codex");
            assert_eq!(platform.platform_id, "codex");
            assert_eq!(platform.account, "email codex@example.com");
            assert_eq!(platform.quota_lines, vec!["80%".to_string()]);
        }
        other => panic!("expected platform snapshot, got {other:?}"),
    }
}

#[test]
fn single_platform_group_reuses_group_submenu_id() {
    let entry = TrayMenuEntry::Group {
        id: "ide".to_string(),
        name: "IDE".to_string(),
        platforms: vec![PlatformId::Codex],
    };
    let snapshot = map_tray_entry_to_snapshot(&entry, |_| sample_info("codex@example.com"));
    match snapshot {
        TrayMenuSnapshotEntry::Platform(platform) => {
            assert_eq!(platform.submenu_id, "group:ide:submenu");
            assert_eq!(platform.title, "IDE");
            assert_eq!(platform.platform_id, "codex");
            assert_eq!(platform.account, "email codex@example.com");
        }
        other => panic!("expected flattened platform, got {other:?}"),
    }
}

#[test]
fn multi_platform_group_keeps_nested_platform_ids() {
    let entry = TrayMenuEntry::Group {
        id: "cli".to_string(),
        name: "CLI".to_string(),
        platforms: vec![PlatformId::Codex, PlatformId::Claude],
    };
    let snapshot = map_tray_entry_to_snapshot(&entry, |platform| match platform {
        PlatformId::Codex => sample_info("codex@example.com"),
        PlatformId::Claude => sample_info("claude@example.com"),
        _ => sample_info("other@example.com"),
    });
    match snapshot {
        TrayMenuSnapshotEntry::Group {
            submenu_id,
            name,
            platforms,
        } => {
            assert_eq!(submenu_id, "group:cli:submenu");
            assert_eq!(name, "CLI");
            assert_eq!(platforms.len(), 2);
            assert_eq!(platforms[0].submenu_id, "platform:codex:submenu");
            assert_eq!(platforms[1].submenu_id, "platform:claude_manager:submenu");
            assert_eq!(platforms[1].account, "email claude@example.com");
        }
        other => panic!("expected group snapshot, got {other:?}"),
    }
}

#[test]
fn newer_apply_generation_invalidates_queued_snapshot() {
    let slot = AtomicUsize::new(0);
    let first = next_tray_menu_apply_generation(&slot);
    let second = next_tray_menu_apply_generation(&slot);
    assert!(is_stale_tray_menu_apply(&slot, first));
    assert!(!is_stale_tray_menu_apply(&slot, second));
}
