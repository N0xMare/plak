use std::path::{Path, PathBuf};

use plakous::model::ToolExecutionOwner;
use plakous::tools::{CodingPresentation, CompositionRuntime, ToolOwnerClass};

use super::*;
use crate::binding::CompositionSelection;
use crate::tool_plan::ToolPlan;
use crate::tools::ProductToolExtensions;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const SECRET_VALUE: &str = "s3cret-value-not-for-logs";

struct Secrets(BTreeMap<&'static str, &'static str>);

impl NamedSecrets for Secrets {
    fn read(&self, name: &str) -> Result<Option<NamedSecretValue>, String> {
        Ok(self
            .0
            .get(name)
            .map(|value| NamedSecretValue::new((*value).to_owned())))
    }
}

fn secrets(entries: &[(&'static str, &'static str)]) -> Arc<dyn NamedSecrets> {
    Arc::new(Secrets(entries.iter().copied().collect()))
}

/// The repository-only fixture server. Workspace test runs build every
/// workspace binary into the directory above the test binary's `deps`.
fn fixture() -> TestResult<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("test binary has no target directory")?;
    let server = directory.join("mcp-fake-server");
    if !server.is_file() {
        return Err(format!(
            "{} is missing; run the workspace tests so the fixture binary is built",
            server.display()
        )
        .into());
    }
    Ok(server)
}

fn quoted(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One table as the product's canonical rendering spells it.
fn rendered(
    command: &str,
    args: &[&str],
    secret_env: &[(&str, &str)],
    tools: Option<&[&str]>,
) -> String {
    let secret_env = secret_env
        .iter()
        .map(|(name, secret)| format!("{name} = {secret:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!(
        "[mcp_servers.fake]\ntransport = \"stdio\"\ncommand = {command:?}\nargs = [{}]\n\
         env = {{ MODE = \"test\" }}\nsecret_env = {{ {secret_env} }}\n",
        quoted(args)
    );
    if let Some(tools) = tools {
        out.push_str(&format!("tools = [{}]\n", quoted(tools)));
    }
    out.push_str("startup_seconds = 20\ncall_seconds = 120\n");
    out
}

fn sections(table: String) -> BTreeMap<String, String> {
    BTreeMap::from([("mcp_servers.fake".to_owned(), table)])
}

fn fake() -> TestResult<McpServerName> {
    Ok(McpServerName::new("fake")?)
}

#[test]
fn a_frozen_table_rebuilds_its_definition_and_names_every_refusal() -> TestResult {
    let token = EnvironmentVariableName::new("TOKEN")?;
    let table = rendered(
        "fake-server",
        &["stdio"],
        &[("TOKEN", "tok")],
        Some(&["echo"]),
    );
    let rebuilt = server_spec(&fake()?, &table, &secrets(&[("tok", SECRET_VALUE)]))?;
    let by_hand = McpServerSpec::stdio(fake()?, "fake-server", ["stdio".to_owned()])
        .with_environment([(EnvironmentVariableName::new("MODE")?, "test".to_owned())])
        .with_secret_environment([token], Arc::new(|| Ok(Vec::new())))
        .with_startup_timeout(Duration::from_secs(20))
        .with_call_timeout(Duration::from_secs(120))
        .with_allowlist(["echo".to_owned()]);
    assert_eq!(rebuilt.definition_digest()?, by_hand.definition_digest()?);
    assert!(!format!("{rebuilt:?}").contains(SECRET_VALUE));

    assert_eq!(
        server_spec(&fake()?, &table, &secrets(&[]))
            .err()
            .as_deref(),
        Some("mcp server fake: secret \"tok\" is not set; remedy: plak secret set tok")
    );
    let http = |url: &str| {
        format!(
            "[mcp_servers.fake]\ntransport = \"http\"\nurl = {url:?}\nbearer_secret = \"tok\"\n\
             headers = {{ X-Client = \"plak\" }}\nsecret_headers = {{}}\n\
             startup_seconds = 20\ncall_seconds = 120\n"
        )
    };
    assert!(
        server_spec(
            &fake()?,
            &http("https://mcp.example/mcp"),
            &secrets(&[("tok", "v")])
        )
        .is_ok()
    );
    assert_eq!(
        server_spec(&fake()?, &http("https://mcp.example/mcp"), &secrets(&[]))
            .err()
            .as_deref(),
        Some("mcp server fake: secret \"tok\" is not set; remedy: plak secret set tok")
    );
    let plain = server_spec(
        &fake()?,
        &http("http://example.com/mcp"),
        &secrets(&[("tok", "v")]),
    )
    .err()
    .ok_or("a plain-http non-loopback url bound")?;
    assert!(
        plain.starts_with("mcp server fake: mcp server url must be https://"),
        "{plain}"
    );
    let invalid = Some("mcp server fake has an invalid frozen definition");
    for broken in [
        table.replace("[mcp_servers.fake]", "[mcp_servers.other]"),
        format!("{table}surprise = 1\n"),
        table.replace("transport = \"stdio\"", "transport = \"carrier-pigeon\""),
        "not toml [".to_owned(),
    ] {
        assert_eq!(
            server_spec(&fake()?, &broken, &secrets(&[("tok", "v")]))
                .err()
                .as_deref(),
            invalid,
            "{broken}"
        );
    }
    Ok(())
}

#[test]
fn an_unconfigured_server_refuses_with_its_remedy() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let refused = bind_mcp_tools(
        &TemplateToolSelection::parse(["mcp:github"])?,
        &BTreeMap::new(),
        workspace.path(),
        secrets(&[]),
        None,
    )
    .err();
    assert_eq!(
        refused.as_deref(),
        Some(
            "mcp server github is not configured; remedy: add [mcp_servers.github] to \
             config.toml or remove mcp:github from the template's tools"
        )
    );
    Ok(())
}

/// `mcp:fake` binds against the real fixture process and reaches the Direct
/// menus of a Messages-vocabulary and a Responses-vocabulary presentation and
/// the JavaScript and Python nested catalogs, owned by its server and earning
/// the untrusted-data paragraph.
#[test]
fn a_template_naming_mcp_fake_binds_into_every_presentation_and_nested_catalog() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let server = fixture()?;
    let selection = TemplateToolSelection::parse(["mcp:fake/echo", "mcp:fake/fail"])?;
    let bound = bind_mcp_tools(
        &selection,
        &sections(rendered(
            server.to_str().ok_or("fixture path")?,
            &[],
            &[],
            None,
        )),
        workspace.path(),
        secrets(&[]),
        None,
    )?;
    assert_eq!(bound.notices, Vec::<String>::new());
    let plan = ToolPlan::new(&selection, []).with_mcp(&selection, bound);
    let extensions = ProductToolExtensions::empty().with_plan(&plan)?;
    let echo = ToolName::new("mcp__fake__echo")?;
    assert_eq!(
        extensions.names().cloned().collect::<Vec<_>>(),
        [echo.clone(), ToolName::new("mcp__fake__fail")?]
    );
    for (composition, runtime, presentation) in [
        (
            CompositionSelection::Direct,
            None,
            CodingPresentation::Claude,
        ),
        (
            CompositionSelection::Direct,
            None,
            CodingPresentation::Codex,
        ),
        (
            CompositionSelection::JsPtc,
            Some(CompositionRuntime::ProductJavaScript),
            CodingPresentation::Codex,
        ),
        (
            CompositionSelection::PyPtc,
            Some(CompositionRuntime::PythonPtc),
            CodingPresentation::Claude,
        ),
    ] {
        let registry = crate::agent::coding_tool_registry_with_extensions(
            "mcp-binding",
            composition,
            false,
            &extensions,
        )?;
        let identity = registry.identity(&echo).ok_or("echo is not registered")?;
        assert_eq!(identity.owner(), &ToolOwnerClass::Mcp(fake()?));
        let definition = registry.definition(&echo).ok_or("echo has no definition")?;
        assert_eq!(
            crate::agent::direct_extension_tool(definition.clone(), identity.owner())?
                .local_owner(),
            Some(ToolExecutionOwner::Mcp {
                server: fake()?.canonical().clone()
            })
        );
        let surface = crate::tools::product_surface_with_fence(
            plakous::tools::JobConfig::default(),
            presentation,
            registry,
            crate::agent::rendered_prompts::suite::synthetic_target(b"mcp-binding"),
            &extensions,
            crate::agent::SessionToolFence::Full,
            runtime,
        )?;
        assert!(
            surface.exposed_names().contains(&echo),
            "{presentation:?} {runtime:?}"
        );
        assert!(
            surface
                .contributions()
                .iter()
                .any(|contribution| contribution.id()
                    == plakous::prompts::identity::UNTRUSTED_TOOL_DATA_CONTRIBUTION_ID),
            "{presentation:?} {runtime:?}"
        );
    }
    Ok(())
}

#[test]
fn a_secret_reaches_the_server_environment_and_no_refusal_text() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let server = fixture()?;
    let selection = TemplateToolSelection::parse(["mcp:fake/echo"])?;
    let expect = format!("TOKEN={SECRET_VALUE}");
    let table = sections(rendered(
        server.to_str().ok_or("fixture path")?,
        &["--expect-env", &expect],
        &[("TOKEN", "tok")],
        None,
    ));
    let bound = bind_mcp_tools(
        &selection,
        &table,
        workspace.path(),
        secrets(&[("tok", SECRET_VALUE)]),
        None,
    )?;
    assert_eq!(bound.names.len(), 1);
    let plan = ToolPlan::new(&selection, []).with_mcp(&selection, bound);
    assert!(!format!("{plan:?}").contains(SECRET_VALUE));

    // The running entry keeps the values it was spawned with; a fresh
    // workspace is a fresh entry, which spawns with the new value.
    let other_workspace = tempfile::tempdir()?;
    let refused = bind_mcp_tools(
        &selection,
        &table,
        other_workspace.path(),
        secrets(&[("tok", "a-different-value")]),
        None,
    )
    .err()
    .ok_or("a server that exited at startup bound")?;
    assert!(refused.starts_with("mcp server fake"), "{refused}");
    assert!(
        refused.ends_with("remedy: fix the server or remove mcp:fake from the template's tools")
    );
    assert!(!refused.contains(SECRET_VALUE) && !refused.contains("a-different-value"));
    Ok(())
}

#[test]
fn a_wildcard_omits_an_unlowerable_tool_with_a_notice_and_an_explicit_name_refuses() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let server = fixture()?;
    let table = sections(rendered(
        server.to_str().ok_or("fixture path")?,
        &["--tools", "unlowerable"],
        &[],
        None,
    ));
    let bound = bind_mcp_tools(
        &TemplateToolSelection::parse(["mcp:fake"])?,
        &table,
        workspace.path(),
        secrets(&[]),
        None,
    )?;
    assert!(
        bound
            .names
            .iter()
            .any(|(_, name)| name.as_str() == "mcp__fake__echo")
    );
    assert!(
        !bound
            .names
            .iter()
            .any(|(_, name)| name.as_str() == "mcp__fake__tangled")
    );
    assert_eq!(bound.notices.len(), 1, "{:?}", bound.notices);
    assert!(bound.notices[0].contains("tangled"), "{:?}", bound.notices);

    let refused = bind_mcp_tools(
        &TemplateToolSelection::parse(["mcp:fake/tangled"])?,
        &table,
        workspace.path(),
        secrets(&[]),
        None,
    )
    .err()
    .ok_or("an unlowerable tool named explicitly bound")?;
    assert!(refused.contains("tangled"), "{refused}");
    Ok(())
}

/// An `http` table binds a streamable HTTP fixture that requires a bearer
/// token: the named secret fills `Authorization`, and a wrong value refuses
/// the bind without its text.
#[test]
fn an_http_server_binds_with_its_bearer_secret() -> TestResult {
    use std::io::BufRead as _;

    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(
        std::process::Command::new(fixture()?)
            .arg("--http")
            .env("MCP_FAKE_BEARER", SECRET_VALUE)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?,
    );
    let stdout = child.0.stdout.take().ok_or("fixture stdout")?;
    let mut line = String::new();
    std::io::BufReader::new(stdout).read_line(&mut line)?;
    let url = line
        .trim()
        .strip_prefix("listening on ")
        .ok_or_else(|| format!("unexpected fixture line {line:?}"))?
        .to_owned();
    let table = sections(format!(
        "[mcp_servers.fake]\ntransport = \"http\"\nurl = {url:?}\nbearer_secret = \"tok\"\n\
         headers = {{}}\nsecret_headers = {{}}\nstartup_seconds = 20\ncall_seconds = 120\n"
    ));
    let selection = TemplateToolSelection::parse(["mcp:fake/echo"])?;
    let workspace = tempfile::tempdir()?;
    let bound = bind_mcp_tools(
        &selection,
        &table,
        workspace.path(),
        secrets(&[("tok", SECRET_VALUE)]),
        None,
    )?;
    assert_eq!(
        bound
            .names
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>(),
        ["mcp__fake__echo"]
    );
    let other_workspace = tempfile::tempdir()?;
    let refused = bind_mcp_tools(
        &selection,
        &table,
        other_workspace.path(),
        secrets(&[("tok", "a-different-value")]),
        None,
    )
    .err()
    .ok_or("a wrong bearer bound")?;
    assert!(refused.starts_with("mcp server fake"), "{refused}");
    assert!(!refused.contains(SECRET_VALUE) && !refused.contains("a-different-value"));
    Ok(())
}

/// Records frozen at a session's creation rebuild the same tools on a later
/// bind without listing again: the omission notice was the creation's, and
/// the names and order are the frozen ones.
#[test]
fn frozen_records_rebuild_the_same_tools_without_a_new_listing() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let server = fixture()?;
    let selection = TemplateToolSelection::parse(["mcp:fake"])?;
    let table = sections(rendered(
        server.to_str().ok_or("fixture path")?,
        &["--tools", "unlowerable"],
        &[],
        None,
    ));
    let created = bind_mcp_tools(&selection, &table, workspace.path(), secrets(&[]), None)?;
    assert_eq!(created.notices.len(), 1, "{:?}", created.notices);
    assert_eq!(created.records.len(), created.names.len());
    let reopened = bind_mcp_tools(
        &selection,
        &table,
        workspace.path(),
        secrets(&[]),
        Some(&created.records),
    )?;
    assert_eq!(reopened.names, created.names);
    assert_eq!(reopened.records, created.records);
    assert!(reopened.notices.is_empty(), "{:?}", reopened.notices);

    // Frozen records of one tool keep exactly that tool, whatever the
    // server lists now.
    let echo = created
        .records
        .iter()
        .filter(|record| record.mcp_name() == "echo")
        .cloned()
        .collect::<Vec<_>>();
    let narrowed = bind_mcp_tools(
        &selection,
        &table,
        workspace.path(),
        secrets(&[]),
        Some(&echo),
    )?;
    assert_eq!(
        narrowed
            .names
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>(),
        ["mcp__fake__echo"]
    );
    Ok(())
}

/// `resources = true` adds the server's `read_resource` tool to every
/// selection, on a fresh bind and on a reopen from frozen records alike; the
/// probe reports it with the resource counts; and a server that does not
/// advertise resources refuses the bind naming the key.
#[test]
fn resources_add_the_read_resource_tool_or_refuse_without_the_capability() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let server = fixture()?;
    let command = server.to_str().ok_or("fixture path")?;
    let with_resources = |args: &[&str]| {
        rendered(command, args, &[], None).replace(
            "startup_seconds = 20\n",
            "resources = true\nstartup_seconds = 20\n",
        )
    };
    let selection = TemplateToolSelection::parse(["mcp:fake/echo"])?;
    let table = sections(with_resources(&["--resources"]));
    let names = |bound: &BoundMcpTools| {
        bound
            .names
            .iter()
            .map(|(_, name)| name.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    let created = bind_mcp_tools(&selection, &table, workspace.path(), secrets(&[]), None)?;
    assert_eq!(
        names(&created),
        ["mcp__fake__echo", "mcp__fake__read_resource"]
    );
    assert_eq!(created.registrations.len(), 2);
    let reopened = bind_mcp_tools(
        &selection,
        &table,
        workspace.path(),
        secrets(&[]),
        Some(&created.records),
    )?;
    assert_eq!(names(&reopened), names(&created));

    let probe = probe_mcp_server(
        "fake",
        &with_resources(&["--resources"]),
        workspace.path(),
        secrets(&[]),
    )?;
    assert_eq!(probe.resources, Some((3, 1)));
    assert_eq!(
        probe.tools.last(),
        Some(&(
            "read_resource".to_owned(),
            "mcp__fake__read_resource".to_owned()
        ))
    );

    let refused = bind_mcp_tools(
        &selection,
        &sections(with_resources(&[])),
        workspace.path(),
        secrets(&[]),
        None,
    )
    .err()
    .ok_or("a server without resources bound read_resource")?;
    assert!(
        refused
            .ends_with("remedy: remove resources = true from [mcp_servers.fake] or fix the server"),
        "{refused}"
    );
    Ok(())
}

/// A product that stores MCP logins in `directory`.
struct LoginSecrets(PathBuf);

impl NamedSecrets for LoginSecrets {
    fn read(&self, _name: &str) -> Result<Option<NamedSecretValue>, String> {
        Ok(None)
    }

    fn mcp_login_file(&self, server: &str) -> Option<PathBuf> {
        Some(self.0.join(server).join("credential.json"))
    }
}

/// An `auth = "oauth"` table binds the stored login: without one the bind
/// refuses before connecting and names `plak mcp login`, and a product that
/// stores no logins refuses the table.
#[test]
fn an_oauth_table_without_a_login_refuses_before_connecting() -> TestResult {
    let home = tempfile::tempdir()?;
    let table = |extra: &str| {
        format!(
            "[mcp_servers.fake]\ntransport = \"http\"\nurl = \"https://mcp.example/mcp\"\n\
             headers = {{}}\nsecret_headers = {{}}\nauth = \"oauth\"\n{extra}\
             startup_seconds = 20\ncall_seconds = 120\n"
        )
    };
    let logins: Arc<dyn NamedSecrets> = Arc::new(LoginSecrets(home.path().to_path_buf()));
    assert_eq!(
        server_spec(&fake()?, &table(""), &logins).err().as_deref(),
        Some("mcp server fake is not logged in; remedy: plak mcp login fake")
    );
    assert_eq!(
        mcp_login_status("fake", &table(""), logins.as_ref())?,
        Some(McpOAuthStatus::NotLoggedIn)
    );
    // The pre-registered client's secret is a bind check like every other.
    let client = "oauth_issuer = \"https://auth.example\"\noauth_client_id = \"plak\"\n\
                  oauth_client_secret = \"fake-client\"\n";
    // Login/logout definitions resolve client secrets only when sending them.
    assert!(mcp_oauth_spec("fake", &table(client), Arc::clone(&logins)).is_ok());
    assert_eq!(
        server_spec(&fake()?, &table(client), &logins)
            .err()
            .as_deref(),
        Some(
            "mcp server fake: secret \"fake-client\" is not set; remedy: plak secret set fake-client"
        )
    );
    assert_eq!(
        server_spec(&fake()?, &table(""), &secrets(&[]))
            .err()
            .as_deref(),
        Some("mcp server fake uses oauth, and this product stores no MCP logins")
    );
    let bearer = "[mcp_servers.fake]\ntransport = \"http\"\nurl = \"https://mcp.example/mcp\"\n\
                  headers = {}\nsecret_headers = {}\nstartup_seconds = 20\ncall_seconds = 120\n";
    assert_eq!(mcp_login_status("fake", bearer, logins.as_ref())?, None);
    Ok(())
}
