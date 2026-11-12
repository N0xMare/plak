use super::{
    Arc, Controller, Duration, HashMap, Instant, NAMESPACE, Namespace, NamespaceKey,
    OperatorAnswer, OperatorOperation, OperatorRefusal, OperatorTerminalService, Path, RateBudget,
    Table, Terminal, TerminalDimensions, TerminalKey, TerminalStatus, VecDeque, WINDOW_BYTES, lock,
};

#[derive(Clone, Copy)]
struct Open<'a> {
    key: &'a TerminalKey,
    workspace: &'a str,
    dimensions: TerminalDimensions,
    digest: &'a str,
    now: Instant,
}

impl OperatorTerminalService {
    pub(super) fn apply(
        &self,
        key: NamespaceKey,
        connection: u64,
        digest: &str,
        operation: OperatorOperation,
    ) -> Result<OperatorAnswer, OperatorRefusal> {
        self.tick();
        let now = (self.inner.clock)();
        let mut table = lock(&self.inner.table);
        if matches!(operation, OperatorOperation::Renew) {
            return self.renew(&mut table, key, connection, now);
        }
        let namespace = table
            .namespaces
            .get(&key)
            .ok_or(OperatorRefusal::ReconcileRequired)?;
        if (namespace.closed || now.duration_since(namespace.renewed) >= NAMESPACE)
            && !matches!(
                operation,
                OperatorOperation::List
                    | OperatorOperation::Close { .. }
                    | OperatorOperation::NamespaceClose
            )
        {
            return Err(OperatorRefusal::NamespaceExpired);
        }
        if namespace.reconciled != connection && !matches!(operation, OperatorOperation::List) {
            return Err(OperatorRefusal::ReconcileRequired);
        }
        if matches!(operation, OperatorOperation::Attach { .. }) {
            let (mut total, mut host, mut principal) = (0, 0, 0);
            for (owner, ns) in &table.namespaces {
                let count = ns
                    .terminals
                    .values()
                    .filter(|t| t.controller.is_some())
                    .count();
                total += count;
                if owner.host == key.host {
                    host += count;
                    if owner.namespace.principal == key.namespace.principal {
                        principal += count;
                    }
                }
            }
            if total >= 32 || host >= 16 || principal >= 4 {
                return Err(OperatorRefusal::Quota);
            }
        }
        if let OperatorOperation::Open {
            key: terminal,
            workspace,
            dimensions,
        } = operation
        {
            if let Some((prior, refusal)) = namespace.refused_opens.get(&terminal) {
                return Err(if prior == digest {
                    *refusal
                } else {
                    OperatorRefusal::Conflict
                });
            }
            if namespace.refused_opens.len() >= 64 && !namespace.terminals.contains_key(&terminal) {
                return Err(OperatorRefusal::Quota);
            }
            let result = self.open(
                &mut table,
                &key,
                Open {
                    key: &terminal,
                    workspace: &workspace,
                    dimensions,
                    digest,
                    now,
                },
            );
            if let Err(refusal) = &result
                && let Some(ns) = table.namespaces.get_mut(&key)
                && ns.refused_opens.len() < 64
                && !ns.terminals.contains_key(&terminal)
            {
                ns.refused_opens
                    .insert(terminal, (digest.to_owned(), *refusal));
            }
            return result;
        }
        // Stream ids share the runner stream-number allocator only at dispatch.
        let stream = table.next_stream;
        table.next_stream += 1;
        let ns = table
            .namespaces
            .get_mut(&key)
            .ok_or(OperatorRefusal::NamespaceExpired)?;
        self.existing(ns, connection, stream, operation, now)
    }

    fn renew(
        &self,
        table: &mut Table,
        key: NamespaceKey,
        connection: u64,
        now: Instant,
    ) -> Result<OperatorAnswer, OperatorRefusal> {
        if table
            .namespaces
            .get(&key)
            .is_some_and(|ns| ns.closed || now.duration_since(ns.renewed) >= NAMESPACE)
        {
            return Err(OperatorRefusal::NamespaceExpired);
        }
        if !table.namespaces.contains_key(&key) && table.namespaces.len() >= 128 {
            return Err(OperatorRefusal::Quota);
        }
        super::collection::admit_generation(table, &key)?;
        // A new generation never adopts old shells, even if its browser retained ids.
        for (old, ns) in &mut table.namespaces {
            if old.host == key.host
                && old.namespace.state_key == key.namespace.state_key
                && old.generation != key.generation
            {
                ns.closed = true;
                for terminal in ns.terminals.values_mut() {
                    terminal.close();
                }
            }
        }
        let ns = table.namespaces.entry(key).or_insert_with(|| Namespace {
            loss_reported: super::HashSet::new(),
            evicted: super::HashSet::new(),
            renewed: now,
            reconciled: connection,
            closed: false,
            terminals: HashMap::new(),
            refused_opens: HashMap::new(),
        });
        ns.renewed = now;
        ns.reconciled = connection;
        Ok(self.rows(ns))
    }

    fn rows(&self, ns: &mut Namespace) -> OperatorAnswer {
        let mut terminals: Vec<_> = ns
            .terminals
            .iter()
            .map(|(key, t)| t.row(key, &self.inner.instance))
            .collect();
        terminals.sort_by(|a, b| a.key.terminal_id.as_str().cmp(b.key.terminal_id.as_str()));
        ns.loss_reported.extend(
            terminals
                .iter()
                .filter(|row| row.status == TerminalStatus::Lost)
                .map(|row| row.key.clone()),
        );
        OperatorAnswer::Table { terminals }
    }

    fn open(
        &self,
        table: &mut Table,
        namespace: &NamespaceKey,
        request: Open<'_>,
    ) -> Result<OperatorAnswer, OperatorRefusal> {
        let Open {
            key,
            workspace,
            dimensions,
            digest,
            now,
        } = request;
        let ns = table
            .namespaces
            .get(namespace)
            .ok_or(OperatorRefusal::NamespaceExpired)?;
        if ns.evicted.contains(&super::collection::terminal_hash(key)) {
            return Err(OperatorRefusal::TerminalLost);
        }
        if ns.evicted.len() >= 4096 {
            return Err(OperatorRefusal::Quota);
        }
        if let Some(terminal) = ns.terminals.get(key) {
            if terminal.digest != digest {
                return Err(OperatorRefusal::Conflict);
            }
            return Ok(OperatorAnswer::Terminal {
                terminal: terminal.row(key, &self.inner.instance),
                stream: None,
            });
        }
        Self::check_quotas(table, namespace, key)?;
        let launch = (self.inner.workspace)(Path::new(workspace))?;
        let workspace = launch.cwd.clone();
        if table
            .reclaiming
            .keys()
            .any(|path| workspace.starts_with(path) || path.starts_with(&workspace))
        {
            return Err(OperatorRefusal::TerminalBusy);
        }
        let ns = table
            .namespaces
            .get_mut(namespace)
            .ok_or(OperatorRefusal::NamespaceExpired)?;
        // Keep failed and exited ids in this bounded table: no forgotten-id respawn.
        if ns.terminals.len() >= 64 {
            return Err(OperatorRefusal::Quota);
        }
        let terminal = ns.terminals.entry(key.clone()).or_insert_with(|| Terminal {
            digest: digest.into(),
            workspace,
            status: TerminalStatus::Starting,
            last_input: now,
            detached: Some(now),
            controller: None,
            process: None,
            output: Arc::default(),
            input_next: 0,
            input_rate: RateBudget::new(now, WINDOW_BYTES),
            acknowledged: VecDeque::new(),
            acknowledged_bytes: 0,
            pending_size: None,
            exit_code: None,
        });
        // The reservation and spawn share the table lock, so simultaneous opens cannot
        // oversubscribe. Backend spawn never calls back into this service.
        match self
            .inner
            .backend
            .spawn(launch, dimensions, Arc::clone(&terminal.output))
        {
            Ok(process) => {
                terminal.process = Some(process);
                terminal.status = TerminalStatus::Running;
            }
            Err(refusal) => {
                ns.terminals.remove(key);
                return Err(refusal);
            }
        }
        Ok(OperatorAnswer::Terminal {
            terminal: terminal.row(key, &self.inner.instance),
            stream: None,
        })
    }

    fn check_quotas(
        table: &Table,
        namespace: &NamespaceKey,
        key: &TerminalKey,
    ) -> Result<(), OperatorRefusal> {
        let (mut runner, mut host, mut principal, mut session) = (0, 0, 0, 0);
        for (ns_key, ns) in &table.namespaces {
            for (terminal_key, terminal) in &ns.terminals {
                if !terminal.pinned() {
                    continue;
                }
                runner += 1;
                if ns_key.host != namespace.host {
                    continue;
                }
                host += 1;
                if terminal_key.session == key.session {
                    session += 1;
                }
                if ns_key.namespace.principal != namespace.namespace.principal {
                    continue;
                }
                principal += 1;
            }
        }
        if runner >= 32 || host >= 16 || principal >= 4 || session >= 2 {
            return Err(OperatorRefusal::Quota);
        }
        // Old-generation cleanup is a barrier before any replacement shell opens.
        if table.namespaces.iter().any(|(old, ns)| {
            old.host == namespace.host
                && old.namespace.state_key == namespace.namespace.state_key
                && old.generation != namespace.generation
                && ns.terminals.values().any(Terminal::pinned)
        }) {
            return Err(OperatorRefusal::TerminalBusy);
        }
        Ok(())
    }

    fn existing(
        &self,
        ns: &mut Namespace,
        connection: u64,
        stream: u64,
        operation: OperatorOperation,
        now: Instant,
    ) -> Result<OperatorAnswer, OperatorRefusal> {
        let key = match &operation {
            OperatorOperation::List => {
                ns.reconciled = connection;
                return Ok(self.rows(ns));
            }
            OperatorOperation::NamespaceClose => {
                ns.closed = true;
                for terminal in ns.terminals.values_mut() {
                    terminal.close();
                }
                return Ok(self.rows(ns));
            }
            OperatorOperation::Attach { key, .. }
            | OperatorOperation::Resize { key, .. }
            | OperatorOperation::Close { key, .. } => key.clone(),
            _ => return Err(OperatorRefusal::InvalidRequest),
        };
        let terminal = ns
            .terminals
            .get_mut(&key)
            .ok_or(OperatorRefusal::TerminalLost)?;
        let attached = match operation {
            OperatorOperation::Attach { cursor, .. } => {
                if cursor.instance != self.inner.instance {
                    return Err(OperatorRefusal::TerminalLost);
                }
                if cursor.offset > lock(&terminal.output).next {
                    return Err(OperatorRefusal::OffsetGap);
                }
                if terminal.controller.is_some() {
                    return Err(OperatorRefusal::TerminalBusy);
                }
                terminal.controller = Some(Controller {
                    connection,
                    stream,
                    inbound_seq: 0,
                    outbound_seq: 0,
                    credit: 0,
                    cursor: cursor.offset,
                    output_rate: RateBudget::new(
                        now.checked_sub(Duration::from_millis(64)).unwrap_or(now),
                        0,
                    ),
                    exit_status: None,
                });
                terminal.detached = None;
                Some(stream)
            }
            OperatorOperation::Resize { dimensions, .. } => {
                if terminal
                    .controller
                    .as_ref()
                    .is_none_or(|c| c.connection != connection)
                {
                    return Err(OperatorRefusal::TerminalBusy);
                }
                terminal.pending_size = Some(dimensions);
                None
            }
            OperatorOperation::Close {
                acknowledge_loss, ..
            } => {
                if terminal.status == TerminalStatus::TerminationUnknown {
                    if let Some(process) = &mut terminal.process {
                        process.reprove(acknowledge_loss);
                    }
                } else {
                    terminal.close();
                }
                None
            }
            _ => return Err(OperatorRefusal::InvalidRequest),
        };
        if terminal.status == TerminalStatus::Lost {
            ns.loss_reported.insert(key.clone());
        }
        Ok(OperatorAnswer::Terminal {
            terminal: terminal.row(&key, &self.inner.instance),
            stream: attached,
        })
    }
}
