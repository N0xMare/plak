use plakd_wire_types::v13::LiveWork;
use plakous_hosted::host_service::KeptActivity;

pub(super) fn live_work(resident: Option<(&KeptActivity, usize)>) -> LiveWork {
    fn bounded(count: usize) -> u32 {
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    resident.map_or_default(|(kept, running_children)| LiveWork {
        children: bounded(running_children),
        processes: bounded(kept.processes),
        held_children: bounded(kept.held_children),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_counts_use_running_not_all_owned_work_and_clear_on_release() {
        let kept = KeptActivity {
            children: 4,
            processes: 1,
            held_children: 1,
            ..KeptActivity::default()
        };
        assert_eq!(
            live_work(Some((&kept, 2))),
            LiveWork {
                children: 2,
                processes: 1,
                held_children: 1,
            }
        );
        assert_eq!(live_work(None), LiveWork::default());
    }
}
