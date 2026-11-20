//! Host semantic DTOs use the facade's closed locator spelling, not its private fields.
use plakous::sessions::SessionLocator;
use serde::{Deserialize, Deserializer, Serializer};

pub(crate) fn serialize<S: Serializer>(
    locator: &SessionLocator,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&locator.to_string())
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<SessionLocator, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}
