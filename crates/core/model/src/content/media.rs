use serde::{Deserialize, Deserializer, Serialize};

use super::{
    ContentError, MAX_CANONICAL_BLOCK_BYTES, MAX_MEDIA_TYPE_BYTES, MAX_MEDIA_URI_BYTES,
    ensure_maximum, ensure_nonempty,
};

/// An owned media source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MediaSource {
    kind: MediaSourceKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum MediaSourceKind {
    Uri(String),
    DataUrl(String),
    Bytes(#[serde(with = "canonical_base64")] Vec<u8>),
}

/// Inline media bytes as one padded standard base64 string.
///
/// Decoding accepts only the exact base64 text encoding would produce. JSON
/// escapes inside the string are resolved before this sees it; canonical
/// history refuses them because `HistorySegment::from_canonical` requires its
/// input to equal the re-encoded bytes.
mod canonical_base64 {
    use std::fmt;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserializer, Serializer, de};

    use super::MAX_CANONICAL_BLOCK_BYTES;

    const MAX_ENCODED_BYTES: usize = MAX_CANONICAL_BLOCK_BYTES.div_ceil(3) * 4;

    #[allow(clippy::ptr_arg, reason = "serde `with` passes the field by reference")]
    pub(super) fn serialize<S>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("padded standard base64 media bytes")
            }

            fn visit_str<E>(self, encoded: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if encoded.len() > MAX_ENCODED_BYTES {
                    return Err(E::custom("media bytes exceed the canonical block limit"));
                }
                // The STANDARD engine requires canonical padding and rejects
                // nonzero trailing bits, whitespace and the URL-safe alphabet.
                STANDARD
                    .decode(encoded)
                    .map_err(|_| E::custom("media bytes are not canonical padded base64"))
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

#[derive(Deserialize)]
struct MediaSourceWire {
    kind: MediaSourceKind,
}

impl<'de> Deserialize<'de> for MediaSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MediaSourceWire::deserialize(deserializer)?;
        match wire.kind {
            MediaSourceKind::Uri(uri) => Self::uri(uri),
            MediaSourceKind::DataUrl(data_url) => Self::data_url(data_url),
            MediaSourceKind::Bytes(bytes) => Self::bytes(bytes),
        }
        .map_err(serde::de::Error::custom)
    }
}

impl MediaSource {
    /// Creates a non-empty URI media source.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when the URI is empty.
    pub fn uri(uri: impl Into<String>) -> Result<Self, ContentError> {
        let uri = uri.into();
        ensure_nonempty(&uri, "media URI")?;
        ensure_maximum(uri.len(), "media URI", MAX_MEDIA_URI_BYTES)?;
        Ok(Self {
            kind: MediaSourceKind::Uri(uri),
        })
    }

    /// Creates an exact, bounded data URL media source.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is not a non-empty `data:` URL or is oversized.
    pub fn data_url(data_url: impl Into<String>) -> Result<Self, ContentError> {
        let data_url = data_url.into();
        if !data_url.starts_with("data:") {
            return Err(ContentError::InvalidDataUrl);
        }
        ensure_maximum(data_url.len(), "media data URL", MAX_CANONICAL_BLOCK_BYTES)?;
        Ok(Self {
            kind: MediaSourceKind::DataUrl(data_url),
        })
    }

    /// Creates a non-empty inline byte source.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when no bytes are supplied.
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Result<Self, ContentError> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(ContentError::Empty {
                field: "media bytes",
            });
        }
        ensure_maximum(bytes.len(), "media bytes", MAX_CANONICAL_BLOCK_BYTES)?;
        Ok(Self {
            kind: MediaSourceKind::Bytes(bytes),
        })
    }

    /// Returns the URI when this source references one.
    #[must_use]
    pub fn as_uri(&self) -> Option<&str> {
        match &self.kind {
            MediaSourceKind::Uri(uri) => Some(uri),
            MediaSourceKind::DataUrl(_) | MediaSourceKind::Bytes(_) => None,
        }
    }

    /// Returns the exact data URL when this source retains one.
    #[must_use]
    pub fn as_data_url(&self) -> Option<&str> {
        match &self.kind {
            MediaSourceKind::DataUrl(data_url) => Some(data_url),
            MediaSourceKind::Uri(_) | MediaSourceKind::Bytes(_) => None,
        }
    }

    /// Returns bytes when this source is inline.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match &self.kind {
            MediaSourceKind::Bytes(bytes) => Some(bytes),
            MediaSourceKind::Uri(_) | MediaSourceKind::DataUrl(_) => None,
        }
    }

    /// Encoded image payload bytes on the wire, or unknown for a remote URI.
    /// Inline bytes lower to padded base64. A data URL retains its exact
    /// encoded payload, without fetching or decoding it.
    #[must_use]
    pub fn encoded_image_bytes(&self) -> Option<u64> {
        match &self.kind {
            MediaSourceKind::Bytes(bytes) => Some((bytes.len() as u64).div_ceil(3) * 4),
            MediaSourceKind::DataUrl(url) => {
                url.split_once(',').map(|(_, payload)| payload.len() as u64)
            }
            MediaSourceKind::Uri(_) => None,
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        match &self.kind {
            MediaSourceKind::Uri(uri) => uri.len(),
            MediaSourceKind::DataUrl(data_url) => data_url.len(),
            MediaSourceKind::Bytes(bytes) => bytes.len(),
        }
    }
}

macro_rules! media_block {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, Eq, PartialEq, Serialize)]
        pub struct $name {
            media_type: String,
            source: MediaSource,
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                #[derive(Deserialize)]
                struct Wire {
                    media_type: String,
                    source: MediaSource,
                }

                let wire = Wire::deserialize(deserializer)?;
                Self::new(wire.media_type, wire.source).map_err(serde::de::Error::custom)
            }
        }

        impl $name {
            /// Creates a media block with a non-empty media type.
            ///
            /// # Errors
            ///
            /// Returns [`ContentError::Empty`] when the media type is empty.
            pub fn new(
                media_type: impl Into<String>,
                source: MediaSource,
            ) -> Result<Self, ContentError> {
                let media_type = media_type.into();
                ensure_nonempty(&media_type, "media type")?;
                ensure_maximum(media_type.len(), "media type", MAX_MEDIA_TYPE_BYTES)?;
                Ok(Self { media_type, source })
            }

            /// Returns the declared media type.
            #[must_use]
            pub fn media_type(&self) -> &str {
                &self.media_type
            }

            /// Returns the media source.
            #[must_use]
            pub const fn source(&self) -> &MediaSource {
                &self.source
            }
        }
    };
}

/// Provider-neutral image rendering detail retained for exact family lowering.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ImageDetailHint {
    /// Let the target choose detail.
    Auto,
    /// Prefer low detail.
    Low,
    /// Prefer high detail.
    High,
    /// Preserve original detail.
    Original,
}

/// A canonical image block.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImageBlock {
    media_type: String,
    source: MediaSource,
    detail: Option<ImageDetailHint>,
}

#[derive(Deserialize)]
struct ImageBlockWire {
    media_type: String,
    source: MediaSource,
    #[serde(default)]
    detail: Option<ImageDetailHint>,
}

impl<'de> Deserialize<'de> for ImageBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ImageBlockWire::deserialize(deserializer)?;
        Self::with_detail(wire.media_type, wire.source, wire.detail)
            .map_err(serde::de::Error::custom)
    }
}

impl ImageBlock {
    /// Creates an image without an explicit rendering detail.
    ///
    /// # Errors
    ///
    /// Returns an error when the media type is empty or oversized.
    pub fn new(media_type: impl Into<String>, source: MediaSource) -> Result<Self, ContentError> {
        Self::with_detail(media_type, source, None)
    }

    /// Creates an image with an optional rendering detail.
    ///
    /// # Errors
    ///
    /// Returns an error when the media type is empty or oversized.
    pub fn with_detail(
        media_type: impl Into<String>,
        source: MediaSource,
        detail: Option<ImageDetailHint>,
    ) -> Result<Self, ContentError> {
        let media_type = media_type.into();
        ensure_nonempty(&media_type, "media type")?;
        ensure_maximum(media_type.len(), "media type", MAX_MEDIA_TYPE_BYTES)?;
        Ok(Self {
            media_type,
            source,
            detail,
        })
    }

    #[must_use]
    /// Returns the declared media type.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    #[must_use]
    /// Returns the retained media source.
    pub const fn source(&self) -> &MediaSource {
        &self.source
    }

    #[must_use]
    /// Returns the optional rendering detail hint.
    pub const fn detail(&self) -> Option<ImageDetailHint> {
        self.detail
    }
}

media_block!(DocumentBlock, "A canonical document block.");
media_block!(AudioBlock, "A canonical audio block.");
