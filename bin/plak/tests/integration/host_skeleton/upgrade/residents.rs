//! Settled actors release together without requiring a live client attachment.

use super::*;

#[test]
fn host_upgrade_settled_resident_and_cold_roots_need_no_attachment_for_release() {
    for cold in [false, true] {
        let Some(fixture) = fixture() else { return };
        let _owner = HomeHost(fixture.home());
        let roots = (1..=4)
            .map(|ordinal| seed(&fixture, ordinal))
            .collect::<Vec<_>>();
        let mut host = start(&fixture);
        {
            let mut client = Client::connect(&host);
            for (ordinal, root) in roots.iter().enumerate() {
                let attachment = client.attach(
                    10 + ordinal as u64,
                    &NodeAddress {
                        tree_id: root.clone(),
                        agent_id: root.clone(),
                    },
                );
                assert_eq!(
                    client.request(20 + ordinal as u64, Op::Detach { attachment }),
                    Outcome::Ok(Answer::Detached)
                );
            }
        }
        if cold {
            stop(&mut host);
            host = start(&fixture);
        }
        let result = upgrade(&fixture, &binary(&fixture));
        assert!(
            result.status.success(),
            "cold={cold}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let rows = receipts(&fixture);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["phase"], "acknowledged");
        for source in roots {
            assert_ne!(successor(&fixture, &source), source);
        }
        stop(&mut host);
    }
}
