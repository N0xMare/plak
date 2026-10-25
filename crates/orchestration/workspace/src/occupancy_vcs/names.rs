//! Physical names shared by copy creation, reclamation and the fences.

/// The physical names of one tree-scoped copy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyNames {
    /// Directory basename under the backend's copy root.
    pub dir: String,
    /// Git fork branch; detached snapshots do not create it.
    pub branch: String,
    /// Native JJ workspace name.
    pub jj_workspace: String,
}

/// Names for an eight-hex tree prefix and a logical agent identity.
#[must_use]
pub fn copy_names(tree8: &str, agent: &str) -> CopyNames {
    CopyNames {
        dir: format!("{tree8}-{agent}"),
        branch: format!("plak/{tree8}/{agent}"),
        jj_workspace: format!("plak-{tree8}-{agent}"),
    }
}

pub(super) fn copy_parts(dir: &str) -> Option<(&str, &str)> {
    let (tree8, agent) = dir.split_once('-')?;
    (tree8.len() == 8
        && tree8.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !agent.is_empty()
        && agent
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then_some((tree8, agent))
}

/// A managed copy's Git branch, or `None` for an unrecognized directory name.
#[must_use]
pub fn branch_of_copy_dir(dir: &str) -> Option<String> {
    let (tree8, agent) = copy_parts(dir)?;
    Some(copy_names(tree8, agent).branch)
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_round_trip_without_accepting_legacy_or_traversal_names() {
        let names = super::copy_names("1234abcd", "c7");
        assert_eq!(names.dir, "1234abcd-c7");
        assert_eq!(names.branch, "plak/1234abcd/c7");
        assert_eq!(names.jj_workspace, "plak-1234abcd-c7");
        assert_eq!(super::branch_of_copy_dir(&names.dir), Some(names.branch));
        for name in ["c7", "1234abcd-../c7", "1234abcd-", "123456789-c7"] {
            assert_eq!(super::branch_of_copy_dir(name), None);
        }
    }
}
