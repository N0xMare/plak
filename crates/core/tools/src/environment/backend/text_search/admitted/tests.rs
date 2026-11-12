//! Slice boundaries cannot hide malformed text or make binary output look textual.

use super::*;

#[test]
fn admitted_search_classifies_bytes_independently_of_slice_boundaries()
-> Result<(), Box<dyn std::error::Error>> {
    let control = OperationContext::new(plakous_model::CancellationToken::new());
    let cases: &[(&[u8], usize, u64, usize)] = &[
        (b"needle-before\nneedle-after", 2, 0, 0),
        (b"needle-before\ninvalid \xff\nneedle-after", 2, 1, 0),
        (b"needle-before\nneedle-\xff-after", 2, 1, 0),
        (b"needle-before\nneedle-\xe2\x82", 2, 1, 0),
        ("needle-before\nneedle-λ-after".as_bytes(), 2, 0, 0),
        (b"needle-before\nbinary\0line\nneedle-after", 0, 0, 1),
        (b"needle-before\nneedle-\0-after", 0, 0, 1),
        (b"needle-\xff\nbinary\0\nneedle-after", 0, 0, 1),
    ];
    for &(bytes, matches, lossy, binary) in cases {
        for regex in [false, true] {
            for split in 0..=bytes.len() {
                let AdmittedTextSearch::Scan(mut scan) = search_admitted_text(
                    &control,
                    AdmittedTextQuery {
                        path: Path::new("issued.log"),
                        pattern: Some("needle"),
                        regex,
                        match_case: RepositoryMatchCase::Sensitive,
                        glob: None,
                        limits: RepositoryQueryLimits::new(1, 1, 4096, 4096, 100, 4096, 16_384)?,
                    },
                )?
                else {
                    return Err("content search did not scan".into());
                };
                if scan.push(&control, &bytes[..split])? {
                    scan.push(&control, &bytes[split..])?;
                }
                let result = scan.finish(&control)?;
                assert_eq!(
                    (
                        result.matches().len(),
                        result.lossy_lines(),
                        result.skipped_binary()
                    ),
                    (matches, lossy, binary),
                    "{bytes:?}, regex={regex}, split={split}"
                );
                assert!(result.matches().iter().all(|hit| !hit.bytes().contains(&0)));
            }
        }
    }
    Ok(())
}
