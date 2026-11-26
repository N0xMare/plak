//! Single publisher for the immutable product snapshot, sidecars, and links.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _, symlink,
};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};

use crate::shipping_artifact::{
    self as shipping, BuildInfo, FileFacts, Manifest, NAMES, check, fail, io,
};
use crate::{CliResult, ProcessOutcome};

mod archive;
pub(crate) mod maintainer;
#[cfg(test)]
pub(crate) mod tests;

#[derive(Clone, Debug, Eq, PartialEq, Subcommand)]
pub(crate) enum Action {
    Install(Install),
    Link { id: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Flavor {
    Release,
    Maintainer,
}

impl Flavor {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Maintainer => "maintainer",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Args)]
pub(crate) struct Install {
    #[arg(long)]
    from: PathBuf,
    #[arg(long)]
    manifest: Option<PathBuf>,
    #[arg(long)]
    no_link: bool,
    #[arg(long, hide = true)]
    source_build: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "release", hide = true)]
    flavor: Flavor,
    #[arg(long, hide = true, conflicts_with = "source_build")]
    release_download: bool,
}

pub(crate) fn run(action: Action, stdout: &mut dyn Write) -> CliResult<ProcessOutcome> {
    match action {
        Action::Install(options) => {
            let path = install(&options)?;
            writeln!(stdout, "{}", path.display()).map_err(io)?;
        }
        Action::Link { id } => {
            link_snapshot(&id)?;
        }
    }
    Ok(ProcessOutcome::Success)
}

fn install(options: &Install) -> CliResult<PathBuf> {
    check(
        options.flavor != Flavor::Maintainer || options.source_build.is_some(),
        "maintainer install requires the source build recipe",
    )?;
    let source = shipping::absolute(&options.from)?;
    let archive = !source.is_dir();
    let manifest_path = options
        .manifest
        .clone()
        .unwrap_or_else(|| source.join("manifest.json"));
    let has_manifest = options.flavor != Flavor::Maintainer;
    let manifest = has_manifest
        .then(|| {
            if archive && options.manifest.is_none() {
                return Err(fail("a local tarball requires --manifest"));
            }
            Manifest::parse(&shipping::read(&manifest_path, shipping::LIMIT)?)
        })
        .transpose()?;
    let unpacked = archive
        .then(|| {
            archive::extract(
                &source,
                manifest
                    .as_ref()
                    .ok_or_else(|| fail("archive requires a shipping manifest"))?,
            )
        })
        .transpose()?;
    let source = unpacked
        .as_ref()
        .map_or(source.as_path(), tempfile::TempDir::path);
    let expected = manifest.as_ref().map_or_else(
        || shipping::collect(source),
        |manifest| Ok(manifest.files.clone()),
    )?;
    if let Some(manifest) = &manifest {
        admit_build(manifest, options.source_build.is_some())?;
    }
    let verified = tempfile::tempdir().map_err(io)?;
    for (name, file) in &expected {
        let mut output = create_file(&verified.path().join(name), 0o500)?;
        shipping::copy(&source.join(name), &mut output, file)?;
    }
    let running = crate::build_identity::running_executable()
        .ok_or_else(|| fail("cannot identify the installer image"))?;
    expected["plakd"].verify(running)?;
    let layout = Layout::resolve(options.flavor)?;
    let id = shipping::snapshot_id(&expected);
    let manifest = if options.flavor == Flavor::Maintainer {
        maintainer::Manifest::from_build(verified.path(), &expected, &id)?
    } else {
        None
    };
    let snapshot = layout.publish(verified.path(), &expected, &id, manifest.as_deref())?;
    if !options.no_link {
        layout.link(
            &snapshot,
            options.source_build.as_deref(),
            !options.release_download,
        )?;
    }
    Ok(snapshot)
}

fn admit_build(manifest: &Manifest, source_build: bool) -> CliResult<()> {
    #[cfg(target_env = "gnu")]
    const GNU: bool = true;
    #[cfg(not(target_env = "gnu"))]
    const GNU: bool = false;

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let platform = if os == "macos" && arch == "aarch64" {
        "aarch64-apple-darwin"
    } else if os == "linux" && arch == "x86_64" {
        if source_build && GNU {
            "x86_64-unknown-linux-gnu"
        } else {
            "x86_64-unknown-linux-musl"
        }
    } else {
        return Err(fail("unsupported artifact platform"));
    };
    check(
        manifest.platform == platform,
        "artifact platform does not match this machine",
    )?;
    let mut current = Vec::new();
    crate::build_identity::print_build_info(&mut current)?;
    let mut current: BuildInfo =
        serde_json::from_slice(&current).map_err(|_| fail("cannot read installer build info"))?;
    if crate::test_hooks::snapshot_fixture() {
        current.flavor = "release".to_owned();
        current.revision.clone_from(&manifest.revision);
        current.test_hooks = false;
    }
    check(
        current == manifest.build_info,
        "artifact and manifest build info differ",
    )
}

pub(crate) fn link_snapshot(id: &str) -> CliResult<()> {
    Layout::resolve(Flavor::Release)?.link_verified(id)
}

struct Layout {
    root: PathBuf,
    bin: PathBuf,
    cargo_bin: PathBuf,
    flavor: Flavor,
}

impl Layout {
    fn resolve(flavor: Flavor) -> CliResult<Self> {
        let home = crate::environment::inherited_home().ok_or_else(|| fail("HOME is required"))?;
        Self::from_home(Path::new(&home), flavor)
    }

    fn from_home(home: &Path, flavor: Flavor) -> CliResult<Self> {
        check(
            home.is_absolute() && home != Path::new("/"),
            "HOME must be absolute and not root",
        )?;
        let home = home.canonicalize().map_err(io)?;
        safe_ancestors(&home)?;
        Ok(Self {
            root: physical_path(&home.join(".local/lib/plak"))?,
            bin: physical_path(&home.join(".local/bin"))?,
            cargo_bin: crate::environment::cargo_home()
                .map_or_else(|| home.join(".cargo"), PathBuf::from)
                .join("bin"),
            flavor,
        })
    }

    fn snapshot(&self, id: &str) -> PathBuf {
        self.root.join(self.flavor.name()).join(id)
    }

    fn verified(&self, id: &str) -> CliResult<PathBuf> {
        check(shipping::hex(id, 64), "invalid snapshot id")?;
        let snapshot = self.snapshot(id);
        self.verify_snapshot(&snapshot, id)?;
        Ok(snapshot)
    }

    fn link_verified(&self, id: &str) -> CliResult<()> {
        self.link(&self.verified(id)?, None, true)
    }

    fn sidecar(&self, name: &str, digest: &str) -> PathBuf {
        self.root
            .join("sidecars")
            .join(name)
            .join(digest)
            .join(name)
    }

    fn publish(
        &self,
        verified: &Path,
        files: &BTreeMap<String, FileFacts>,
        id: &str,
        deploy_manifest: Option<&[u8]>,
    ) -> CliResult<PathBuf> {
        let root = self.root.join(self.flavor.name());
        ensure(&root)?;
        owned(&root)?;
        let snapshot = self.snapshot(id);
        let stage = tempfile::tempdir_in(&root).map_err(io)?;
        immutable(
            &verified.join("plakd"),
            &stage.path().join("plakd"),
            &files["plakd"],
        )?;
        for name in NAMES.into_iter().skip(1) {
            let facts = &files[name];
            let worker = self.sidecar(name, &facts.sha256);
            Self::publish_sidecar(&verified.join(name), &worker, facts)?;
            symlink(worker, stage.path().join(name)).map_err(io)?;
        }
        crate::build_identity::write_image_manifest(
            stage.path(),
            shipping::open(&stage.path().join("plakd"))?,
        )
        .map_err(io)?;
        fs::write(
            stage.path().join("state-key"),
            format!("{}\n", crate::home::state::state_key()),
        )
        .map_err(io)?;
        fs::write(
            stage.path().join("export-version"),
            format!("{}\n", crate::carry::VERSION),
        )
        .map_err(io)?;
        if let Some(raw) = deploy_manifest {
            immutable_bytes(&stage.path().join(maintainer::MANIFEST), raw)?;
        }
        publish_directory(stage.path(), &snapshot)?;
        self.verify_snapshot(&snapshot, id)?;
        if let Some(raw) = deploy_manifest {
            check(
                shipping::read(&snapshot.join(maintainer::MANIFEST), shipping::LIMIT)? == raw,
                "installed maintainer manifest differs",
            )?;
        }
        Ok(snapshot)
    }

    fn publish_sidecar(source: &Path, dest: &Path, facts: &FileFacts) -> CliResult<()> {
        let directory = parent(dest)?;
        let category = parent(directory)?;
        ensure(category)?;
        owned(category)?;
        let stage = tempfile::tempdir_in(category).map_err(io)?;
        let name = dest
            .file_name()
            .ok_or_else(|| fail("invalid sidecar name"))?;
        immutable(source, &stage.path().join(name), facts)?;
        publish_directory(stage.path(), directory)?;
        owned(directory)?;
        owned_image(dest)?;
        facts.verify(dest)
    }

    fn verify_snapshot(&self, snapshot: &Path, id: &str) -> CliResult<()> {
        owned(snapshot)?;
        let binary = snapshot.join("plakd");
        owned(
            snapshot
                .parent()
                .ok_or_else(|| fail("invalid snapshot root"))?,
        )?;
        owned_image(&binary)?;
        let mut files = BTreeMap::from([("plakd".to_owned(), FileFacts::read(&binary)?)]);
        for name in NAMES.into_iter().skip(1) {
            let worker = fs::read_link(snapshot.join(name)).map_err(io)?;
            let facts = FileFacts::read(&worker)?;
            check(
                worker == self.sidecar(name, &facts.sha256),
                "sidecar link is not canonical",
            )?;
            let digest_dir = parent(&worker)?;
            owned(digest_dir)?;
            owned(parent(digest_dir)?)?;
            owned(&self.root.join("sidecars"))?;
            owned_image(&worker)?;
            files.insert(name.to_owned(), facts);
        }
        for marker in ["binary-id", "state-key", "export-version"] {
            check(
                !shipping::read(&snapshot.join(marker), 1024)?.is_empty(),
                "missing target marker",
            )?;
        }
        check(
            shipping::snapshot_id(&files) == id,
            "snapshot integrity refused",
        )
    }

    fn link(&self, snapshot: &Path, build: Option<&Path>, cargo: bool) -> CliResult<()> {
        ensure(&self.bin)?;
        let build = build.map(Path::canonicalize).transpose().map_err(io)?;
        let mut links = Vec::new();
        for name in NAMES {
            let source = if name == "plakd" {
                snapshot.join(name)
            } else {
                fs::read_link(snapshot.join(name)).map_err(io)?
            };
            let path = self.bin.join(name);
            self.check_link(&path, build.as_deref())?;
            links.push((path, source.clone()));
            let path = self.cargo_bin.join(name);
            if cargo && fs::symlink_metadata(&path).is_ok() {
                let path = physical_path(&self.cargo_bin)?.join(name);
                safe_ancestors(parent(&path)?)?;
                self.check_link(&path, build.as_deref())?;
                links.push((path, source));
            }
        }
        if !cargo || build.is_some() {
            self.remove_obsolete(&self.bin)?;
        }
        for (dest, source) in links {
            atomic_link(&dest, &source)?;
        }
        Ok(())
    }

    fn remove_obsolete(&self, bin: &Path) -> CliResult<()> {
        let old = bin.join("monty");
        if let Ok(destination) = fs::read_link(&old)
            && let Ok(destination) = physical_path(&bin.join(destination))
            && ["release", "maintainer", "sidecars"]
                .into_iter()
                .any(|root| destination.starts_with(self.root.join(root)))
        {
            fs::remove_file(old).map_err(io)?;
        }
        Ok(())
    }

    fn check_link(&self, path: &Path, build: Option<&Path>) -> CliResult<()> {
        let link = match fs::read_link(path) {
            Ok(link) => link,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(fail("refusing to replace non-Plak link; move it aside")),
        };
        let absolute = parent(path)?.join(link);
        let target = absolute
            .canonicalize()
            .or_else(|_| {
                let directory = absolute
                    .parent()
                    .ok_or_else(|| std::io::Error::other("missing link parent"))?;
                Ok::<_, std::io::Error>(
                    directory.canonicalize()?.join(
                        absolute
                            .file_name()
                            .ok_or_else(|| std::io::Error::other("missing link leaf"))?,
                    ),
                )
            })
            .map_err(io)?;
        check(
            ["release", "maintainer", "sidecars"]
                .into_iter()
                .any(|name| target.starts_with(self.root.join(name)))
                || build.is_some_and(|build| target.starts_with(build)),
            "refusing to replace non-Plak link; move it aside",
        )
    }
}

fn create_file(path: &Path, mode: u32) -> CliResult<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(io)
}

fn immutable(source: &Path, dest: &Path, facts: &FileFacts) -> CliResult<()> {
    let mut output = create_file(dest, 0o555)?;
    shipping::copy(source, &mut output, facts)?;
    output
        .set_permissions(fs::Permissions::from_mode(0o555))
        .map_err(io)
}

fn immutable_bytes(dest: &Path, raw: &[u8]) -> CliResult<()> {
    let mut output = create_file(dest, 0o444)?;
    output.write_all(raw).map_err(io)
}

fn publish_directory(stage: &Path, dest: &Path) -> CliResult<()> {
    match fs::symlink_metadata(dest) {
        Ok(_) => owned(dest),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(stage, dest)
                .or_else(|error| if dest.is_dir() { Ok(()) } else { Err(error) })
                .map_err(io)?;
            owned(dest)
        }
        Err(error) => Err(io(error)),
    }
}

pub(crate) fn atomic_link(dest: &Path, source: &Path) -> CliResult<()> {
    let dir = tempfile::tempdir_in(parent(dest)?).map_err(io)?;
    let pending = dir.path().join("link");
    symlink(source, &pending).map_err(io)?;
    fs::rename(pending, dest).map_err(io)
}

pub(crate) fn parent(path: &Path) -> CliResult<&Path> {
    path.parent().ok_or_else(|| fail("invalid file path"))
}

pub(crate) fn ensure(path: &Path) -> CliResult<()> {
    if fs::symlink_metadata(path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        ensure(parent(path)?)?;
        match fs::DirBuilder::new().mode(0o755).create(path) {
            Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(io)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io(error)),
        }
    }
    safe_ancestors(path)
}

pub(crate) fn physical_path(path: &Path) -> CliResult<PathBuf> {
    let path = shipping::absolute(path)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => path.canonicalize().map_err(io),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = physical_path(parent(&path)?)?;
            let name = path
                .file_name()
                .ok_or_else(|| fail("invalid installation path"))?;
            Ok(parent.join(name))
        }
        Err(error) => Err(io(error)),
    }
}

fn safe_ancestors(path: &Path) -> CliResult<()> {
    let uid = rustix::process::geteuid().as_raw();
    for dir in path.ancestors() {
        let meta = fs::symlink_metadata(dir).map_err(io)?;
        check(meta.is_dir(), "unsafe or symlinked installation directory")?;
        check(
            meta.uid() == 0 || meta.uid() == uid,
            "foreign installation ancestor refused",
        )?;
        check(
            meta.mode() & 0o022 == 0 || (meta.uid() == 0 && meta.mode() & 0o1000 != 0),
            "writable installation ancestor refused",
        )?;
    }
    Ok(())
}

fn owned(path: &Path) -> CliResult<()> {
    safe_ancestors(path)?;
    check(
        fs::symlink_metadata(path).map_err(io)?.uid() == rustix::process::geteuid().as_raw(),
        "foreign installation directory refused",
    )
}

fn owned_image(path: &Path) -> CliResult<()> {
    let meta = fs::symlink_metadata(path).map_err(io)?;
    check(
        meta.is_file()
            && meta.mode() & 0o7777 == 0o555
            && meta.uid() == rustix::process::geteuid().as_raw(),
        "unexpected snapshot executable owner or mode",
    )
}
