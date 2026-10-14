//! One walker over every pack's pins.
//!
//! A refactor that moves text may not move a pin. A deliberate text change
//! re-pins here, in one place, from the digest the failure prints.

use std::collections::BTreeSet;

use plakous_compose::standard_coding_menus;
use plakous_tools::PromptDigest;

#[test]
fn every_pack_pin_matches_its_text() {
    let mut drifted = Vec::new();
    for pack in standard_coding_menus(plakous_tools::JobConfig::default()) {
        for pin in pack.pins() {
            let actual = PromptDigest::of(pin.text());
            if actual != pin.digest() {
                drifted.push(format!(
                    "{} {} -> \"{}\"",
                    pack.id(),
                    pin.id(),
                    actual.to_hex()
                ));
            }
        }
    }
    assert!(drifted.is_empty(), "re-pin:\n{}", drifted.join("\n"));
}

#[test]
fn pack_pin_ids_are_unique_and_no_string_is_pinned_twice() {
    let mut ids = BTreeSet::new();
    let mut digests = BTreeSet::new();
    for pack in standard_coding_menus(plakous_tools::JobConfig::default()) {
        for pin in pack.pins() {
            assert!(ids.insert(pin.id()), "duplicate pin id {}", pin.id());
            assert!(
                digests.insert(PromptDigest::of(pin.text())),
                "{} pins a string that is already pinned",
                pin.id()
            );
        }
    }
    assert!(!ids.is_empty());
}
