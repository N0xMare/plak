use super::*;

#[test]
fn glibc_requirements_are_compared_numerically_and_fail_closed() {
    let raw = "  0x0010: Name: GLIBC_2.9 Flags: none\n  0x0020: Name: GLIBC_2.34 Flags: none\n";
    assert_eq!(required_glibc(raw).unwrap(), [2, 34, 0]);
    let with_abi = format!("Name: GLIBC_ABI_DT_RELR\n{raw}");
    assert_eq!(required_glibc(&with_abi).unwrap(), [2, 34, 0]);
    assert!(required_glibc("Name: GLIBC_ABI_DT_RELR").is_err());
    assert!(required_glibc("Name: GLIBC_2.invalid").is_err());
    assert!(required_glibc("Name: GLIBC_PRIVATE").is_err());
    assert!(required_glibc("no version information").is_err());
    assert!(glibc_version("2.34.x").is_err());
    assert_eq!(glibc_version("2.2.5").unwrap(), [2, 2, 5]);
}
