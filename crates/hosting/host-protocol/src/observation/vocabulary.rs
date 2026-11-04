//! One variant vocabulary for received observations and content-only producers.

macro_rules! define_observations {
    ($p:ident; $($variants:tt)*) => {
        define_observations!(@parse $p [] [] []; $($variants)*);
    };
    (@parse $p:ident [$($all:tt)*] [$($content:tt)*] [$($arms:tt)*];) => {
        /// Observations the hosted session publishes to its driver: transcript
        /// and activity deltas, immutable snapshots, admission receipts and
        /// binding/usage projections. Every variant but `Extension` is closed.
        #[allow(clippy::large_enum_variant, reason = "bounded typed observation vocabulary")]
        #[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(
            tag = "kind",
            content = "body",
            rename_all = "snake_case",
            deny_unknown_fields,
            bound(
                serialize = "P::Intent: serde::Serialize, P::Observation: serde::Serialize",
                deserialize = "P::Intent: serde::de::DeserializeOwned, P::Observation: serde::de::DeserializeOwned"
            )
        )]
        pub enum SessionObservation<$p: crate::intent::ProtocolExtension = crate::intent::Core> {
            $($all)*
        }

        /// Producer content, without keyed transcript snapshots or patches.
        /// Keyed publication is outside this producer vocabulary.
        #[allow(clippy::large_enum_variant, reason = "bounded typed content vocabulary")]
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub enum ContentObservation<$p: crate::intent::ProtocolExtension = crate::intent::Core> {
            $($content)*
        }

        impl<$p: crate::intent::ProtocolExtension> From<ContentObservation<$p>> for SessionObservation<$p> {
            fn from(value: ContentObservation<$p>) -> Self {
                match value { $($arms)* }
            }
        }
    };
    (@parse $p:ident [$($all:tt)*] [$($content:tt)*] [$($arms:tt)*];
        $(#[$attr:meta])* PresentationSnapshot($ty:ty), $($rest:tt)*) => {
        define_observations!(@parse $p
            [$($all)* $(#[$attr])* PresentationSnapshot($ty),]
            [$($content)*] [$($arms)*]; $($rest)*);
    };
    (@parse $p:ident [$($all:tt)*] [$($content:tt)*] [$($arms:tt)*];
        $(#[$attr:meta])* TranscriptPatch($ty:ty), $($rest:tt)*) => {
        define_observations!(@parse $p
            [$($all)* $(#[$attr])* TranscriptPatch($ty),]
            [$($content)*] [$($arms)*]; $($rest)*);
    };
    (@parse $p:ident [$($all:tt)*] [$($content:tt)*] [$($arms:tt)*];
        $(#[$attr:meta])* $variant:ident($ty:ty), $($rest:tt)*) => {
        define_observations!(@parse $p
            [$($all)* $(#[$attr])* $variant($ty),]
            [$($content)* $(#[$attr])* $variant($ty),]
            [$($arms)* ContentObservation::$variant(value) => {
                let value: $ty = value;
                Self::$variant(value)
            },]; $($rest)*);
    };
    (@parse $p:ident [$($all:tt)*] [$($content:tt)*] [$($arms:tt)*];
        $(#[$attr:meta])* $variant:ident {
            $($(#[$field_attr:meta])* $field:ident: $ty:ty),* $(,)?
        }, $($rest:tt)*) => {
        define_observations!(@parse $p
            [$($all)* $(#[$attr])* $variant { $($(#[$field_attr])* $field: $ty,)* },]
            [$($content)* $(#[$attr])* $variant { $($(#[$field_attr])* $field: $ty,)* },]
            [$($arms)* ContentObservation::$variant { $($field,)* } => Self::$variant { $($field,)* },];
            $($rest)*);
    };
    (@parse $p:ident [$($all:tt)*] [$($content:tt)*] [$($arms:tt)*];
        $(#[$attr:meta])* $variant:ident, $($rest:tt)*) => {
        define_observations!(@parse $p
            [$($all)* $(#[$attr])* $variant,]
            [$($content)* $(#[$attr])* $variant,]
            [$($arms)* ContentObservation::$variant => Self::$variant,]; $($rest)*);
    };
}
