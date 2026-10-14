//! Pure Responses-family request wire helpers.
//!
//! These builders operate on wire primitives and [`serde_json::Value`] only.
//! Dialects map canonical `plakous-model` types at their boundary; this module
//! never depends on agent or model crates.
//!
//! ## Shared vs dialect-local
//!
//! | Shared here | Stays in dialect plugins |
//! | --- | --- |
//! | Reasoning effort labels + `reasoning` object | Token-budget rejection, include lists |
//! | Function-only `tool_choice` wire | OpenAI custom tools; SuperGrok omit when empty |
//! | Function tool definition shape | OpenAI `strict`; OpenRouter `provider{}` |
//! | Instructions join, stream/store flags | `previous_response_id`, max_output_tokens rules |
//! | Message / function_call / output Value shapes | Actor→role maps, replay envelopes |

mod body;
mod items;
mod reasoning;
mod tool_choice;
mod tools;

pub use body::{insert_stream_store, join_instruction_texts};
pub use items::{
    function_call_item, function_call_output_item, input_text_part, message_item, output_text_part,
    refusal_part,
};
pub use reasoning::{
    REASONING_SUMMARY_AUTO, ReasoningConfigWire, ReasoningEffortWire, reasoning_object,
};
pub use tool_choice::FunctionToolChoiceWire;
pub use tools::FunctionToolWire;
