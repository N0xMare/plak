//! Read-only target admission. Source databases are never opened here.
use super::*;
use plakous_agent::internal::host_move::MoveInstall;

impl SqliteSessionStore {
    /// Host-only preflight of fresh imported identities and ordinary logical capacity.
    /// No database, writer, owner or execution lease is created.
    ///
    /// # Errors
    /// Refuses existing identities, unsupported shape, corrupt object aliases or capacity.
    #[doc(hidden)]
    pub fn inspect_move_install(
        path: impl AsRef<Path>,
        install: &MoveInstall,
        options: SqliteStoreOptions,
    ) -> Result<(), SessionStoreError> {
        let mut connection = Self::open_inspection(path.as_ref())?;
        if let Some(c) = connection.as_ref()
            && classify_connection(c)? != SqlitePersistenceShape::Current
        {
            connection = None;
        }
        let snapshot = connection
            .as_mut()
            .map(Connection::transaction)
            .transpose()
            .map_err(map_sqlite)?;
        if let Some(c) = snapshot.as_deref() {
            admit_inspection_snapshot(c)?;
        }
        inspect(snapshot.as_deref(), install, options.limits)
    }
}

fn inspect(
    connection: Option<&Connection>,
    install: &MoveInstall,
    limits: MemoryStoreLimits,
) -> Result<(), SessionStoreError> {
    let stats = connection.map(read_stats).transpose()?;
    for session in install.sessions() {
        if let Some(c) = connection {
            require_unused_session_identity(c, session.session.session_id())?;
        }
        enforce(
            StoreCapacityBound::BranchesPerSession,
            u64n(session.branches.len()),
            limits.max_branches_per_session().get(),
        )?;
    }
    enforce(
        StoreCapacityBound::Sessions,
        stats
            .map_or(0, |s| s.sessions)
            .saturating_add(u64n(install.sessions().len())),
        limits.max_sessions().get(),
    )?;
    if let Some(c) = connection {
        for line in install.lines() {
            if c.query_row(
                "SELECT EXISTS(SELECT 1 FROM lines WHERE line_id=?1)",
                [id(line.line.line())],
                |row| row.get::<_, bool>(0),
            )
            .map_err(map_sqlite)?
            {
                return Err(SessionStoreError::IdentityConflict);
            }
        }
    }
    let bundles = crate::move_install::bundles(install)?;
    let mut objects = BTreeMap::new();
    for bundle in &bundles {
        let candidate = candidate(bundle)?;
        if let Some(c) = connection
            && (checkpoint_row(c, bundle.checkpoint_id())?.is_some()
                || pruned_checkpoint(c, bundle.checkpoint_id())?)
        {
            return Err(SessionStoreError::IdentityConflict);
        }
        for object in candidate.objects {
            if let Some(old) = objects.insert(object.address(), object.clone())
                && old != object
            {
                return Err(SessionStoreError::corrupt());
            }
        }
    }
    let mut count = stats.map_or(0, |s| s.objects);
    let mut bytes = stats.map_or(0, |s| s.object_bytes);
    for object in objects.values() {
        if let Some(c) = connection
            && object_exists(c, object.address())?
        {
            if load_object(c, object.address())?.as_ref() != Some(object) {
                return Err(SessionStoreError::corrupt());
            }
        } else {
            count = count.saturating_add(1);
            bytes = bytes.saturating_add(u64n(object.retained_bytes()));
        }
    }
    for (bound, value, maximum) in [
        (
            StoreCapacityBound::Checkpoints,
            stats
                .map_or(0, |s| s.checkpoints)
                .saturating_add(u64n(bundles.len())),
            limits.max_checkpoints().get(),
        ),
        (
            StoreCapacityBound::Objects,
            count,
            limits.max_objects().get(),
        ),
        (
            StoreCapacityBound::ObjectBytes,
            bytes,
            limits.max_object_bytes().get(),
        ),
    ] {
        admit(bound, value, maximum, StoreAdmission::Ordinary)?;
    }
    Ok(())
}
