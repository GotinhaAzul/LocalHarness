//! One accumulator for tool calls, shared by every stream shape.
//!
//! Engines deliver tool calls very differently. SGLang streams argument
//! fragments by index; vLLM emits each call whole once it parses; Ollama's
//! compatibility route omits ids and indices; MLX applies stop strings to tool
//! calls, so a call can end mid-JSON; and some servers ignore `stream: true`
//! and answer with one JSON body. All of them are reduced to the same
//! [`StreamEvent`] sequence by the provider, and this module folds that
//! sequence into one normalized [`Completion`].
//!
//! Each call is a slot with an explicit state:
//!
//! * `Open` — fragments for its stream index are buffered. Arguments are never
//!   parsed while open; a prefix of valid JSON is not a call.
//! * `Closed` — the slot will take no more fragments. A slot closes when the
//!   stream ends, or when a *new whole call* arrives on the same stream index
//!   (a name plus argument text while the open arguments already form a
//!   complete JSON object, or a different engine id). Engines that reuse one
//!   index and one id for every call would otherwise splice two calls into
//!   one, and pair a single result with both.
//!
//! JSON is validated at close. When the output limit ended the stream
//! (`finish_reason = length`) and a call's arguments are incomplete, the whole
//! completion fails with [`ErrorClass::ToolCallTruncated`]; the call is never
//! returned, so it can never run with partial arguments.

use std::collections::HashMap;

use serde_json::Value;

use super::types::{ErrorClass, ProviderError, StreamEvent, StreamItem, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallState {
    Open,
    Closed,
}

#[derive(Debug, Clone)]
struct Slot {
    state: CallState,
    engine_id: String,
    name: String,
    arguments: String,
}

impl Slot {
    fn new() -> Self {
        Self {
            state: CallState::Open,
            engine_id: String::new(),
            name: String::new(),
            arguments: String::new(),
        }
    }

    /// The arguments already form one complete JSON object, so any further
    /// argument text cannot belong to this call.
    fn arguments_complete(&self) -> bool {
        !self.arguments.trim().is_empty()
            && matches!(
                serde_json::from_str::<Value>(&self.arguments),
                Ok(Value::Object(_))
            )
    }
}

/// A tool call after its slot closed. The engine id is kept only so the
/// caller can map it to an internal ledger id; it must never reach the
/// session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledCall {
    pub engine_id: Option<String>,
    pub name: String,
    /// Raw argument text exactly as the engine produced it.
    pub arguments: String,
}

impl AssembledCall {
    /// Parsed arguments. Empty text is `{}`; text that is not JSON is kept as
    /// a string for the repair layer, which can fix common model mistakes.
    pub fn parsed_arguments(&self) -> Value {
        if self.arguments.trim().is_empty() {
            return Value::Object(serde_json::Map::new());
        }
        serde_json::from_str(&self.arguments)
            .unwrap_or_else(|_| Value::String(self.arguments.clone()))
    }
}

/// Keyed by internal slot order; stream indices map to their open slot.
#[derive(Debug, Clone, Default)]
pub struct CallAccumulator {
    slots: Vec<Slot>,
    open: HashMap<usize, usize>,
}

impl CallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Buffer one fragment for stream index `index`.
    pub fn push(
        &mut self,
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) {
        let id = id.map(str::trim).filter(|id| !id.is_empty());
        let name = name.filter(|name| !name.trim().is_empty());
        let arguments = arguments.filter(|arguments| !arguments.is_empty());
        let slot = match self.open.get(&index).copied() {
            Some(current) if !self.starts_new_call(current, id, name, arguments) => current,
            previous => {
                if let Some(previous) = previous {
                    self.slots[previous].state = CallState::Closed;
                }
                self.slots.push(Slot::new());
                let slot = self.slots.len() - 1;
                self.open.insert(index, slot);
                slot
            }
        };
        let slot = &mut self.slots[slot];
        debug_assert_eq!(slot.state, CallState::Open);
        if let Some(id) = id {
            merge_fragment(&mut slot.engine_id, id);
        }
        if let Some(name) = name {
            merge_fragment(&mut slot.name, name);
        }
        if let Some(arguments) = arguments {
            slot.arguments.push_str(arguments);
        }
    }

    /// Whether a fragment on an index that already has an open slot is really
    /// the header of another call.
    fn starts_new_call(
        &self,
        current: usize,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> bool {
        let slot = &self.slots[current];
        if let Some(id) = id
            && !slot.engine_id.is_empty()
            && slot.engine_id != id
        {
            return true;
        }
        // A whole call on a reused index: the open call is already complete
        // and the fragment carries a header plus its own argument text.
        name.is_some()
            && arguments.is_some_and(|arguments| !arguments.trim().is_empty())
            && slot.arguments_complete()
    }

    /// Fail when the output limit cut a call before its JSON closed.
    pub fn check_truncation(&self, finish: Option<&str>) -> Result<(), ProviderError> {
        if !matches!(finish, Some("length") | Some("max_tokens")) {
            return Ok(());
        }
        for slot in &self.slots {
            if slot.name.trim().is_empty() {
                continue;
            }
            // A call cut off right after its name has no argument text at
            // all. Treating that as `{}` could run a no-required-keys tool
            // from a reply the output limit cut off.
            if slot.arguments.trim().is_empty()
                || serde_json::from_str::<Value>(&slot.arguments).is_err()
            {
                return Err(ProviderError::new(
                    ErrorClass::ToolCallTruncated,
                    format!(
                        "the model hit the output token limit while writing the {:?} tool call; its arguments are incomplete and nothing was executed. Raise [model] max_output_tokens or split the call into smaller steps.",
                        slot.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Close every slot and return the calls in arrival order. Slots that
    /// never received a function name are dropped.
    pub fn into_calls(self) -> Vec<AssembledCall> {
        self.slots
            .into_iter()
            .filter(|slot| !slot.name.trim().is_empty())
            .map(|slot| AssembledCall {
                engine_id: (!slot.engine_id.is_empty()).then_some(slot.engine_id),
                name: slot.name.trim().to_string(),
                arguments: slot.arguments,
            })
            .collect()
    }
}

/// Append a fragment unless it repeats what the buffer already ends with:
/// some engines resend the full name or id on every chunk.
pub fn merge_fragment(buffer: &mut String, fragment: &str) {
    if fragment.is_empty() {
        return;
    }
    if buffer.is_empty() || !buffer.ends_with(fragment) {
        buffer.push_str(fragment);
    }
}

/// The normalized result of one chat completion, whatever its wire shape.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Completion {
    pub content: String,
    pub reasoning: String,
    pub calls: Vec<AssembledCall>,
    /// Every usage report merged into one.
    pub usage: Option<Usage>,
    pub finish: Option<String>,
}

/// Fold a provider event sequence into a [`Completion`]. An error item aborts
/// with that error: an in-stream error must never become an empty answer.
pub fn fold_stream(
    items: impl IntoIterator<Item = StreamItem>,
) -> Result<Completion, ProviderError> {
    let mut completion = Completion::default();
    let mut calls = CallAccumulator::new();
    for item in items {
        match item? {
            StreamEvent::Delta(text) => completion.content.push_str(&text),
            StreamEvent::ReasoningDelta(text) => completion.reasoning.push_str(&text),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => calls.push(index, id.as_deref(), name.as_deref(), arguments.as_deref()),
            StreamEvent::Usage(usage) => match completion.usage.as_mut() {
                Some(merged) => merged.merge(&usage),
                None => completion.usage = Some(usage),
            },
            StreamEvent::Finish(reason) => completion.finish = Some(reason),
        }
    }
    calls.check_truncation(completion.finish.as_deref())?;
    completion.calls = calls.into_calls();
    Ok(completion)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(index: usize, id: Option<&str>, name: Option<&str>, args: &str) -> StreamItem {
        Ok(StreamEvent::ToolCallDelta {
            index,
            id: id.map(str::to_string),
            name: name.map(str::to_string),
            arguments: Some(args.to_string()),
        })
    }

    #[test]
    fn fragments_on_one_index_form_one_call() {
        let completion = fold_stream([
            delta(0, Some("x"), Some("read"), "{\"pa"),
            delta(0, None, None, "th\":"),
            delta(0, None, None, "\"a.rs\"}"),
            Ok(StreamEvent::Finish("tool_calls".into())),
        ])
        .unwrap();
        assert_eq!(completion.calls.len(), 1);
        assert_eq!(completion.calls[0].arguments, "{\"path\":\"a.rs\"}");
        assert_eq!(completion.calls[0].engine_id.as_deref(), Some("x"));
    }

    #[test]
    fn a_reused_index_and_id_does_not_merge_two_whole_calls() {
        let completion = fold_stream([
            delta(0, Some("call_1"), Some("read"), "{\"path\":\"a.rs\"}"),
            delta(0, Some("call_1"), Some("read"), "{\"path\":\"b.rs\"}"),
            Ok(StreamEvent::Finish("tool_calls".into())),
        ])
        .unwrap();
        assert_eq!(completion.calls.len(), 2, "{:?}", completion.calls);
        assert_eq!(completion.calls[0].arguments, "{\"path\":\"a.rs\"}");
        assert_eq!(completion.calls[1].arguments, "{\"path\":\"b.rs\"}");
    }

    #[test]
    fn a_repeated_name_on_each_fragment_continues_the_call() {
        let completion = fold_stream([
            delta(0, Some("x"), Some("read"), "{\"path\":"),
            delta(0, Some("x"), Some("read"), "\"a.rs\"}"),
            delta(0, Some("x"), Some("read"), ""),
        ])
        .unwrap();
        assert_eq!(completion.calls.len(), 1);
        assert_eq!(completion.calls[0].arguments, "{\"path\":\"a.rs\"}");
    }

    #[test]
    fn a_length_cut_call_is_truncated_and_not_returned() {
        let err = fold_stream([
            delta(0, None, Some("write"), "{\"path\":\"a"),
            Ok(StreamEvent::Finish("length".into())),
        ])
        .unwrap_err();
        assert_eq!(err.class, ErrorClass::ToolCallTruncated);
        assert_eq!(err.class.code(), "tool_call_truncated");
    }

    #[test]
    fn an_error_item_aborts_the_fold() {
        let err = fold_stream([
            Ok(StreamEvent::Delta("par".into())),
            Err(ProviderError::new(ErrorClass::Server, "boom")),
        ])
        .unwrap_err();
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn repeated_usage_merges_once() {
        let completion = fold_stream([
            Ok(StreamEvent::Usage(Usage::new(10, 1))),
            Ok(StreamEvent::Usage(Usage::new(10, 5))),
        ])
        .unwrap();
        assert_eq!(completion.usage, Some(Usage::new(10, 5)));
    }

    #[test]
    fn merge_fragment_handles_repeats() {
        let mut buffer = String::new();
        merge_fragment(&mut buffer, "read");
        merge_fragment(&mut buffer, "read");
        assert_eq!(buffer, "read");
        merge_fragment(&mut buffer, "_file");
        assert_eq!(buffer, "read_file");
    }
}
