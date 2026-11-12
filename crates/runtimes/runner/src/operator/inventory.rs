//! Paired-host recovery and connection-scoped exclusion; no persisted state.

use super::{
    NamespaceKey, OperatorAnswer, OperatorId, OperatorRefusal, OperatorTerminalService, Path,
    ReclamationLease, Table, TerminalPin, resolved_workspace,
};

impl OperatorTerminalService {
    pub(super) fn inventory(
        &self,
        table: &Table,
        key: &NamespaceKey,
    ) -> Result<OperatorAnswer, OperatorRefusal> {
        let mut pins = Vec::new();
        for (owner, ns) in &table.namespaces {
            if owner.host != key.host || owner.namespace.state_key != key.namespace.state_key {
                continue;
            }
            for (key, terminal) in &ns.terminals {
                if !terminal.pinned() {
                    continue;
                }
                pins.push(TerminalPin {
                    generation: OperatorId::try_from(owner.generation.clone())
                        .map_err(|_| OperatorRefusal::InvalidRequest)?,
                    namespace: owner.namespace.clone(),
                    workspace: terminal
                        .workspace
                        .to_str()
                        .ok_or(OperatorRefusal::WorkspaceUnavailable)?
                        .into(),
                    digest: terminal.digest.clone(),
                    terminal: terminal.row(key, &self.inner.instance),
                });
            }
        }
        if pins.len() > 32 {
            return Err(OperatorRefusal::Quota);
        }
        pins.sort_by(|a, b| {
            (
                a.generation.as_str(),
                a.namespace.principal.as_str(),
                a.terminal.key.terminal_id.as_str(),
            )
                .cmp(&(
                    b.generation.as_str(),
                    b.namespace.principal.as_str(),
                    b.terminal.key.terminal_id.as_str(),
                ))
        });
        Ok(OperatorAnswer::Inventory {
            instance: self.inner.instance.clone(),
            pins,
        })
    }

    pub(super) fn reclaim_stream(
        table: &mut Table,
        key: NamespaceKey,
        connection: u64,
        path: &str,
    ) -> Result<OperatorAnswer, OperatorRefusal> {
        let workspace = resolved_workspace(Path::new(path))?;
        if table.leases.len() >= 32 {
            return Err(OperatorRefusal::Quota);
        }
        if table.namespaces.values().any(|ns| {
            ns.terminals.values().any(|terminal| {
                terminal.pinned()
                    && (terminal.workspace.starts_with(&workspace)
                        || workspace.starts_with(&terminal.workspace))
            })
        }) {
            return Err(OperatorRefusal::TerminalBusy);
        }
        let stream = table.next_stream;
        table.next_stream += 1;
        *table.reclaiming.entry(workspace.clone()).or_default() += 1;
        table.leases.insert(
            stream,
            ReclamationLease {
                host: key.host,
                generation: key.generation,
                connection,
                workspace,
            },
        );
        Ok(OperatorAnswer::Reclamation {
            stream: Some(stream),
        })
    }
}
