use super::*;
use std::fs;

const DEPLOY: &str =
    "---\nname: deploy\ndescription: Ship it.\n---\n# Deploy\n\nRun scripts/release.sh.\n";

struct Fixture {
    _temporary: tempfile::TempDir,
    workspace: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temporary.path()).unwrap();
        let workspace = base.join("repo");
        let home = base.join("home");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&home).unwrap();
        Self {
            _temporary: temporary,
            workspace,
            home,
        }
    }

    fn skill(&self, root: &Path, name: &str, text: &str) {
        fs::create_dir_all(root.join(name)).unwrap();
        fs::write(root.join(name).join("SKILL.md"), text).unwrap();
    }

    fn project(&self) -> PathBuf {
        self.workspace.join(".agents/skills")
    }

    fn user(&self) -> PathBuf {
        self.home.join(".agents/skills")
    }

    fn setting(&self, enabled: bool) -> SkillsSetting {
        SkillsSetting::new(enabled, Some(self.home.clone()))
    }

    fn discover(&self, enabled: bool, frozen: Option<&FrozenSkills>) -> NodeSkills {
        NodeSkills::discover(
            &self.setting(enabled),
            frozen.map_or(SkillsMode::New, SkillsMode::Frozen),
            &self.workspace,
            SkillsPresentation::Claude,
        )
        .unwrap()
    }
}

#[test]
fn the_skill_tool_description_matches_its_pin() {
    let pins = [plakous::tools::PromptPin::new(
        "plak.skills.claude.Skill",
        SKILL_TOOL_DESCRIPTION,
        "cd252db9553d2d397ea46859c8bee1c5631634d288b243868e51bdb8f0b03755",
    )];
    let drifted = plakous::tools::drifted_pins(&pins);
    assert!(drifted.is_empty(), "re-pin: {drifted:?}");
}

#[test]
fn every_node_registers_skill_while_skills_are_on_and_only_claude_presents_it() {
    let fixture = Fixture::new();
    let claude = fixture.discover(true, None);
    let registration = claude.tool_registration().unwrap().unwrap();
    assert_eq!(registration.name().as_str(), "Skill");
    assert!(registration.is_nested_eligible());
    assert!(claude.listing_source().unwrap().is_none());
    assert!(claude.presented());
    for presentation in [
        SkillsPresentation::Codex,
        SkillsPresentation::Grok,
        SkillsPresentation::Pi,
        SkillsPresentation::Neutral,
    ] {
        let node = NodeSkills::discover(
            &fixture.setting(true),
            SkillsMode::New,
            &fixture.workspace,
            presentation,
        )
        .unwrap();
        assert!(node.tool_registration().unwrap().is_some());
        assert!(!node.presented());
    }
    for (inherited, registered) in [(true, true), (false, false)] {
        let node = NodeSkills::discover(
            &fixture.setting(!inherited),
            SkillsMode::Inherited(inherited),
            &fixture.workspace,
            SkillsPresentation::Codex,
        )
        .unwrap();
        assert_eq!(node.tool_registration().unwrap().is_some(), registered);
    }
    let off = fixture.discover(false, None);
    assert!(off.tool_registration().unwrap().is_none());
    assert_eq!(off.read_roots(), [] as [PathBuf; 0]);
}

#[test]
fn a_reopen_follows_what_the_session_froze_not_the_setting() {
    let fixture = Fixture::new();
    fixture.skill(&fixture.project(), "deploy", DEPLOY);
    let created = fixture.discover(true, None);
    let frozen = FrozenSkills {
        on: true,
        listing: created
            .listing_source()
            .unwrap()
            .map(|source| source.content().to_owned()),
    };
    let reopened_off = fixture.discover(false, Some(&frozen));
    assert!(reopened_off.tool_registration().unwrap().is_some());
    assert!(reopened_off.listing_source().unwrap().is_none());
    assert!(reopened_off.notice().is_none());
    assert_eq!(
        skill_result(
            reopened_off.discovery.skills(),
            &LocalInstructionSourceReader,
            "deploy",
            None
        )
        .unwrap()
        .lines()
        .next(),
        Some("Launching skill: deploy")
    );

    let from_main = FrozenSkills::default();
    let reopened_on = fixture.discover(true, Some(&from_main));
    assert!(reopened_on.tool_registration().unwrap().is_none());
    assert_eq!(reopened_on.read_roots(), [] as [PathBuf; 0]);
}

#[test]
fn only_the_user_root_enters_the_read_exception() {
    let fixture = Fixture::new();
    fixture.skill(&fixture.project(), "deploy", DEPLOY);
    fixture.skill(&fixture.user(), "notes", &DEPLOY.replace("deploy", "notes"));
    let node = fixture.discover(true, None);
    assert_eq!(node.read_roots(), [fixture.user()]);
    assert_eq!(node.discovery.skills().len(), 2);
}

#[test]
fn skipped_skills_are_one_notice_on_a_new_construction_only() {
    let fixture = Fixture::new();
    fixture.skill(&fixture.project(), "broken", "no front matter");
    fixture.skill(&fixture.project(), "Bad", &DEPLOY.replace("deploy", "Bad"));
    let notice = fixture.discover(true, None).notice().unwrap();
    assert!(notice.starts_with("skills not listed: skill "), "{notice}");
    assert_eq!(notice.matches("; ").count(), 1, "{notice}");
    let frozen = FrozenSkills {
        on: true,
        listing: None,
    };
    assert!(fixture.discover(true, Some(&frozen)).notice().is_none());
}

#[test]
fn the_skill_tool_loads_exactly_the_listed_skills() {
    let fixture = Fixture::new();
    fixture.skill(&fixture.project(), "deploy", "invalid project skill");
    fixture.skill(&fixture.user(), "deploy", DEPLOY);
    let node = fixture.discover(true, None);
    let skills = node.discovery.skills();
    assert_eq!(
        skill_result(
            skills,
            &LocalInstructionSourceReader,
            "deploy",
            Some(" prod ")
        )
        .unwrap(),
        format!(
            "Launching skill: deploy\n\nBase directory for this skill: {}\n\n# Deploy\n\nRun scripts/release.sh.\n\nARGUMENTS: prod",
            fixture.user().join("deploy").display()
        )
    );
    fixture.skill(
        &fixture.project(),
        "later",
        &DEPLOY.replace("deploy", "later"),
    );
    assert_eq!(
        skill_result(skills, &LocalInstructionSourceReader, "later", None).unwrap_err(),
        "Unknown skill: later"
    );
    assert_eq!(
        skill_result(skills, &LocalInstructionSourceReader, "missing", None).unwrap_err(),
        "Unknown skill: missing"
    );
    fs::write(fixture.user().join("deploy/SKILL.md"), "now invalid").unwrap();
    assert!(
        skill_result(skills, &LocalInstructionSourceReader, "deploy", None)
            .unwrap_err()
            .starts_with("Unknown skill: deploy (skill "),
    );
}

#[test]
fn a_skill_cut_by_the_listing_bound_is_not_loadable() {
    let fixture = Fixture::new();
    let description = "d".repeat(1024);
    for index in 0..20 {
        let name = format!("s{index:02}");
        fixture.skill(
            &fixture.project(),
            &name,
            &format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
        );
    }
    let node = fixture.discover(true, None);
    let loadable = node.discovery.listed_skills(SkillsPresentation::Claude);
    let cut = node.discovery.skills()[loadable.len()].name().to_owned();
    assert!(
        skill_result(
            loadable,
            &LocalInstructionSourceReader,
            loadable[0].name(),
            None
        )
        .is_ok()
    );
    assert_eq!(
        skill_result(loadable, &LocalInstructionSourceReader, &cut, None).unwrap_err(),
        format!("Unknown skill: {cut}")
    );
}

/// A notice repeats for neither a spawn nor a fork of the same root session,
/// and shows again for another root session.
#[test]
fn identical_skills_notices_show_once_per_root_session() {
    let mut shown = ShownSkillNotices::default();
    let notice = || vec!["skills not listed: a".to_owned()];
    assert_eq!(shown.fresh("root-1", notice()), notice());
    assert_eq!(shown.fresh("root-1", notice()), Vec::<String>::new());
    let both = vec![
        "skills not listed: a".to_owned(),
        "skills not listed: b".to_owned(),
    ];
    assert_eq!(
        shown.fresh("root-1", both),
        vec!["skills not listed: b".to_owned()]
    );
    assert_eq!(shown.fresh("root-2", notice()), notice());
}
