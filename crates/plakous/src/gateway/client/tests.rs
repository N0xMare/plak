use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plakous_auth::api_key::{
    ApiKey, ApiKeyError, ApiKeyRequest, ApiKeySource, CredentialProviderTag,
};
use plakous_auth::custody::{
    CustodyError, CustodySlot, KeyHeaderForm, KeyInjection, KeySource, LocalKey, LocalKeyCustodian,
    LocalKeyStore,
};
use plakous_http::{
    Bytes, Client, ExchangeControl, HeaderMap, HeaderName, HeaderValue, Limits, Method, Origin,
    Request, sensitive_header_value,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::GatewayCustody;
use crate::gateway::pins::openai::{OPENAI_API_CREDENTIAL_HEADERS, OPENAI_API_UPSTREAM_PINS};
use crate::gateway::server::{InProcessGateway, LocalCustody, new_instance};

const KEY: &str = "synthetic-key-one";
const ORIGIN: &str = "https://api.openai.com/";

struct Keys;

impl LocalKeyStore for Keys {
    fn key(&self, _slot: &CustodySlot) -> Result<Option<LocalKey>, CustodyError> {
        Ok(Some(LocalKey {
            key: ApiKey::new(KEY).map_err(|_| CustodyError::Store)?,
            source: KeySource::File,
            env_var: None,
        }))
    }
}

struct Selection;

impl ApiKeyRequest for Selection {
    fn provider(&self) -> CredentialProviderTag {
        CredentialProviderTag::OpenAiPlatform
    }
    fn origin(&self) -> &str {
        ORIGIN
    }
    fn credential_reference(&self) -> Option<&str> {
        Some("work")
    }
}

/// A loopback upstream recording each whole request and answering `200`.
async fn upstream() -> (Origin, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                match socket.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            recorded.lock().unwrap().push(request);
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .await;
            let _ = socket.shutdown().await;
        }
    });
    (
        Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), port).unwrap(),
        requests,
    )
}

fn request(origin: Origin, authorization: HeaderValue) -> Request {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(HeaderName::from_static("authorization"), authorization);
    headers.insert(
        HeaderName::from_static("openai-beta"),
        HeaderValue::from_static("responses=v1"),
    );
    Request::new(
        origin,
        Method::POST,
        "/v1/responses",
        headers,
        Bytes::from_static(b"{\"model\":\"m\",\"stream\":true}"),
        &Limits::default(),
    )
    .unwrap()
}

fn control() -> ExchangeControl {
    ExchangeControl::new(Instant::now() + Duration::from_secs(10))
}

#[tokio::test]
async fn custodied_keys_admit_the_local_generation_and_relay_identical_bytes() {
    let (fixture, requests) = upstream().await;
    let injection = |kind: &str| {
        (kind == "openai").then_some(KeyInjection {
            provider: CredentialProviderTag::OpenAiPlatform,
            header: "authorization",
            form: KeyHeaderForm::Bearer,
        })
    };
    let custody = LocalCustody::new(new_instance().unwrap()).with_kind(
        "openai",
        Arc::new(LocalKeyCustodian::new(Arc::new(Keys), injection)),
        OPENAI_API_UPSTREAM_PINS,
    );
    let server = InProcessGateway::server(custody, Client::new_http1(Limits::default()).unwrap())
        .with_dial_override("https://api.openai.com", fixture.clone());
    let gateway = GatewayCustody::connect(InProcessGateway::endpoint_for(&server), None, |kind| {
        (kind == "openai").then_some(OPENAI_API_CREDENTIAL_HEADERS)
    })
    .unwrap();
    let source = gateway
        .key_source(
            "openai/work",
            CredentialProviderTag::OpenAiPlatform,
            ORIGIN,
            Some("work".to_owned()),
            (),
        )
        .unwrap();
    let source: &dyn ApiKeySource<Selection> = &source;
    assert_eq!(
        source.generation_grant(&Selection),
        Err(ApiKeyError::Unavailable),
        "nothing is admitted before the attempt's facts are fetched"
    );
    source.prepare().await.unwrap();

    // The generation a session admits is the local key's exact generation.
    let local = ApiKey::new(KEY)
        .unwrap()
        .generation_grant(CredentialProviderTag::OpenAiPlatform, ());
    let grant = source.generation_grant(&Selection).unwrap();
    assert_eq!(grant, local);

    // The custodied field never carries the key, and the relayed request
    // reaches upstream byte for byte as the direct one with the local key.
    let field = source
        .credential_field(&Selection, &grant, KeyHeaderForm::Bearer)
        .unwrap()
        .into_header_bytes();
    assert!(
        !field
            .windows(KEY.len())
            .any(|window| window == KEY.as_bytes())
    );
    let relayed = gateway
        .transport()
        .client(Client::new_http1(Limits::default()).unwrap());
    let response = relayed
        .execute(
            request(
                Origin::parse(ORIGIN).unwrap(),
                sensitive_header_value(field).unwrap(),
            ),
            control(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let direct = Client::new_http1(Limits::default()).unwrap();
    let local_field = KeyHeaderForm::Bearer
        .field(&ApiKey::new(KEY).unwrap())
        .into_header_bytes();
    direct
        .execute(
            request(fixture, sensitive_header_value(local_field).unwrap()),
            control(),
        )
        .await
        .unwrap();
    let recorded = requests.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2);
    assert_eq!(
        String::from_utf8_lossy(&recorded[0]),
        String::from_utf8_lossy(&recorded[1])
    );
}
