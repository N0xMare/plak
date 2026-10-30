use plakous::openrouter::{
    OpenRouterApiKeySelection, OpenRouterKeyGeneration, OpenRouterKeySource,
};

fn forbidden(
    source: &OpenRouterKeySource,
    selection: &OpenRouterApiKeySelection,
    grant: &OpenRouterKeyGeneration,
) {
    let _ = source.api_key(selection, grant);
}

fn main() {}
