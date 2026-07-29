use super::*;

fn client(address: &str, title: &str, rect: Rect) -> HyprClient {
    HyprClient {
        address: address.to_owned(),
        class_name: "google-chrome".to_owned(),
        title: title.to_owned(),
        pid: Some(100),
        workspace_id: Some(1),
        focus_history_id: Some(0),
        rect: Some(rect),
    }
}

fn state_with_clients(active_address: Option<&str>, clients: Vec<HyprClient>) -> HyprlandState {
    HyprlandState {
        active_address: active_address.map(ToOwned::to_owned),
        clients: clients
            .into_iter()
            .map(|client| (client.address.clone(), client))
            .collect(),
        ..HyprlandState::default()
    }
}

#[test]
fn focused_active_hyprland_window_gets_high_confidence_mapping() {
    let hyprland = state_with_clients(
        Some("0xaaa"),
        vec![client(
            "0xaaa",
            "Example - Google Chrome",
            Rect {
                x: 0,
                y: 0,
                width: 1200,
                height: 800,
            },
        )],
    );

    let scores = score_hyprland_candidates(
        "Example - Google Chrome",
        true,
        Some(&Rect {
            x: 0,
            y: 0,
            width: 1200,
            height: 800,
        }),
        Some("7"),
        &hyprland,
        &HashMap::new(),
    );

    assert_eq!(scores[0].client.address, "0xaaa");
    assert!(scores[0].confidence >= 0.95);
    assert!(scores[0]
        .sources
        .contains(&"hyprland_active_window".to_owned()));
    assert!(scores[0].sources.contains(&"geometry_match".to_owned()));
}

#[test]
fn geometry_breaks_ties_between_similar_chrome_titles() {
    let matching_rect = Rect {
        x: 20,
        y: 30,
        width: 900,
        height: 700,
    };
    let hyprland = state_with_clients(
        None,
        vec![
            client("0xaaa", "Docs - Google Chrome", matching_rect.clone()),
            client(
                "0xbbb",
                "Docs - Google Chrome",
                Rect {
                    x: 1500,
                    y: 30,
                    width: 900,
                    height: 700,
                },
            ),
        ],
    );

    let scores = score_hyprland_candidates(
        "Docs - Google Chrome",
        false,
        Some(&matching_rect),
        Some("9"),
        &hyprland,
        &HashMap::new(),
    );

    assert_eq!(scores[0].client.address, "0xaaa");
    assert!(scores[0].confidence > scores[1].confidence);
    assert!(scores[0].sources.contains(&"geometry_match".to_owned()));
}

#[test]
fn cached_mapping_continuity_can_preserve_a_known_window() {
    let hyprland = state_with_clients(
        None,
        vec![client(
            "0xaaa",
            "Different title after navigation",
            Rect {
                x: 0,
                y: 0,
                width: 1000,
                height: 700,
            },
        )],
    );
    let mut mappings = HashMap::new();
    mappings.insert(
        "7".to_owned(),
        WindowMapping {
            address: "0xaaa".to_owned(),
            confidence: 0.9,
            sources: vec!["previous".to_owned()],
        },
    );

    let scores = score_hyprland_candidates(
        "Unrelated active tab title",
        false,
        None,
        Some("7"),
        &hyprland,
        &mappings,
    );
    let mapped = update_mapping_from_candidates(Some("7"), &scores, &mut mappings);

    assert!(mapped.is_some());
    assert_eq!(mappings.get("7").unwrap().address, "0xaaa");
    assert!(mappings
        .get("7")
        .unwrap()
        .sources
        .contains(&"cached_mapping_continuity".to_owned()));
}

#[test]
fn stronger_candidate_evicts_weaker_rival_mapping() {
    let hyprland = state_with_clients(
        Some("0xaaa"),
        vec![client(
            "0xaaa",
            "Shared Page - Google Chrome",
            Rect {
                x: 0,
                y: 0,
                width: 1000,
                height: 700,
            },
        )],
    );
    let mut mappings = HashMap::new();
    mappings.insert(
        "7".to_owned(),
        WindowMapping {
            address: "0xaaa".to_owned(),
            confidence: 0.3,
            sources: vec!["previous".to_owned()],
        },
    );

    let scores = score_hyprland_candidates(
        "Shared Page - Google Chrome",
        true,
        None,
        Some("9"),
        &hyprland,
        &mappings,
    );
    let mapped = update_mapping_from_candidates(Some("9"), &scores, &mut mappings);

    assert!(mapped.is_some());
    assert_eq!(mappings.get("9").unwrap().address, "0xaaa");
    assert!(!mappings.contains_key("7"));
}

#[test]
fn weaker_candidate_does_not_steal_stronger_rival_mapping() {
    let mut recently_focused = client(
        "0xaaa",
        "Docs - Google Chrome",
        Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 700,
        },
    );
    recently_focused.focus_history_id = Some(0);
    let mut other = client(
        "0xbbb",
        "Docs - Google Chrome",
        Rect {
            x: 1500,
            y: 0,
            width: 1000,
            height: 700,
        },
    );
    other.focus_history_id = Some(1);

    let hyprland = state_with_clients(None, vec![recently_focused, other]);
    let mut mappings = HashMap::new();
    mappings.insert(
        "7".to_owned(),
        WindowMapping {
            address: "0xaaa".to_owned(),
            confidence: 0.9,
            sources: vec!["previous".to_owned()],
        },
    );

    let scores = score_hyprland_candidates(
        "Docs - Google Chrome",
        false,
        None,
        Some("9"),
        &hyprland,
        &mappings,
    );
    let mapped = update_mapping_from_candidates(Some("9"), &scores, &mut mappings);

    assert!(mapped.is_some());
    assert_eq!(mappings.get("9").unwrap().address, "0xbbb");
    assert_eq!(mappings.get("7").unwrap().address, "0xaaa");
}

#[test]
fn equal_confidence_candidates_order_by_focus_history() {
    let mut older = client(
        "0xaaa",
        "Docs - Google Chrome",
        Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 700,
        },
    );
    older.focus_history_id = Some(4);
    let mut newer = client(
        "0xbbb",
        "Docs - Google Chrome",
        Rect {
            x: 1500,
            y: 0,
            width: 1000,
            height: 700,
        },
    );
    newer.focus_history_id = Some(1);

    let hyprland = state_with_clients(None, vec![older, newer]);

    let scores = score_hyprland_candidates(
        "Docs - Google Chrome",
        false,
        None,
        Some("9"),
        &hyprland,
        &HashMap::new(),
    );

    assert_eq!(scores[0].confidence, scores[1].confidence);
    assert_eq!(scores[0].client.address, "0xbbb");
}

#[test]
fn stale_mappings_are_removed_when_hyprland_client_disappears() {
    let hyprland = state_with_clients(None, Vec::new());
    let mut mappings = HashMap::new();
    mappings.insert(
        "7".to_owned(),
        WindowMapping {
            address: "0xdead".to_owned(),
            confidence: 0.9,
            sources: vec!["previous".to_owned()],
        },
    );

    prune_stale_window_mappings(&mut mappings, &hyprland);

    assert!(mappings.is_empty());
}

#[tokio::test]
async fn hyprland_close_event_removes_client_and_active_address() {
    let shared = Arc::new(RwLock::new(state_with_clients(
        Some("0xaaa"),
        vec![client(
            "0xaaa",
            "Example",
            Rect {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
        )],
    )));

    apply_hyprland_event(&shared, "closewindow>>0xaaa").await;
    let guard = shared.read().await;

    assert!(guard.clients.is_empty());
    assert_eq!(guard.active_address, None);
    assert_eq!(guard.last_event.as_deref(), Some("closewindow>>0xaaa"));
}

#[tokio::test]
async fn empty_activewindowv2_clears_active_address() {
    let shared = Arc::new(RwLock::new(state_with_clients(
        Some("0xaaa"),
        vec![client(
            "0xaaa",
            "Example",
            Rect {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
        )],
    )));

    apply_hyprland_event(&shared, "activewindowv2>>").await;
    let guard = shared.read().await;

    assert_eq!(guard.active_address, None);
}

#[tokio::test]
async fn hyprland_socket_addresses_are_normalized() {
    let shared = Arc::new(RwLock::new(HyprlandState::default()));

    apply_hyprland_event(&shared, "openwindow>>abc,1,google-chrome,Example").await;
    apply_hyprland_event(&shared, "activewindowv2>>abc").await;
    apply_hyprland_event(&shared, "windowtitlev2>>abc,Updated title").await;
    let guard = shared.read().await;

    assert_eq!(guard.active_address.as_deref(), Some("0xabc"));
    assert_eq!(
        guard
            .clients
            .get("0xabc")
            .map(|client| client.title.as_str()),
        Some("Updated title")
    );
}
