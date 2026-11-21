use super::*;
use crate::{
    model::{HomeWidget, WidgetPosition},
    state::{Subpage, WIDGETS_SECTION},
    trakt::{Item, Kind, Login, NUVIO_APP},
};

fn item(title: &str, kind: Kind) -> Item {
    Item {
        title: title.into(),
        kind,
        progress: 40.,
        id: "tt0944947".into(),
    }
}

fn connected() -> State {
    let mut state = ready();
    state.preferences.trakt.enabled = true;
    state.preferences.show_weather = false;
    state.trakt.available = true;
    state.trakt.connected = Some(true);
    state.trakt.loaded = true;
    state.trakt.items = vec![
        item(
            "Severance",
            Kind::Episode {
                season: 2,
                number: 5,
            },
        ),
        item("Inception", Kind::Movie { year: Some(2010) }),
        item(
            "Specials",
            Kind::Episode {
                season: 0,
                number: 1,
            },
        ),
    ];
    state
}

fn trakt_controls(state: &State) -> Vec<crate::state::Control> {
    controls(state)
        .into_iter()
        .filter(|control| matches!(control.target, Target::TraktItem(_)))
        .collect()
}

#[test]
fn remote_moves_from_header_through_cards_to_dock_and_back() {
    let mut state = connected();
    assert_eq!(trakt_controls(&state).len(), 3);
    state.focus = state.first_input_target();
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::TraktItem(0));
    state.direction(Direction::Left, &controls(&state));
    assert_eq!(state.focus, Target::TraktItem(0));
    for _ in 0..3 {
        state.direction(Direction::Right, &controls(&state));
    }
    assert_eq!(state.focus, Target::TraktItem(2));
    state.direction(Direction::Down, &controls(&state));
    let dock = state.first_content();
    assert!(matches!(dock, Target::App(_)));
    assert_eq!(state.focus, dock);
    state.direction(Direction::Up, &controls(&state));
    assert_eq!(
        state.focus,
        Target::TraktItem(0),
        "The first dock app sits below the first card"
    );
    state.direction(Direction::Up, &controls(&state));
    assert_eq!(state.focus, state.first_input_target());
}

#[test]
fn weather_setup_stays_between_header_and_cards() {
    let mut state = connected();
    state.preferences.show_weather = true;
    state.preferences.weather_location = None;
    state.focus = state.first_input_target();
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::WeatherConfigure);
    state.direction(Direction::Down, &controls(&state));
    assert_eq!(state.focus, Target::TraktItem(0));
    state.direction(Direction::Up, &controls(&state));
    assert_eq!(state.focus, Target::WeatherConfigure);
}

#[test]
fn cards_launch_nuvio_legacy_with_a_deep_link() {
    let mut state = connected();
    state.focus = Target::TraktItem(0);
    assert_eq!(
        state.activate(),
        [Effect::LaunchWith {
            id: NUVIO_APP.into(),
            params: json!({"url": "nuvio-legacy://play/series/tt0944947/2/5"}),
        }]
    );
    state.focus = Target::TraktItem(2);
    assert_eq!(
        state.activate(),
        [Effect::LaunchWith {
            id: NUVIO_APP.into(),
            params: json!({"url": "nuvio-legacy://meta/series/tt0944947"}),
        }]
    );
    state.launching = true;
    assert!(
        state.activate().is_empty(),
        "No second launch while one is pending"
    );
}

#[test]
fn hidden_or_disconnected_widgets_offer_no_cards() {
    for change in [
        |state: &mut State| state.trakt.connected = Some(false),
        |state: &mut State| state.trakt.available = false,
        |state: &mut State| state.preferences.trakt.enabled = false,
        |state: &mut State| state.trakt.items.clear(),
    ] {
        let mut state = connected();
        change(&mut state);
        assert!(trakt_controls(&state).is_empty());
        state.focus = state.first_input_target();
        state.direction(Direction::Down, &controls(&state));
        assert!(!matches!(state.focus, Target::TraktItem(_)));
    }
}

#[test]
fn focus_follows_removed_cards() {
    let mut state = connected();
    state.focus = Target::TraktItem(2);
    state.trakt.items.truncate(1);
    state.sync_trakt();
    assert_eq!(state.focus, Target::TraktItem(0));
    state.trakt.items.clear();
    state.sync_trakt();
    assert_eq!(state.focus, state.first_content());
}

#[test]
fn account_row_signs_in_with_a_dialog_and_signs_out_directly() {
    let mut state = connected();
    state.trakt.connected = Some(false);
    state.focus = Target::ConfigureWidget(HomeWidget::Trakt);
    state.activate();
    assert_eq!(state.subpage, Some(Subpage::Trakt));
    assert_eq!(state.focus, Target::TraktAccount);
    assert!(
        controls(&state)
            .iter()
            .any(|control| control.target == Target::TraktAccount)
    );
    state.activate();
    assert!(matches!(state.overlay, Some(Overlay::TraktLogin)));
    assert_eq!(state.focus, Target::CloseDialog);
    assert_eq!(state.trakt.login, Login::Requesting);
    state.direction(Direction::Up, &controls(&state));
    assert_eq!(state.focus, Target::CloseDialog);
    state.activate();
    assert!(state.overlay.is_none());
    assert_eq!(state.focus, Target::TraktAccount);
    assert_eq!(state.trakt.login, Login::Idle);

    state.activate();
    state.trakt.connected = Some(true);
    state.trakt.login = Login::Idle;
    state.sync_trakt();
    assert!(
        state.overlay.is_none(),
        "A finished login closes the dialog"
    );
    assert_eq!(state.focus, Target::TraktAccount);

    state.trakt.items = vec![item("Movie", Kind::Movie { year: None })];
    state.activate();
    assert_eq!(state.trakt.connected, Some(false));
    assert!(state.trakt.items.is_empty());
    state.back();
    assert_eq!(state.subpage, None);
    assert_eq!(
        state.focus,
        Target::WidgetCell(HomeWidget::Trakt.default_position().cell())
    );
}

#[test]
fn every_language_renders_each_widget_state_and_the_login_dialog() {
    for language in crate::i18n::Language::ALL {
        for connected_state in [None, Some(false), Some(true)] {
            for (loaded, failed) in [(false, false), (true, false), (true, true)] {
                let mut state = connected();
                state.preferences.language = language;
                state.trakt.connected = connected_state;
                state.trakt.loaded = loaded;
                state.trakt.failed = failed;
                state.trakt.items.clear();
                scene(&state);
            }
        }
        let mut state = connected();
        state.preferences.language = language;
        state.trakt.items[0].title = "A very long show title ".repeat(8);
        state.focus = Target::TraktItem(0);
        scene(&state);
        state.open(2);
        state.section = WIDGETS_SECTION;
        state.subpage = Some(Subpage::Trakt);
        state.focus = Target::TraktAccount;
        scene(&state);
        state.overlay = Some(Overlay::TraktLogin);
        for login in [
            Login::Requesting,
            Login::Failed("traktCodeExpired"),
            Login::Code(crate::trakt::Code {
                user_code: "5055CC52".into(),
                url: "https://auth.trakt.tv/activate".into(),
                qr_size: 1,
                qr: vec![true],
            }),
        ] {
            state.trakt.login = login;
            let controls = controls(&state);
            assert_eq!(controls.len(), 1);
            assert_eq!(controls[0].target, Target::CloseDialog);
        }
    }
}

#[test]
fn trakt_preferences_default_to_a_hidden_bottom_band() {
    let preferences: Preferences = serde_json::from_value(json!({"trakt": "broken"})).unwrap();
    assert!(!preferences.trakt.enabled);
    assert_eq!(
        preferences.trakt.position,
        WidgetPosition { column: 0, row: 3 }
    );
    assert_eq!(HomeWidget::Trakt.span(), (4, 1));
    let mut preferences = Preferences::default();
    preferences.trakt.enabled = true;
    preferences.sanitize();
    assert!(preferences.can_place_widget(HomeWidget::Trakt, preferences.trakt.position));
    assert_eq!(
        preferences.available_widget(HomeWidget::Trakt),
        None,
        "An added Trakt widget is not offered again"
    );
    preferences.trakt.enabled = false;
    assert_eq!(
        preferences.available_widget(HomeWidget::Trakt).is_some(),
        crate::trakt::configured(),
        "Builds without Trakt credentials do not offer the widget"
    );
}
