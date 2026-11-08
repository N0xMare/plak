//! An external product can use host payloads without a session intent.

use plakous_host_protocol::wire::{
    Answer, ClientFrame, HostFrame, Op, Outcome, WIRE_VERSION, Welcome,
};
use plakous_host_protocol::{Never, ProtocolExtension};

#[derive(Clone, Debug, Eq, PartialEq)]
struct Product;
impl ProtocolExtension for Product {
    type Intent = Never;
    type Observation = Never;
    type HostRequest = bool;
    type HostResponse = u8;
    type WelcomeExtension = Option<bool>;
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let request = ClientFrame::<Product>::Request {
        id: 7,
        op: Op::Host { request: true },
    };
    let encoded = serde_json::to_value(&request)?;
    assert_eq!(
        serde_json::from_value::<ClientFrame<Product>>(encoded)?,
        request
    );
    let response = HostFrame::<Product>::Response {
        id: 7,
        outcome: Outcome::Ok(Answer::Host { response: 9 }),
    };
    assert_eq!(
        serde_json::from_value::<HostFrame<Product>>(serde_json::to_value(&response)?)?,
        response
    );
    let welcome = Welcome::<Product> {
        extension: Some(true),
        proto: WIRE_VERSION,
        binary_id: "fixture".into(),
        host_generation: 1,
        caps: Vec::new(),
        session: None,
    };
    assert_eq!(
        serde_json::from_value::<Welcome<Product>>(serde_json::to_value(&welcome)?)?,
        welcome
    );
    Ok(())
}
