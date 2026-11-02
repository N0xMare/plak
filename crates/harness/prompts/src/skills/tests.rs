use super::*;
use crate::instructions::LocalInstructionSourceReader;
use std::fs;

fn skill_file(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\n\nSteps.\n")
}

fn write_skill(root: &Path, directory: &str, text: &str) {
    fs::create_dir_all(root.join(directory)).unwrap();
    fs::write(root.join(directory).join(SKILL_FILE_NAME), text).unwrap();
}

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

    fn project(&self) -> PathBuf {
        self.workspace.join(PROJECT_SKILLS_DIRECTORY)
    }

    fn user(&self) -> PathBuf {
        self.home.join(USER_SKILLS_DIRECTORY)
    }

    fn discover(&self) -> SkillDiscovery {
        discover_skills(
            &LocalInstructionSourceReader,
            &self.workspace,
            &self.roots(),
        )
        .unwrap()
    }

    fn roots(&self) -> Vec<SkillRoot> {
        vec![
            SkillRoot::project(&self.workspace),
            SkillRoot::user(&self.home),
        ]
    }
}

#[test]
fn missing_roots_list_nothing_and_report_nothing() {
    let fixture = Fixture::new();
    let discovery = fixture.discover();
    assert_eq!(discovery.skills(), [] as [Skill; 0]);
    assert_eq!(discovery.roots(), [] as [DiscoveredRoot; 0]);
    assert_eq!(discovery.diagnostics(), [] as [SkillDiagnostic; 0]);
    assert!(
        discovery
            .registered_source(SkillsPresentation::Claude)
            .unwrap()
            .is_none()
    );
}

#[test]
fn project_skills_come_first_in_name_order_and_win_a_shared_name() {
    let fixture = Fixture::new();
    write_skill(
        &fixture.project(),
        "review",
        &skill_file("review", "Project review."),
    );
    write_skill(
        &fixture.project(),
        "deploy",
        &skill_file("deploy", "Project deploy."),
    );
    write_skill(
        &fixture.user(),
        "review",
        &skill_file("review", "User review."),
    );
    write_skill(
        &fixture.user(),
        "commit",
        &skill_file("commit", "User commit."),
    );
    fs::create_dir_all(fixture.user().join(".hidden")).unwrap();
    fs::write(fixture.user().join("notes.txt"), "not a skill").unwrap();

    let discovery = fixture.discover();
    let listed = discovery
        .skills()
        .iter()
        .map(|skill| (skill.name(), skill.description(), skill.scope()))
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            ("deploy", "Project deploy.", SkillScope::Project),
            ("review", "Project review.", SkillScope::Project),
            ("commit", "User commit.", SkillScope::User),
        ]
    );
    assert_eq!(
        discovery
            .roots()
            .iter()
            .map(|root| (root.scope(), root.path().to_path_buf()))
            .collect::<Vec<_>>(),
        [
            (SkillScope::Project, fixture.project()),
            (SkillScope::User, fixture.user())
        ]
    );
    assert_eq!(
        discovery.skills()[2].file(),
        fixture.user().join("commit/SKILL.md")
    );
    assert_eq!(
        discovery.diagnostics(),
        [SkillDiagnostic {
            path: fixture.user().join("review"),
            problem: SkillProblem::Shadowed,
        }]
    );
}

#[test]
fn front_matter_scalars_are_read_and_descriptions_become_one_line() {
    let fixture = Fixture::new();
    let root = fixture.project();
    write_skill(
        &root,
        "plain",
        "---\nname: plain\ndescription: Runs the\n  release checklist. # note\nmetadata:\n  owner: team\n---\nbody\n",
    );
    write_skill(
        &root,
        "quoted",
        "---\r\nname: \"quoted\"\r\ndescription: 'It''s quoted: yes'\r\n---\r\nbody\r\n",
    );
    write_skill(
        &root,
        "folded",
        "\u{feff}---\nname: folded\ndescription: >-\n  Folded across\n  two lines.\nlicense: MIT\n---\n",
    );
    write_skill(
        &root,
        "literal",
        "---\nname: literal\ndescription: |\n  First line.\n  Second line.\n---\n",
    );
    let discovery = fixture.discover();
    let descriptions = discovery
        .skills()
        .iter()
        .map(|skill| (skill.name(), skill.description()))
        .collect::<Vec<_>>();
    assert_eq!(
        descriptions,
        [
            ("folded", "Folded across two lines."),
            ("literal", "First line. Second line."),
            ("plain", "Runs the release checklist."),
            ("quoted", "It's quoted: yes"),
        ]
    );
    assert!(
        discovery.diagnostics().is_empty(),
        "{:?}",
        discovery.diagnostics()
    );
}

#[test]
fn invalid_skills_are_reported_and_left_out_without_failing_discovery() {
    let fixture = Fixture::new();
    let root = fixture.project();
    write_skill(&root, "good", &skill_file("good", "Fine."));
    write_skill(&root, "no-front-matter", "# just markdown\n");
    write_skill(&root, "unclosed", "---\nname: unclosed\ndescription: x\n");
    write_skill(&root, "Bad_Name", &skill_file("Bad_Name", "Bad."));
    write_skill(&root, "mismatch", &skill_file("other", "Mismatch."));
    write_skill(&root, "no-description", "---\nname: no-description\n---\n");
    write_skill(
        &root,
        "long",
        &skill_file("long", &"x".repeat(MAX_SKILL_DESCRIPTION_CHARS + 1)),
    );
    write_skill(&root, "flow", "---\nname: flow\ndescription: [a, b]\n---\n");
    write_skill(
        &root,
        "twice",
        "---\nname: twice\nname: twice\ndescription: d\n---\n",
    );
    write_skill(
        &root,
        "huge",
        &format!(
            "---\nname: huge\ndescription: Huge.\n# {}\n---\n",
            "y".repeat(MAX_FRONT_MATTER_BYTES)
        ),
    );
    fs::create_dir_all(root.join("empty")).unwrap();
    fs::create_dir_all(root.join("binary")).unwrap();
    fs::write(root.join("binary").join(SKILL_FILE_NAME), [0xff, 0xfe]).unwrap();

    let discovery = fixture.discover();
    assert_eq!(
        discovery
            .skills()
            .iter()
            .map(Skill::name)
            .collect::<Vec<_>>(),
        ["good"]
    );
    let problems = discovery
        .diagnostics()
        .iter()
        .map(|diagnostic| {
            (
                diagnostic
                    .path()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                diagnostic.problem().clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        problems,
        [
            (
                "Bad_Name".to_owned(),
                SkillProblem::InvalidName("may use only lowercase letters, digits and hyphens")
            ),
            ("binary".to_owned(), SkillProblem::InvalidUtf8),
            ("empty".to_owned(), SkillProblem::MissingSkillFile),
            (
                "flow".to_owned(),
                SkillProblem::FrontMatter("has an unsupported value")
            ),
            ("huge".to_owned(), SkillProblem::FrontMatterTooLarge),
            ("long".to_owned(), SkillProblem::DescriptionTooLong),
            (
                "mismatch".to_owned(),
                SkillProblem::NameMismatch {
                    name: "other".to_owned()
                }
            ),
            (
                "no-description".to_owned(),
                SkillProblem::MissingDescription
            ),
            (
                "no-front-matter".to_owned(),
                SkillProblem::FrontMatter("is missing")
            ),
            (
                "twice".to_owned(),
                SkillProblem::FrontMatter("repeats a key")
            ),
            (
                "unclosed".to_owned(),
                SkillProblem::FrontMatter("is not closed")
            ),
        ]
    );
    let shown = discovery.diagnostics()[6].to_string();
    assert!(
        shown.ends_with("names \"other\", not its directory name"),
        "{shown}"
    );
}

#[test]
fn name_rules_follow_the_agent_skills_format() {
    for valid in ["a", "pdf-tools", "x1-y2", &"a".repeat(MAX_SKILL_NAME_CHARS)] {
        assert_eq!(front_matter::name_problem(valid), None, "{valid}");
    }
    for invalid in ["", "-a", "a-", "a--b", "A", "a_b", "a b", &"a".repeat(65)] {
        assert!(front_matter::name_problem(invalid).is_some(), "{invalid}");
    }
}

#[test]
fn a_skill_linked_out_of_its_root_is_refused_and_one_inside_is_listed() {
    let fixture = Fixture::new();
    let outside = fixture.home.join("elsewhere/escape");
    write_skill(
        &fixture.home.join("elsewhere"),
        "escape",
        &skill_file("escape", "Out."),
    );
    write_skill(&fixture.user(), "real", &skill_file("real", "Real."));
    std::os::unix::fs::symlink(&outside, fixture.user().join("escape")).unwrap();

    let discovery = fixture.discover();
    assert_eq!(
        discovery
            .skills()
            .iter()
            .map(Skill::name)
            .collect::<Vec<_>>(),
        ["real"]
    );
    assert_eq!(
        discovery.diagnostics()[0].problem(),
        &SkillProblem::OutsideRoot
    );
}

#[test]
fn discovery_bounds_the_listed_skills() {
    let fixture = Fixture::new();
    for index in 0..=MAX_LISTED_SKILLS {
        let name = format!("s{index:03}");
        write_skill(&fixture.project(), &name, &skill_file(&name, "d"));
    }
    write_skill(&fixture.user(), "unread", &skill_file("unread", "d"));
    let discovery = fixture.discover();
    assert_eq!(discovery.skills().len(), MAX_LISTED_SKILLS);
    let full = discovery.diagnostics().last().unwrap();
    assert_eq!(
        full.problem(),
        &SkillProblem::TooManySkills {
            rest: 1,
            later_roots: 1
        }
    );
    assert!(full.to_string().ends_with(
        "the rest of this directory (1 entries) and 1 later skills directories were not read"
    ));
}

fn two_skills(fixture: &Fixture) -> SkillDiscovery {
    write_skill(
        &fixture.project(),
        "deploy",
        &skill_file("deploy", "Ship <it> & check."),
    );
    write_skill(
        &fixture.user(),
        "commit",
        &skill_file("commit", "Write commits."),
    );
    fixture.discover()
}

#[test]
fn each_family_gets_its_native_listing() {
    let fixture = Fixture::new();
    let discovery = two_skills(&fixture);
    let project = fixture.project();
    let user = fixture.user();
    let render = |presentation| render_skills_listing(presentation, discovery.skills()).unwrap();

    let codex = render(SkillsPresentation::Codex);
    assert!(codex.starts_with(&format!(
        "<skills_instructions>\n## Skills\n{}\n### Skill roots\n- `r0` = `{}`\n- `r1` = `{}`\n### Available skills\n- deploy: Ship <it> & check. (file: r0/deploy/SKILL.md)\n- commit: Write commits. (file: r1/commit/SKILL.md)\n### How to use skills\n- Discovery:",
        listing::CODEX_INTRO,
        project.display(),
        user.display()
    )));
    assert!(codex.ends_with("pick the next-best approach, and continue.\n</skills_instructions>"));

    assert_eq!(
        render(SkillsPresentation::Claude),
        "The following skills are available for use with the Skill tool:\n\n- deploy: Ship <it> & check.\n- commit: Write commits."
    );

    assert_eq!(
        render(SkillsPresentation::Grok),
        format!(
            "The following skills are available for use:\n\n- deploy: Ship <it> & check.\n  Absolute path: {}/deploy/SKILL.md\n- commit: Write commits.\n  Absolute path: {}/commit/SKILL.md\n\nRead a skill's SKILL.md with read_file at its absolute path before following it; read_file also reads the other files in a listed skill's directory.",
            project.display(),
            user.display()
        )
    );

    let pi = render(SkillsPresentation::Pi);
    assert_eq!(
        pi,
        format!(
            "<skills>\nThe following skills provide specialized instructions for specific tasks.\nUse the read tool to load a skill's file when the task matches its description.\nWhen a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n<available_skills>\n  <skill>\n    <name>deploy</name>\n    <description>Ship &lt;it&gt; &amp; check.</description>\n    <location>{}/deploy/SKILL.md</location>\n  </skill>\n  <skill>\n    <name>commit</name>\n    <description>Write commits.</description>\n    <location>{}/commit/SKILL.md</location>\n  </skill>\n</available_skills>\n</skills>",
            project.display(),
            user.display()
        )
    );
    let neutral = render(SkillsPresentation::Neutral);
    assert_eq!(
        neutral,
        pi.replace(
            "Use the read tool to load a skill's file",
            "Load a skill's file"
        )
    );
}

#[test]
fn the_listing_is_byte_bounded_with_an_omission_line() {
    let fixture = Fixture::new();
    let description = "d".repeat(MAX_SKILL_DESCRIPTION_CHARS);
    for index in 0..20 {
        let name = format!("s{index:02}");
        write_skill(&fixture.project(), &name, &skill_file(&name, &description));
    }
    let discovery = fixture.discover();
    assert_eq!(discovery.skills().len(), 20);
    let claude = render_skills_listing(SkillsPresentation::Claude, discovery.skills()).unwrap();
    assert!(claude.len() <= MAX_SKILL_LISTING_BYTES + 200);
    let listed = claude.matches("\n- s").count();
    assert!(listed < 20);
    assert!(claude.ends_with(&format!(
        "- {} additional skills omitted from this bounded skills list.",
        20 - listed
    )));
}

#[test]
fn the_registered_source_is_the_listing_under_one_stable_id() {
    let fixture = Fixture::new();
    let discovery = two_skills(&fixture);
    let source = discovery
        .registered_source(SkillsPresentation::Grok)
        .unwrap()
        .unwrap();
    assert_eq!(source.source_id(), SKILLS_SOURCE_ID);
    assert_eq!(
        source.content(),
        render_skills_listing(SkillsPresentation::Grok, discovery.skills()).unwrap()
    );
    assert!(source.version().starts_with("sha256:"));
    assert_eq!(source.provenance(), "plak-skills-v1:grok");
    assert_eq!(
        crate::instructions::instruction_source_scope_label(SKILLS_SOURCE_ID),
        "skills listing"
    );
}

#[test]
fn a_project_root_that_resolves_outside_the_workspace_is_refused() {
    let fixture = Fixture::new();
    write_skill(
        &fixture.home.join("outside"),
        "deploy",
        &skill_file("deploy", "Out."),
    );
    fs::create_dir_all(fixture.workspace.join(".agents")).unwrap();
    std::os::unix::fs::symlink(
        fixture.home.join("outside"),
        fixture.workspace.join(".agents/skills"),
    )
    .unwrap();
    let discovery = fixture.discover();
    assert_eq!(discovery.skills(), [] as [Skill; 0]);
    assert_eq!(discovery.roots(), [] as [DiscoveredRoot; 0]);
    assert_eq!(
        discovery.diagnostics(),
        [SkillDiagnostic {
            path: fixture.project(),
            problem: SkillProblem::OutsideWorkspace,
        }]
    );
}

#[test]
fn a_project_root_lists_only_paths_the_workspace_read_can_open() {
    let fixture = Fixture::new();
    let real = fixture.workspace.join("tools/skills");
    write_skill(&real, "deploy", &skill_file("deploy", "Deploy."));
    write_skill(
        &fixture.workspace.join("elsewhere"),
        "linked",
        &skill_file("linked", "L."),
    );
    std::os::unix::fs::symlink(
        fixture.workspace.join("elsewhere/linked"),
        real.join("linked"),
    )
    .unwrap();
    write_skill(&real, "file-link", "");
    fs::remove_file(real.join("file-link/SKILL.md")).unwrap();
    std::os::unix::fs::symlink(
        real.join("deploy/SKILL.md"),
        real.join("file-link/SKILL.md"),
    )
    .unwrap();
    fs::create_dir_all(fixture.workspace.join(".agents")).unwrap();
    std::os::unix::fs::symlink(&real, fixture.workspace.join(".agents/skills")).unwrap();

    let discovery = fixture.discover();
    let listed = discovery
        .skills()
        .iter()
        .map(|skill| skill.file().to_path_buf())
        .collect::<Vec<_>>();
    assert_eq!(listed, [real.join("deploy/SKILL.md")]);
    let problems = discovery
        .diagnostics()
        .iter()
        .map(|diagnostic| diagnostic.problem().clone())
        .collect::<Vec<_>>();
    assert_eq!(
        problems,
        [SkillProblem::LinkedPath, SkillProblem::LinkedPath]
    );
}

#[test]
fn a_fifo_skill_file_is_refused_without_blocking() {
    let fixture = Fixture::new();
    for root in [fixture.project(), fixture.user()] {
        fs::create_dir_all(root.join("pipe")).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(root.join("pipe").join(SKILL_FILE_NAME))
            .status()
            .unwrap();
        assert!(status.success());
    }
    let discovery = fixture.discover();
    assert_eq!(discovery.skills(), [] as [Skill; 0]);
    assert_eq!(
        discovery
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.problem().clone())
            .collect::<Vec<_>>(),
        [
            SkillProblem::MissingSkillFile,
            SkillProblem::MissingSkillFile
        ]
    );
}

#[test]
fn a_skill_that_disables_model_invocation_is_not_listed_or_loaded() {
    let fixture = Fixture::new();
    write_skill(
        &fixture.project(),
        "manual",
        "---\nname: manual\ndescription: Only by hand.\ndisable-model-invocation: true\n---\n",
    );
    write_skill(&fixture.project(), "auto", &skill_file("auto", "Auto."));
    let discovery = fixture.discover();
    assert_eq!(
        discovery
            .skills()
            .iter()
            .map(Skill::name)
            .collect::<Vec<_>>(),
        ["auto"]
    );
    assert_eq!(discovery.diagnostics(), [] as [SkillDiagnostic; 0]);

    fs::write(
        fixture.project().join("auto/SKILL.md"),
        "---\nname: auto\ndescription: Auto.\ndisable-model-invocation: true\n---\n",
    )
    .unwrap();
    assert_eq!(
        discovery.skills()[0]
            .load(&LocalInstructionSourceReader)
            .unwrap_err()
            .problem(),
        &SkillProblem::NotModelInvocable
    );
}

#[test]
fn the_front_matter_subset_reads_the_common_yaml_spellings() {
    let fixture = Fixture::new();
    let root = fixture.project();
    write_skill(
        &root,
        "spaced",
        "---  \nname: spaced\t# a comment after a tab\n\"description\": \"Caf\\u00e9 \\\"quoted\\\"\" # trailing\n---  \n",
    );
    write_skill(
        &root,
        "indented",
        "---\n'name': 'indented'\ndescription: |2\n  Kept\n  as one.\nlicense: ~\n...\n",
    );
    write_skill(&root, "absent", "---\nname: absent\ndescription: ~\n---\n");
    write_skill(
        &root,
        "nulled",
        "---\nname: nulled\ndescription: null\n---\n",
    );
    write_skill(
        &root,
        "escape",
        "---\nname: escape\ndescription: \"a\\qb\"\n---\n",
    );
    write_skill(
        &root,
        "after",
        "---\nname: after\ndescription: 'quoted' trailing\n---\n",
    );
    let discovery = fixture.discover();
    assert_eq!(
        discovery
            .skills()
            .iter()
            .map(|skill| (skill.name(), skill.description()))
            .collect::<Vec<_>>(),
        [("indented", "Kept as one."), ("spaced", "Café \"quoted\"")]
    );
    let problems = discovery
        .diagnostics()
        .iter()
        .map(|diagnostic| {
            (
                diagnostic
                    .path()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                diagnostic.problem().clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        problems,
        [
            ("absent".to_owned(), SkillProblem::MissingDescription),
            (
                "after".to_owned(),
                SkillProblem::FrontMatter("has text after a quoted value")
            ),
            (
                "escape".to_owned(),
                SkillProblem::FrontMatter("has an unsupported escape")
            ),
            ("nulled".to_owned(), SkillProblem::MissingDescription),
        ]
    );
}

#[test]
fn the_entry_cap_keeps_the_first_names_in_byte_order() {
    let fixture = Fixture::new();
    for index in 0..=MAX_ROOT_ENTRIES {
        fs::create_dir_all(fixture.project().join(format!("e{index:03}"))).unwrap();
    }
    let last = format!("e{MAX_ROOT_ENTRIES:03}");
    write_skill(&fixture.project(), "e000", &skill_file("e000", "First."));
    write_skill(
        &fixture.project(),
        &last,
        &skill_file(&last, "Past the cap."),
    );
    let discovery = fixture.discover();
    assert_eq!(
        discovery
            .skills()
            .iter()
            .map(Skill::name)
            .collect::<Vec<_>>(),
        ["e000"]
    );
    assert_eq!(
        discovery.diagnostics()[0].problem(),
        &SkillProblem::TooManyEntries
    );
}

#[test]
fn discovery_refuses_an_unloadable_file_and_load_revalidates_the_whole_file() {
    let fixture = Fixture::new();
    write_skill(
        &fixture.user(),
        "big",
        &format!(
            "{}{}",
            skill_file("big", "Big body."),
            "y".repeat(MAX_SKILL_FILE_BYTES)
        ),
    );
    write_skill(&fixture.user(), "small", &skill_file("small", "Small."));
    let discovery = fixture.discover();
    let [small] = discovery.skills() else {
        panic!("{:?}", discovery.skills());
    };
    assert_eq!(
        discovery.diagnostics()[0].problem(),
        &SkillProblem::TooLarge
    );
    let loaded = small.load(&LocalInstructionSourceReader).unwrap();
    assert_eq!(loaded.name(), "small");
    assert_eq!(loaded.directory(), fixture.user().join("small"));
    assert_eq!(loaded.body(), "# small\n\nSteps.\n");
    fs::write(
        fixture.user().join("small/SKILL.md"),
        skill_file("other", "Moved."),
    )
    .unwrap();
    assert_eq!(
        small
            .load(&LocalInstructionSourceReader)
            .unwrap_err()
            .problem(),
        &SkillProblem::NameMismatch {
            name: "other".to_owned()
        }
    );
}

#[test]
fn an_invalid_project_skill_leaves_the_user_skill_of_that_name_listed() {
    let fixture = Fixture::new();
    write_skill(&fixture.project(), "deploy", "no front matter");
    write_skill(
        &fixture.user(),
        "deploy",
        &skill_file("deploy", "User deploy."),
    );
    let discovery = fixture.discover();
    let [listed] = discovery.skills() else {
        panic!("{:?}", discovery.skills());
    };
    assert_eq!(listed.scope(), SkillScope::User);
    assert_eq!(
        listed
            .load(&LocalInstructionSourceReader)
            .unwrap()
            .directory(),
        fixture.user().join("deploy")
    );
}

#[test]
fn a_frozen_listing_keeps_only_the_skills_it_named() {
    let fixture = Fixture::new();
    let frozen = two_skills(&fixture)
        .registered_source(SkillsPresentation::Claude)
        .unwrap()
        .unwrap()
        .content()
        .to_owned();
    write_skill(
        &fixture.project(),
        "added",
        &skill_file("added", "Added later."),
    );
    write_skill(
        &fixture.user(),
        "commit",
        &skill_file("commit", "Reworded."),
    );
    let mut now = fixture.discover();
    now.retain_listed_in(&frozen);
    assert_eq!(
        now.skills().iter().map(Skill::name).collect::<Vec<_>>(),
        ["deploy"]
    );
    let roots_only = discover_skill_roots(
        &LocalInstructionSourceReader,
        &fixture.workspace,
        &fixture.roots(),
    )
    .unwrap();
    assert_eq!(roots_only.skills(), [] as [Skill; 0]);
    assert_eq!(roots_only.roots().len(), 2);
}

#[test]
fn skills_cut_by_the_listing_bound_are_not_listed_skills() {
    let fixture = Fixture::new();
    let description = "d".repeat(MAX_SKILL_DESCRIPTION_CHARS);
    for index in 0..20 {
        let name = format!("s{index:02}");
        write_skill(&fixture.project(), &name, &skill_file(&name, &description));
    }
    let discovery = fixture.discover();
    let listed = discovery.listed_skills(SkillsPresentation::Claude);
    let rendered = render_skills_listing(SkillsPresentation::Claude, discovery.skills()).unwrap();
    assert!(listed.len() < discovery.skills().len());
    assert_eq!(listed.len(), rendered.matches("\n- s").count());
    assert!(rendered.contains(&format!("- {}:", listed.last().unwrap().name())));
    assert!(!rendered.contains(&format!("- {}:", discovery.skills()[listed.len()].name())));
}

#[test]
fn disable_model_invocation_takes_the_yaml_core_booleans_only() {
    let fixture = Fixture::new();
    for (name, value) in [
        ("a", "True"),
        ("b", "TRUE"),
        ("c", "False"),
        ("d", "FALSE"),
        ("e", "yes"),
    ] {
        write_skill(
            &fixture.project(),
            name,
            &format!(
                "---\nname: {name}\ndescription: D.\ndisable-model-invocation: {value}\n---\n"
            ),
        );
    }
    let discovery = fixture.discover();
    assert_eq!(
        discovery
            .skills()
            .iter()
            .map(Skill::name)
            .collect::<Vec<_>>(),
        ["c", "d"]
    );
    assert_eq!(
        discovery.diagnostics()[0].problem(),
        &SkillProblem::FrontMatter("has a disable-model-invocation that is not true or false")
    );
}

/// Skills are discovered and loaded only through the reader: a project root
/// that exists only where the reader answers is listed under its remote
/// canonical path and loads from there.
#[test]
fn skills_are_discovered_and_loaded_only_through_the_reader() {
    use crate::test_readers::{RUNNER_ROOT, RemappedReader};

    let fixture = Fixture::new();
    write_skill(
        &fixture.project(),
        "deploy",
        &skill_file("deploy", "Ship it."),
    );
    let base = fixture.workspace.parent().unwrap();
    let reader = RemappedReader::new(base);
    let remote_workspace = Path::new(RUNNER_ROOT).join("repo");
    assert!(!remote_workspace.exists());

    let discovery = discover_skills(
        &reader,
        &remote_workspace,
        &[SkillRoot::project(&remote_workspace)],
    )
    .unwrap();
    assert_eq!(discovery.diagnostics(), [] as [SkillDiagnostic; 0]);
    let [skill] = discovery.skills() else {
        panic!("one remote skill is listed");
    };
    assert_eq!(skill.name(), "deploy");
    assert_eq!(
        skill.file(),
        remote_workspace
            .join(PROJECT_SKILLS_DIRECTORY)
            .join("deploy")
            .join(SKILL_FILE_NAME)
    );
    let loaded = skill.load(&reader).unwrap();
    assert_eq!(loaded.directory(), skill.directory());
    assert!(loaded.body().contains("Steps."));
    assert_eq!(
        skill
            .load(&LocalInstructionSourceReader)
            .unwrap_err()
            .problem(),
        &SkillProblem::MissingSkillFile
    );
}

/// An unavailable reader fails discovery and loading closed: nothing is
/// listed as empty and then frozen. Over-long reads are refused, and a
/// listing's duplicate or non-name entries are never read.
#[test]
fn a_misbehaving_reader_fails_skills_closed() {
    use crate::test_readers::{Misbehavior, ScriptedReader};

    let fixture = Fixture::new();
    write_skill(
        &fixture.project(),
        "deploy",
        &skill_file("deploy", "Ship it."),
    );
    let discover = |misbehavior| {
        discover_skills(
            &ScriptedReader(misbehavior),
            &fixture.workspace,
            &fixture.roots(),
        )
    };
    assert_eq!(
        discover(Misbehavior::Unavailable),
        Err(SourceReadError::Unavailable)
    );
    assert_eq!(
        discover_skill_roots(
            &ScriptedReader(Misbehavior::Unavailable),
            &fixture.workspace,
            &fixture.roots(),
        ),
        Err(SourceReadError::Unavailable)
    );
    assert_eq!(
        discover(Misbehavior::OverReads),
        Err(SourceReadError::InvalidAnswer)
    );
    let listed = discover(Misbehavior::MalformedListing).unwrap();
    assert_eq!(
        listed.skills().iter().map(Skill::name).collect::<Vec<_>>(),
        ["deploy"]
    );
    assert_eq!(listed.diagnostics(), [] as [SkillDiagnostic; 0]);

    let discovery = fixture.discover();
    let skill = &discovery.skills()[0];
    assert_eq!(
        skill
            .load(&ScriptedReader(Misbehavior::Unavailable))
            .unwrap_err()
            .problem(),
        &SkillProblem::Reader(SourceReadError::Unavailable)
    );
}
