//! Count encoded bytes without allocating a serialization buffer first.

pub(super) fn size(value: &impl serde::Serialize, maximum: usize) -> Option<usize> {
    let mut writer = Count { bytes: 0, maximum };
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.bytes)
}
struct Count {
    bytes: usize,
    maximum: usize,
}
impl std::io::Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|length| *length <= self.maximum)
            .ok_or_else(|| std::io::Error::other("console response exceeds its bound"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
