use super::*;

fn directory_rows() -> (Bindings, Value, Value) {
    let mut bindings = Bindings::new("source", Path::new("/fixture"));
    bindings.0.insert("@uuid:1".into(), json!("next"));
    let captured = json!({"rows":[
        {"session":"@uuid:0","title":"original source","activity_seq":15,"updated_at":"@time"},
        {"session":"@uuid:1","activity_seq":0,"updated_at":"@time"}
    ]});
    let current = json!({"rows":[
        {"session":"next","activity_seq":0,"updated_at":7},
        {"session":"source","title":"original source","activity_seq":15,"updated_at":7}
    ]});
    (bindings, captured, current)
}

#[test]
fn directory_replay_accepts_current_timestamp_and_uuid_order_without_losing_titles() {
    let (mut bindings, captured, current) = directory_rows();
    // The capture's timestamps are opaque. A tie in the current run puts
    // the sibling first by UUID, but both complete rows must still match.
    bindings.compare(&captured, &current);
    let mut later = current;
    later["rows"][1]["updated_at"] = json!(8);
    later["rows"].as_array_mut().unwrap().reverse();
    bindings.compare(&captured, &later);
}

#[test]
fn directory_replay_rejects_wrong_order_missing_rows_and_changed_metadata() {
    for mutation in 0..4 {
        let (mut bindings, captured, mut current) = directory_rows();
        match mutation {
            0 => current["rows"].as_array_mut().unwrap().reverse(),
            1 => {
                current["rows"].as_array_mut().unwrap().pop();
            }
            2 => {
                current["rows"][1].as_object_mut().unwrap().remove("title");
            }
            _ => current["rows"][1]["activity_seq"] = json!(0),
        }
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                bindings.compare(&captured, &current);
            }))
            .is_err(),
            "directory mutation {mutation} was accepted"
        );
    }
}
