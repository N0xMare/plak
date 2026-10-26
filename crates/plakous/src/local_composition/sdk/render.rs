use super::refs::Refs;
use plakous_agent::internal::{LocalCompositionContentItem as Item, LocalCompositionResponse};
use std::sync::{Arc, Mutex};

pub(super) fn observation(response: &mut LocalCompositionResponse, refs: &Arc<Mutex<Refs>>) {
    let LocalCompositionResponse::Result { items, error, .. } = response else {
        return;
    };
    let mut text = items
        .iter()
        .filter_map(|item| {
            if let Item::Text(text) = item {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(error) = error {
        text.push('\n');
        text.push_str(error);
    }
    let bound = plakous_compose::PythonHarnessSdkDescriptorV1::OUTPUT_BYTES;
    if text.len() <= bound {
        return;
    }
    let mut end = bound - 256;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let note = match refs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(text.clone())
    {
        Ok(id) => format!(
            "\n[{} output bytes omitted; full output: Ref('{id}')]",
            text.len() - end
        ),
        Err(reason) => format!("\n[{} output bytes omitted; {reason}]", text.len() - end),
    };
    *items = vec![Item::Text(format!("{}{note}", &text[..end]))];
    if error.is_some() {
        *error = Some("SDK cell failed; see bounded output and reference".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_output_keeps_a_reference_to_the_exact_full_stream() -> Result<(), String> {
        let refs = Arc::default();
        let full = "é".repeat(10_000);
        let mut response = LocalCompositionResponse::Result {
            cell_id: "1".into(),
            items: vec![Item::Text(full.clone())],
            error: None,
        };
        observation(&mut response, &refs);
        let LocalCompositionResponse::Result { items, error, .. } = response else {
            return Err("response variant changed".into());
        };
        assert!(error.is_none());
        let Item::Text(text) = &items[0] else {
            return Err("text output missing".into());
        };
        assert!(text.len() <= 16_384 && text.contains("output bytes omitted"));
        let (_, suffix) = text.split_once("Ref('").ok_or("missing ref")?;
        let (id, _) = suffix.split_once("')").ok_or("invalid ref")?;
        let guard = refs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            matches!(guard.get(id),Some(super::super::refs::Reference::Text(value)) if value==&full)
        );
        Ok(())
    }

    #[test]
    fn large_diagnostics_remain_a_failure_not_a_state_publishing_success() {
        let refs = Arc::default();
        let mut response = LocalCompositionResponse::Result {
            cell_id: "1".into(),
            items: Vec::new(),
            error: Some("x".repeat(20_000)),
        };
        observation(&mut response, &refs);
        assert!(
            matches!(response,LocalCompositionResponse::Result{error:Some(_),ref items,..} if matches!(&items[0],Item::Text(text) if text.contains("Ref('")))
        );
    }
}
