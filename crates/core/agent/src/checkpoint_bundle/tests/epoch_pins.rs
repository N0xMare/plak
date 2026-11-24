use super::*;

const FRAME: u32 = 38;

type ClosurePins = [[&'static str; 3]; 4];

const EXACT_38: ClosurePins = [
    [
        "426a0280068dd4e2b95b0ef0794377810e5a514c2daa5912d98cb706e23ac7f1",
        "80277d907953f219c902da2e0111da2a2ece3307fd837b461d6d5ff7dce4e0f4",
        "426d27b672f2d1908f4c1944d771c55a4bc1ed8a6dc3515f94ee16a8c9ed8529",
    ],
    [
        "9e0f22f794b7cf9622af2a0e8e713ceedb38d76d506ea97cd3bb4d610ea5f813",
        "0959c6228a1f1857c2f309349a0a49ff2126d187ec695324f4e5010dcec15e4e",
        "c1a33068544f9b8f6ce5632af0852f20bad089120e0878a9ba54cddc26cde2a6",
    ],
    [
        "161bfa37428da143591df2d2b3f5eca2dcdd1e5c650f8e77c7f2211a456009f6",
        "16a5b78c35ed52021c5637d15f3085c69b3566cd12c36aace32e8d449d900bfa",
        "aa63861230c123aea1b4ed0141901bc7fb6f2f7430983feb917809d2bba8e1b6",
    ],
    [
        "83bbe57899ba359f3a393abc3dc2895245abc77e93208171c0a3e3c4276842f5",
        "6d16817992b6cd37844fad4c8c439ed13d4fac2b45dc50ed0b5678bd9d97c434",
        "ce4df89b5ce0b8084282154d87f4bad8addb447a80235cb31a564ced2e0f82cd",
    ],
];

const EXACT_39: ClosurePins = [
    [
        "c1926de28e8abcb13e56075837a017568511d3bb355edf97a17eae5466fa44f5",
        "02a87a1372598fa2b71d8c5375909490240f6ca11fd24f2ed2f970a5e41ec704",
        "6db4843fa8f6ffd907942631877a729daeb7f987910292343ca4c9d97ff83362",
    ],
    [
        "b741f8f81eeec3cf408698d89e8299f940eba49dd6bf6ed20b7e5c44dd2ed61c",
        "a7bfeda7ab4ff0d43ddf6856e9568dec9edbc26686b5a3800aa9274d2a4874c3",
        "3d5eda11dafd19e312788c3b142756a7495b0c284ee9ca7a1bca73f393f5fa42",
    ],
    [
        "cfa9b41936d40e6a074c06fa2decc130ef30f6b343cc2171f851b0b5c8894990",
        "16c3bd2db45f03cb5148e80d018dfc559582f2a7a93555cdf79d4cc583feecb1",
        "b41775a827fd12e45102d326fd0fbf217d1a761338995668678fee6a454c2dd2",
    ],
    [
        "f2a761cfc798c56bd6ca178f8571891d69a39c40b544c623a96c904c7a96331e",
        "71a361361a1d5ed09de9c2903cf4ae2e259b796f927146811a10f0898552bf1d",
        "0d45adfc1b762ca304f2b090defcfcc2adb35b662b794493aececd3d3f7908fc",
    ],
];

struct Pins {
    epoch: u32,
    exact: ClosurePins,
    // Exact closures at FRAME, including rederived named-state and ledger proofs.
    fixed: ClosurePins,
    authority: [u8; 32],
}

const PINS: [Pins; 2] = [
    Pins {
        epoch: 38,
        exact: EXACT_38,
        fixed: EXACT_38,
        authority: [
            0x5a, 0xe7, 0x40, 0xd1, 0x0e, 0x48, 0xe1, 0x90, 0xdb, 0x52, 0xcd, 0xfc, 0x23, 0x39,
            0x95, 0x4b, 0x1d, 0xc5, 0x5b, 0xfa, 0x2c, 0x0e, 0x6d, 0x82, 0x12, 0x12, 0x23, 0xad,
            0xb0, 0x04, 0x7a, 0xb6,
        ],
    },
    Pins {
        epoch: 39,
        exact: EXACT_39,
        fixed: EXACT_38,
        authority: [
            0x40, 0x43, 0x33, 0xc7, 0xc1, 0xcc, 0xd1, 0xbf, 0xbd, 0x7f, 0x3a, 0x8a, 0xa1, 0x9c,
            0xcd, 0xc2, 0x68, 0x93, 0x94, 0x28, 0x8a, 0x03, 0xe3, 0x4d, 0x92, 0xd2, 0xae, 0xf0,
            0xea, 0x25, 0x07, 0x08,
        ],
    },
];

fn pins(epoch: u32) -> TestResult<&'static Pins> {
    PINS.iter()
        .find(|pin| pin.epoch == epoch)
        .ok_or_else(|| format!("no checkpoint pin for encoding epoch {epoch}").into())
}

fn fixtures() -> TestResult<[RuntimeCheckpoint; 4]> {
    Ok([
        named_state_fixture(3)?,
        fixture(3, 3, false)?,
        compaction_working_fixture(5)?,
        anchored_quiescent_fixture()?,
    ])
}

fn fingerprints(bundle: &CheckpointObjectBundle, frame: u32) -> TestResult<[String; 3]> {
    let mut exact = Vec::new();
    for object in bundle.objects() {
        let address = object.address();
        exact.push(address.kind() as u8);
        exact.extend_from_slice(&address.codec_revision().to_be_bytes());
        exact.extend_from_slice(address.digest().bytes());
        exact.extend_from_slice(&u32::try_from(object.canonical_bytes().len())?.to_be_bytes());
        exact.extend_from_slice(object.canonical_bytes());
        exact.extend_from_slice(&u32::try_from(object.children().len())?.to_be_bytes());
        for child in object.children() {
            exact.push(child.kind() as u8);
            exact.extend_from_slice(&child.codec_revision().to_be_bytes());
            exact.extend_from_slice(child.digest().bytes());
        }
    }
    let closure = Digest::sha256(
        DigestDomain::Blob,
        frame,
        &[
            b"plakous-agent/runtime-checkpoint-exact-closure-golden-v1",
            &exact,
        ],
    )?;
    Ok([
        bundle.root().digest().to_string(),
        bundle.checkpoint_integrity().to_string(),
        closure.to_string(),
    ])
}

#[test]
fn checkpoint_pins_bind_exact_bytes_at_each_readable_epoch() -> TestResult {
    assert_eq!(PREVIOUS_RUNTIME_FORMAT_EPOCH + 1, RUNTIME_FORMAT_EPOCH);
    for epoch in [PREVIOUS_RUNTIME_FORMAT_EPOCH, RUNTIME_FORMAT_EPOCH] {
        for (checkpoint, expected) in fixtures()?.into_iter().zip(pins(epoch)?.exact) {
            let fixture = RuntimeCheckpoint::new_at(checkpoint.manifest().clone(), epoch)?;
            let bundle = if epoch == RUNTIME_FORMAT_EPOCH {
                RuntimeCheckpointCodec::bundle(&fixture, None)?
            } else {
                RuntimeCheckpointCodec::bundle_full(&fixture)?
            };
            assert_eq!(
                bundle.objects().len(),
                34 + checkpoint.manifest().named_state().values().len()
            );
            assert_eq!(fingerprints(&bundle, epoch)?, expected);
            assert_eq!(RuntimeCheckpointCodec::materialize(&bundle)?, fixture);
        }
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct Payload {
    graph: Vec<[String; 3]>,
    feed: Vec<String>,
    authority: RuntimeObjectAddress,
}

fn framed_graph(checkpoint: &RuntimeCheckpoint, epoch: u32) -> TestResult<[String; 3]> {
    let fixture = RuntimeCheckpoint::new_at(checkpoint.manifest().clone(), epoch)?;
    fingerprints(&RuntimeCheckpointCodec::bundle_full(&fixture)?, epoch)
}

// The epoch-39 change was in the authority input to admission evidence, not
// in the grammar of the model-independent graph fixtures. These credential-free
// authority digests are frozen outputs of both historical derivations for one
// fixture slot. Hash the persisted component, not an epoch tag or timestamp.
fn framed_authority(bytes: [u8; 32]) -> TestResult<RuntimeObjectAddress> {
    let proof = Digest::from_sha256_bytes(DigestDomain::ModelBinding, 1, bytes)?;
    let component = ReconstructiveComponent::new(
        RuntimeComponentKind::AdmissionEvidence,
        1,
        serde_json::to_vec(&proof)?,
    )?;
    Ok(object_address_at(
        RuntimeObjectKind::ReconstructiveComponent,
        &encode_bundle_component(&component)?,
        FRAME,
    )?)
}

fn allows_epoch_change(previous: &Payload, candidate: &Payload) -> bool {
    previous != candidate
}

#[test]
fn changed_epoch_requires_a_real_payload_change_under_fixed_framing() -> TestResult {
    let current = pins(RUNTIME_FORMAT_EPOCH)?;
    let previous = pins(PREVIOUS_RUNTIME_FORMAT_EPOCH)?;
    let graph = fixtures()?
        .iter()
        .map(|cp| framed_graph(cp, FRAME))
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(graph, current.fixed);
    let current_payload = Payload {
        graph,
        feed: crate::feed::fixed_feed_fingerprints()?,
        authority: framed_authority(current.authority)?,
    };
    let previous_payload = Payload {
        graph: previous.fixed.map(|row| row.map(str::to_owned)).to_vec(),
        feed: crate::feed::feed_pins(previous.epoch)?
            .map(str::to_owned)
            .to_vec(),
        authority: framed_authority(previous.authority)?,
    };
    assert!(allows_epoch_change(&previous_payload, &current_payload));

    let fixture = named_state_fixture(3)?;
    let bumped_epoch = current.epoch + 1;
    assert_ne!(
        framed_graph(&fixture, bumped_epoch)?,
        framed_graph(&fixture, current.epoch)?,
        "addresses and proofs change if the tag alone changes"
    );
    let synthetic = Payload {
        graph: fixtures()?
            .iter()
            .map(|cp| {
                let bumped = RuntimeCheckpoint::new_at(cp.manifest().clone(), bumped_epoch)?;
                framed_graph(&bumped, FRAME)
            })
            .collect::<TestResult<Vec<_>>>()?,
        feed: crate::feed::fixed_feed_fingerprints()?,
        authority: framed_authority(current.authority)?,
    };
    assert!(!allows_epoch_change(&current_payload, &synthetic));
    Ok(())
}
