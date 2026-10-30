use super::{Rgb, Theme, ThemeId};

fn contrast_ratio(foreground: Rgb, background: Rgb) -> f64 {
    fn luminance(color: Rgb) -> f64 {
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.red) + 0.7152 * channel(color.green) + 0.0722 * channel(color.blue)
    }

    let foreground = luminance(foreground);
    let background = luminance(background);
    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
}

#[test]
fn identities_and_compatibility_labels_are_stable() {
    let expected = [
        (ThemeId::DefaultDark, "default-dark"),
        (ThemeId::DefaultLight, "default-light"),
        (ThemeId::CatppuccinLatte, "catppuccin-latte"),
        (ThemeId::CatppuccinFrappe, "catppuccin-frappe"),
        (ThemeId::CatppuccinMacchiato, "catppuccin-macchiato"),
        (ThemeId::CatppuccinMocha, "catppuccin-mocha"),
    ];
    assert_eq!(ThemeId::ALL, expected.map(|(id, _)| id));
    for (id, label) in expected {
        assert_eq!(id.as_str(), label);
        assert_eq!(ThemeId::parse_compatible(label), Some(id));
    }
    assert_eq!(
        ThemeId::parse_compatible("dark"),
        Some(ThemeId::DefaultDark)
    );
    assert_eq!(
        ThemeId::parse_compatible("light"),
        Some(ThemeId::DefaultLight)
    );
    assert_eq!(ThemeId::parse_compatible("sepia"), None);
    assert_eq!(ThemeId::DefaultDark.display_name(), "Default Dark");
    assert_eq!(ThemeId::DefaultLight.display_name(), "Default Light");
    assert_eq!(ThemeId::CatppuccinLatte.display_name(), "Catppuccin Latte");
    assert_eq!(
        ThemeId::CatppuccinFrappe.display_name(),
        "Catppuccin Frappe"
    );
    assert_eq!(
        ThemeId::CatppuccinMacchiato.display_name(),
        "Catppuccin Macchiato"
    );
    assert_eq!(ThemeId::CatppuccinMocha.display_name(), "Catppuccin Mocha");
    assert!(ThemeId::DefaultLight.is_light());
    assert!(ThemeId::CatppuccinLatte.is_light());
    assert!(!ThemeId::DefaultDark.is_light());
    assert!(!ThemeId::CatppuccinMocha.is_light());
    for (index, id) in ThemeId::ALL.into_iter().enumerate() {
        assert_eq!(id.next(), ThemeId::ALL[(index + 1) % ThemeId::ALL.len()]);
    }
}

#[test]
fn catppuccin_flavors_keep_their_exact_base_colors() {
    assert_eq!(
        ThemeId::CatppuccinLatte.colors().background,
        Rgb::new(239, 241, 245)
    );
    assert_eq!(
        ThemeId::CatppuccinFrappe.colors().background,
        Rgb::new(48, 52, 70)
    );
    assert_eq!(
        ThemeId::CatppuccinMacchiato.colors().background,
        Rgb::new(36, 39, 58)
    );
    assert_eq!(
        ThemeId::CatppuccinMocha.colors().background,
        Rgb::new(30, 30, 46)
    );
}

#[test]
fn constructors_preserve_identity_and_light_is_the_default_light_theme() {
    assert_eq!(Theme::dark().id, ThemeId::DefaultDark);
    assert_eq!(Theme::light().id, ThemeId::DefaultLight);
    assert_eq!(Theme::catppuccin_latte().id, ThemeId::CatppuccinLatte);
    assert_eq!(Theme::catppuccin_frappe().id, ThemeId::CatppuccinFrappe);
    assert_eq!(
        Theme::catppuccin_macchiato().id,
        ThemeId::CatppuccinMacchiato
    );
    assert_eq!(Theme::catppuccin_mocha().id, ThemeId::CatppuccinMocha);
}

#[test]
fn every_theme_defines_distinct_readable_command_tokens() {
    for id in ThemeId::ALL {
        let theme = Theme::for_id(id);
        let colors = id.colors();
        let tokens = [
            (theme.command_prompt, colors.muted_text),
            (theme.command_program, colors.accent),
            (theme.command_arg, colors.tool_text),
            (theme.command_string, colors.warning),
        ];
        for (index, (style, rgb)) in tokens.iter().enumerate() {
            assert_eq!(style.fg, Some(rgb.terminal()), "{id:?} token {index}");
            assert!(
                contrast_ratio(*rgb, colors.background) >= 3.0,
                "{id:?} command token {index} is unreadable on the background"
            );
            for (other, (_, other_rgb)) in tokens.iter().enumerate().skip(index + 1) {
                assert_ne!(rgb, other_rgb, "{id:?} tokens {index} and {other} collide");
            }
        }
    }
}

#[test]
fn queue_tokens_sit_on_the_transcript_background_in_every_theme() {
    for id in ThemeId::ALL {
        let theme = Theme::for_id(id);
        let background = id.colors().background.terminal();
        assert_eq!(theme.queue_item.bg, Some(background), "{id:?}");
        assert_eq!(theme.queue_help.bg, Some(background), "{id:?}");
        assert_eq!(
            theme.queue_item.fg,
            Some(id.colors().text.terminal()),
            "{id:?}"
        );
    }
}

#[test]
fn primary_text_and_selection_pairs_meet_normal_text_contrast() {
    for id in ThemeId::ALL {
        let colors = id.colors();
        for (foreground, background) in [
            (colors.text, colors.background),
            (colors.text, colors.surface),
            (colors.strong_text, colors.user_surface),
            (colors.strong_text, colors.raised_surface),
            (colors.chrome_text, colors.surface),
            (colors.tool_text, colors.surface),
            (colors.system_text, colors.surface),
            (colors.menu_text, colors.surface),
            (colors.on_accent, colors.accent),
            (colors.on_selection, colors.selection),
        ] {
            assert!(
                contrast_ratio(foreground, background) >= 4.5,
                "{id:?} has insufficient contrast for {foreground:?} on {background:?}"
            );
        }
        for foreground in [colors.muted_text, colors.faint_text] {
            let minimum = if id == ThemeId::CatppuccinLatte {
                3.0
            } else {
                4.5
            };
            assert!(
                contrast_ratio(foreground, colors.surface) >= minimum,
                "{id:?} has insufficient secondary contrast for {foreground:?}"
            );
        }
    }
}

#[test]
fn default_dark_preserves_established_concrete_colors() {
    use ratatui::style::Color;

    let theme = Theme::dark();
    assert_eq!(theme.app_bg, Color::Rgb(20, 22, 27));
    assert_eq!(theme.chrome.fg, Some(Color::Rgb(190, 194, 204)));
    assert_eq!(theme.sticky_user.fg, Some(Color::Rgb(252, 252, 250)));
    assert_eq!(theme.tool.fg, Some(Color::Rgb(240, 242, 248)));
    assert_eq!(theme.system.fg, Some(Color::Rgb(170, 176, 190)));
    assert_eq!(theme.slash_item.fg, Some(Color::Rgb(230, 232, 238)));
    assert_eq!(theme.queue_item.fg, Some(Color::Rgb(240, 242, 248)));
}

#[test]
fn every_theme_maps_semantic_tokens_to_concrete_styles() {
    for id in ThemeId::ALL {
        let theme = Theme::for_id(id);
        let colors = id.colors();
        assert_eq!(theme.id, id);
        assert_eq!(theme.app_bg, colors.background.terminal());
        assert_eq!(theme.menu_bg, colors.surface.terminal());
        assert_eq!(theme.assistant.fg, Some(colors.text.terminal()));
        assert_eq!(theme.error.fg, Some(colors.error.terminal()));
        assert_eq!(
            theme.transcript_selection,
            ratatui::style::Style::default()
                .fg(colors.on_selection.terminal())
                .bg(colors.selection.terminal())
        );
        assert_eq!(theme.slash_selected.bg, Some(colors.selection.terminal()));
        assert_ne!(theme.user_bg, theme.sticky_user_bg);
    }
}

#[test]
fn prose_emphasis_and_tool_output_use_readable_non_success_tokens() {
    for id in ThemeId::ALL {
        let theme = Theme::for_id(id);
        assert_ne!(theme.assistant_label.fg, theme.success.fg, "{id:?}");
        assert_eq!(theme.tool.fg, theme.assistant.fg, "{id:?}");
    }
    let latte = ThemeId::CatppuccinLatte.colors();
    assert_ne!(latte.strong_text, latte.text);
    assert_ne!(latte.muted_text, latte.text);
    assert_eq!(latte.muted_text, Rgb::new(108, 111, 133));
    assert_eq!(latte.faint_text, Rgb::new(108, 111, 133));
}
