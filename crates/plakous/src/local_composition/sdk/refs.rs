use plakous_compose::PythonHarnessSdkDescriptorV1 as Limits;
use std::collections::{BTreeMap, VecDeque};

#[derive(Default)]
pub(super) struct Refs {
    values: BTreeMap<String, Reference>,
    bytes: usize,
    order: VecDeque<String>,
}

pub(super) enum Reference {
    Text(String),
    JobOutput(String),
}

impl Reference {
    fn bytes(&self) -> usize {
        match self {
            Self::Text(text) | Self::JobOutput(text) => text.len(),
        }
    }
}

impl Refs {
    pub(super) fn insert(&mut self, text: String) -> Result<String, String> {
        let bytes = text.len();
        self.insert_reference(Reference::Text(text), bytes)
    }
    pub(super) fn job_output(&mut self, path: String) -> Result<String, String> {
        if let Some((id, _)) = self
            .values
            .iter()
            .find(|(_, value)| matches!(value,Reference::JobOutput(existing) if existing==&path))
        {
            return Ok(id.clone());
        }
        let bytes = path.len();
        self.insert_reference(Reference::JobOutput(path), bytes)
    }
    fn insert_reference(&mut self, value: Reference, bytes: usize) -> Result<String, String> {
        if bytes > Limits::REF_BYTES {
            return Err("SDK output reference capacity exceeded".into());
        }
        while self.values.len() >= Limits::REF_ENTRIES
            || self.bytes.saturating_add(bytes) > Limits::REF_BYTES
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(value) = self.values.remove(&oldest) {
                self.bytes -= value.bytes();
            }
        }
        let id = format!("ref_{}", uuid::Uuid::new_v4().simple());
        self.bytes += bytes;
        self.order.push_back(id.clone());
        self.values.insert(id.clone(), value);
        Ok(id)
    }
    pub(super) fn get(&self, id: &str) -> Option<&Reference> {
        self.values.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refs_are_opaque_and_bounded_by_count_and_retained_bytes() -> Result<(), String> {
        let mut refs = Refs::default();
        let id = refs.insert("x".into())?;
        assert_eq!(id.len(), 36);
        assert!(id.starts_with("ref_") && id[4..].bytes().all(|b| b.is_ascii_hexdigit()));
        for _ in 1..64 {
            refs.insert(String::new())?;
        }
        refs.insert(String::new())?;
        assert!(refs.get(&id).is_none());
        assert_eq!(refs.values.len(), Limits::REF_ENTRIES);
        let mut bytes = Refs::default();
        let oldest = bytes.insert("x".repeat(Limits::REF_BYTES))?;
        bytes.insert("x".into())?;
        assert!(bytes.get(&oldest).is_none());
        assert!(bytes.insert("x".repeat(Limits::REF_BYTES + 1)).is_err());
        assert!(refs.get("ref_not_issued").is_none());
        Ok(())
    }
}
