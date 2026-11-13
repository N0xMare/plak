use super::DormantTree;
use crate::tree::{CustodyRecord, MemoryTreeStore, TreeStoreError, digest_text};

impl MemoryTreeStore {
    pub(crate) fn install_move_seed_inner(
        &self,
        operation: &str,
        digest: &str,
        seed: &DormantTree,
        source: &CustodyRecord,
    ) -> Result<(), TreeStoreError> {
        let rows = seed.installed_rows().map_err(TreeStoreError::corrupt)?;
        if operation.is_empty() || digest.len() != 64 || source.fence >= i64::MAX.cast_unsigned() {
            return Err(TreeStoreError::corrupt(
                "invalid semantic tree install identity or fence",
            ));
        }
        let text = serde_json::to_string(seed)
            .map_err(|_| TreeStoreError::corrupt("invalid semantic tree base"))?;
        let expected = (operation.to_owned(), digest.to_owned(), digest_text(&text));
        let poisoned = || TreeStoreError::unavailable("memory tree store is poisoned");
        let mut custody = self.custody.lock().map_err(|_| poisoned())?;
        let mut trees = self.trees.lock().map_err(|_| poisoned())?;
        let mut installs = self.move_installs.lock().map_err(|_| poisoned())?;
        let id = seed.tree_id();
        if let Some(previous) = installs.get(id) {
            return if previous == &expected && trees.contains_key(id) {
                Ok(())
            } else {
                Err(TreeStoreError::corrupt(
                    "semantic tree install conflicts with its reservation",
                ))
            };
        }
        if trees.contains_key(id) || custody.contains_key(id) {
            return Err(TreeStoreError::corrupt(
                "semantic tree identity is already in use",
            ));
        }
        let mut floor = source.clone();
        floor.released = true;
        custody.insert(id.into(), floor);
        trees.insert(id.into(), rows.0);
        installs.insert(id.into(), expected);
        Ok(())
    }
}
