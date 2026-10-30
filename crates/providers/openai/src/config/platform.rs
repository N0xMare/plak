use super::{
    Arc, ConfigError, CredentialReference, Deserialize, Deserializer, Digest, DigestDomain,
    EndpointOrigin, OPENAI_PLATFORM_ORIGIN, OPENAI_PLATFORM_ROUTE_ID, OpenAiRoute,
    OpenAiRouteConformance, PlatformCredentialSource, PlatformProjectPolicy, PlatformStoragePolicy,
    ResponsesDialect, RouteAuth, Serialize, StorageMode, Url,
};

/// Serializable, secret-free `OpenAI` Platform route configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OpenAiPlatformRouteConfig {
    origin: EndpointOrigin,
    credential: CredentialReference,
    storage: PlatformStoragePolicy,
    project: PlatformProjectPolicy,
    #[serde(skip)]
    local_fixture: bool,
}

impl OpenAiPlatformRouteConfig {
    /// Creates first-party Platform route configuration with an explicit credential reference.
    ///
    /// # Errors
    ///
    /// Returns an error unless `origin` is exactly the official `OpenAI` Platform origin.
    pub fn new(
        origin: EndpointOrigin,
        credential: CredentialReference,
        storage: PlatformStoragePolicy,
        project: PlatformProjectPolicy,
    ) -> Result<Self, ConfigError> {
        if origin.as_str() != OPENAI_PLATFORM_ORIGIN {
            return Err(ConfigError::NonOfficialPlatformOrigin);
        }
        Ok(Self {
            origin,
            credential,
            storage,
            project,
            local_fixture: false,
        })
    }

    /// Creates an HTTPS-loopback-only route for deterministic local transport evidence.
    #[doc(hidden)]
    pub fn new_local_fixture(
        origin: EndpointOrigin,
        credential: CredentialReference,
        storage: PlatformStoragePolicy,
        project: PlatformProjectPolicy,
    ) -> Result<Self, ConfigError> {
        if !local_fixture_origin(&origin) {
            return Err(ConfigError::NonLocalFixtureOrigin);
        }
        Ok(Self {
            origin,
            credential,
            storage,
            project,
            local_fixture: true,
        })
    }

    /// Resolves configuration with an application-owned credential source.
    #[must_use]
    pub fn resolve(self, source: Arc<dyn PlatformCredentialSource>) -> OpenAiRoute {
        OpenAiRoute::new(
            OPENAI_PLATFORM_ROUTE_ID,
            ResponsesDialect::platform(),
            self.origin,
            match self.storage {
                PlatformStoragePolicy::StoreResponses => StorageMode::ProviderStored,
                PlatformStoragePolicy::Stateless => StorageMode::Stateless,
            },
            OpenAiRouteConformance::platform_o1(),
            RouteAuth::Platform {
                reference: self.credential,
                source,
                project: self.project,
                local_fixture: self.local_fixture,
            },
        )
    }

    /// Returns the exact non-secret account binding produced by this
    /// credential reference on the official Platform route.
    ///
    /// This is intended for an offline paid-campaign approval. It does not
    /// inspect or resolve credential bytes.
    #[must_use]
    pub fn account_binding(&self) -> Digest {
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"openai-platform-credential-reference",
                self.origin.as_str().as_bytes(),
                self.credential.as_str().as_bytes(),
                match &self.project {
                    PlatformProjectPolicy::DefaultProject => b"default".as_slice(),
                    PlatformProjectPolicy::Project(project) => project.as_str().as_bytes(),
                },
            ],
        )
        .unwrap_or_else(|error| unreachable!("binding digest version is valid: {error}"))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiPlatformRouteConfigWire {
    origin: EndpointOrigin,
    credential: CredentialReference,
    storage: PlatformStoragePolicy,
    project: PlatformProjectPolicy,
}

impl<'de> Deserialize<'de> for OpenAiPlatformRouteConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OpenAiPlatformRouteConfigWire::deserialize(deserializer)?;
        Self::new(wire.origin, wire.credential, wire.storage, wire.project)
            .map_err(serde::de::Error::custom)
    }
}

fn local_fixture_origin(origin: &EndpointOrigin) -> bool {
    let Ok(url) = Url::parse(origin.as_str()) else {
        return false;
    };
    url.scheme() == "https"
        && url.port().is_some_and(|port| port != 0)
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
}
