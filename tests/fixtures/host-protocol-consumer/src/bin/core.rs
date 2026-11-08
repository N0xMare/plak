//! An extension-free external client has no product payload or implicit alias.

use plakous_host_protocol::Core;
use plakous_host_protocol::wire::{ClientFrame, HostFrame, Op, WIRE_VERSION, Welcome};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let request = ClientFrame::<Core>::Request {
        id: 7,
        op: Op::Ping,
    };
    assert_eq!(
        serde_json::from_value::<ClientFrame<Core>>(serde_json::to_value(&request)?)?,
        request
    );
    let frame = HostFrame::<Core>::Welcome(Welcome::<Core> {
        extension: None,
        proto: WIRE_VERSION,
        binary_id: "fixture".into(),
        host_generation: 1,
        caps: Vec::new(),
        session: None,
    });
    let mut encoded = serde_json::to_value(&frame)?;
    assert!(encoded["extension"].is_null());
    assert_eq!(
        serde_json::from_value::<HostFrame<Core>>(encoded.clone())?,
        frame
    );
    if let Some(object) = encoded.as_object_mut() {
        object.remove("extension");
    }
    assert!(serde_json::from_value::<HostFrame<Core>>(encoded).is_err());
    assert!(
        serde_json::from_value::<ClientFrame<Core>>(
            serde_json::json!({"frame":"request","id":8,"op":{"op":"host","request":true}})
        )
        .is_err()
    );
    Ok(())
}
