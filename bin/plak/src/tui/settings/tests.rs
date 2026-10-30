use super::{SettingsMenu, SettingsPane, SettingsTheme, SettingsView};
use crate::tui::CompositionSelection;
use crate::tui::settings_registry::{self, SettingKind};

fn menu() -> SettingsMenu {
    SettingsMenu::open(SettingsView::default())
}

#[test]
fn theme_labels_and_cycle_cover_all_six() {
    assert_eq!(
        SettingsTheme::ALL.map(SettingsTheme::as_str),
        [
            "default-dark",
            "default-light",
            "catppuccin-latte",
            "catppuccin-frappe",
            "catppuccin-macchiato",
            "catppuccin-mocha",
        ]
    );
    for (index, theme) in SettingsTheme::ALL.into_iter().enumerate() {
        assert_eq!(theme.next(), SettingsTheme::ALL[(index + 1) % 6]);
        assert_eq!(SettingsTheme::parse(theme.as_str()), Some(theme));
    }
    assert_eq!(
        SettingsTheme::parse("dark"),
        Some(SettingsTheme::DefaultDark)
    );
    assert_eq!(
        SettingsTheme::parse("light"),
        Some(SettingsTheme::DefaultLight)
    );
}

#[test]
fn composition_labels_and_cycle_are_closed_and_stable() {
    let compositions = [
        CompositionSelection::Direct,
        CompositionSelection::FollowCatalog,
        CompositionSelection::NativePtc,
        CompositionSelection::JsPtc,
        CompositionSelection::PyPtc,
        CompositionSelection::PySdk,
    ];
    assert_eq!(
        compositions.map(CompositionSelection::as_str),
        [
            "direct",
            "plakous-default",
            "native-ptc",
            "js-ptc",
            "py-ptc",
            "py-sdk"
        ]
    );
    assert_eq!(CompositionSelection::ALL, compositions);
    for composition in compositions {
        assert_eq!(
            CompositionSelection::parse(composition.as_str()),
            Some(composition)
        );
    }
}

#[test]
fn the_draft_opens_on_the_registry_defaults() {
    let menu = menu();
    for (index, meta) in settings_registry::ROWS.iter().enumerate() {
        match meta.kind {
            SettingKind::Bool { default } => {
                assert_eq!(menu.bool_value(meta.key), Some(default), "{}", meta.key);
            }
            SettingKind::Enum { default, .. } => {
                assert_eq!(menu.enum_value(meta.key), Some(default), "{}", meta.key);
            }
            SettingKind::Int { default, .. } => {
                assert_eq!(menu.int_value(meta.key), Some(default), "{}", meta.key);
            }
            SettingKind::Action => {
                assert_eq!(menu.value_label(index), "");
            }
        }
    }
}

#[test]
fn search_matches_label_description_and_keywords() {
    let mut menu = menu();
    assert_eq!(menu.visible_rows().len(), SettingsMenu::ROW_COUNT);
    for c in "completed activity groups".chars() {
        menu.push_filter_char(c);
    }
    assert_eq!(
        menu.visible_rows()
            .into_iter()
            .map(|index| SettingsMenu::meta(index).unwrap().label)
            .collect::<Vec<_>>(),
        vec!["Group tool calls"],
        "description text is searchable"
    );
    menu.filter.clear();
    for c in "accessibility".chars() {
        menu.push_filter_char(c);
    }
    assert_eq!(
        menu.visible_rows()
            .into_iter()
            .map(|index| SettingsMenu::meta(index).unwrap().label)
            .collect::<Vec<_>>(),
        vec!["Reduced motion"],
        "keywords are searchable"
    );
}

#[test]
fn activating_each_kind_opens_the_right_sub_pane() {
    let mut menu = menu();
    let mouse = settings_registry::index_of(settings_registry::MOUSE).unwrap();
    assert!(menu.activate(mouse).is_none());
    assert_eq!(menu.bool_value(settings_registry::MOUSE), Some(false));
    assert_eq!(menu.pane, SettingsPane::Browse);

    let theme = settings_registry::index_of(settings_registry::THEME).unwrap();
    assert!(menu.activate(theme).is_none());
    assert!(matches!(menu.pane, SettingsPane::Choosing(_)));

    menu.pane = SettingsPane::Browse;
    let lines = settings_registry::index_of(settings_registry::SCROLL_LINES).unwrap();
    assert!(menu.activate(lines).is_none());
    assert!(matches!(menu.pane, SettingsPane::Stepping(_)));

    menu.pane = SettingsPane::Browse;
    let raw = settings_registry::index_of(settings_registry::EDIT_RAW_CONFIG).unwrap();
    assert_eq!(menu.activate(raw), Some(settings_registry::EDIT_RAW_CONFIG));
}

#[test]
fn int_rows_clamp_to_the_registry_bounds() {
    let mut menu = menu();
    menu.set_int(settings_registry::SCROLL_LINES, 0);
    assert_eq!(menu.int_value(settings_registry::SCROLL_LINES), Some(1));
    menu.set_int(settings_registry::SCROLL_LINES, 99);
    assert_eq!(menu.int_value(settings_registry::SCROLL_LINES), Some(10));
}

#[test]
fn reset_restores_the_schema_default() {
    let mut menu = menu();
    let row = settings_registry::index_of(settings_registry::THEME).unwrap();
    menu.draft.tui.theme = SettingsTheme::CatppuccinMocha;
    assert_eq!(SettingsMenu::default_label(row), "default-dark");
    menu.reset_to_default(row);
    assert_eq!(menu.draft.tui.theme, SettingsTheme::DefaultDark);
}
