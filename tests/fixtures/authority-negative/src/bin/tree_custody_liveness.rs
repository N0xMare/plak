use plakous_hosted::agent_tree::CustodyLiveness;

struct ForgedLiveness;

impl CustodyLiveness for ForgedLiveness {
    fn holder_gone(&self, _holder: &plakous_hosted::agent_tree::CustodyHost) -> bool {
        true
    }
}

fn main() {}
