use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    base: PathBase,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        let home = dir.path().join("home");
        std::fs::create_dir_all(cwd.join("shots")).unwrap();
        std::fs::create_dir_all(home.join("Desktop")).unwrap();
        let png = crate::config::image_attachments::png(4, 3, 16);
        for path in [
            cwd.join("shots/a.png"),
            cwd.join("b c.png"),
            home.join("Desktop/Screenshot 2026-10-16 at 10.23.45\u{202f}AM.png"),
            home.join("Desktop/it's \"q\".png"),
            home.join("cat.png"),
        ] {
            std::fs::write(path, &png).unwrap();
        }
        std::fs::write(cwd.join("notes.png"), "not an image").unwrap();
        Self {
            base: PathBase {
                cwd,
                home: Some(home),
            },
            _dir: dir,
        }
    }

    fn cwd(&self, rest: &str) -> PathBuf {
        self.base.cwd.join(rest)
    }

    fn home(&self, rest: &str) -> PathBuf {
        self.base.home.as_ref().unwrap().join(rest)
    }
}

#[test]
fn every_terminal_drop_form_resolves_to_the_image() {
    let fixture = Fixture::new();
    let screenshot = fixture.home("Desktop/Screenshot 2026-10-16 at 10.23.45\u{202f}AM.png");
    let absolute = screenshot.display().to_string();
    let home = fixture.home("").display().to_string();
    let cases = [
        // macOS Terminal: backslash-escaped spaces, the narrow space literal.
        absolute.replace(' ', "\\ "),
        // A bare path with spaces.
        absolute.clone(),
        format!("'{absolute}'"),
        format!("\"{absolute}\"  \n"),
        "~/Desktop/Screenshot\\ 2026-10-16\\ at\\ 10.23.45\u{202f}AM.png".to_owned(),
        // The narrow space relayed as a plain one.
        "~/Desktop/Screenshot\\ 2026-10-16\\ at\\ 10.23.45\\ AM.png".to_owned(),
        format!(
            "file://{}",
            absolute
                .replace(' ', "%20")
                .replace('\u{202f}', "%E2%80%AF")
        ),
        format!(
            "file://localhost{}/Desktop/Screenshot%202026-10-16%20at%2010.23.45%E2%80%AFAM.png",
            home.trim_end_matches('/')
        ),
    ];
    for case in cases {
        assert_eq!(
            fixture.base.dropped_images(&case),
            Some(vec![screenshot.clone()]),
            "{case:?}"
        );
    }
    assert_eq!(
        fixture.base.dropped_images("shots/a.png"),
        Some(vec![fixture.cwd("shots/a.png")])
    );
    assert_eq!(
        fixture.base.dropped_images("~/cat.png"),
        Some(vec![fixture.home("cat.png")])
    );
    assert_eq!(
        fixture.base.dropped_images(r#"~/Desktop/it\'s\ \"q\".png"#),
        Some(vec![fixture.home("Desktop/it's \"q\".png")])
    );
}

#[test]
fn several_dropped_paths_attach_together_only_when_all_are_images() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture
            .base
            .dropped_images("shots/a.png b\\ c.png\n~/cat.png"),
        Some(vec![
            fixture.cwd("shots/a.png"),
            fixture.cwd("b c.png"),
            fixture.home("cat.png"),
        ])
    );
    // One word that is not an existing image keeps the whole paste text.
    assert_eq!(fixture.base.dropped_images("shots/a.png shots/b.png"), None);
    assert_eq!(
        fixture
            .base
            .dropped_images("shots/a.png \"b c.png\" ~/cat.png"),
        Some(vec![
            fixture.cwd("shots/a.png"),
            fixture.cwd("b c.png"),
            fixture.home("cat.png"),
        ])
    );
}

#[test]
fn pastes_that_are_not_existing_images_stay_text() {
    let fixture = Fixture::new();
    for text in [
        "",
        "   ",
        "hello world",
        "look at shots/a.png",
        "notes.png",
        "shots/missing.png",
        "shots",
        "'shots/a.png",
        "shots/a.png\\",
        "file://relative/a.png",
        "file:///bad%zz.png",
    ] {
        assert_eq!(fixture.base.dropped_images(text), None, "{text:?}");
    }
    let many = vec!["shots/a.png"; MAX_DROPPED_PATHS + 1].join(" ");
    assert_eq!(fixture.base.dropped_images(&many), None);
}

#[test]
fn slash_image_names_one_or_several_files() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.base.named_files("b c.png"),
        vec![fixture.cwd("b c.png")]
    );
    assert_eq!(
        fixture.base.named_files("~/cat.png shots/a.png"),
        vec![fixture.home("cat.png"), fixture.cwd("shots/a.png")]
    );
    assert_eq!(
        fixture.base.named_files("missing.png"),
        vec![fixture.cwd("missing.png")]
    );
    assert_eq!(fixture.base.named_files(" "), Vec::<PathBuf>::new());
}
