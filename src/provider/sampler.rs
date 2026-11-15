//! Per-model sampler profiles.
//!
//! Sampling is a model property, not a global one. Qwen, Gemma, GLM, and
//! Devstral ship different recommendations, and DRY / min_p / XTC are the
//! standard fixes for the repetition loops that quantized models fall into.
//! A profile value overrides the global `[model]` value for the same field.
//!
//! Engines disagree about names and support. Only fields the selected engine
//! accepts are written into the request body; anything else is omitted and
//! reported by `lh doctor` instead of being sent as an unknown key.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::config::{ModelConfig, ModelProfile};

use super::engine::Engine;

/// One named sampler field modelled by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplerField {
    Temperature,
    TopP,
    MinP,
    RepeatPenalty,
    DryMultiplier,
    DryBase,
    XtcProbability,
    Seed,
}

impl SamplerField {
    pub fn name(self) -> &'static str {
        match self {
            SamplerField::Temperature => "temperature",
            SamplerField::TopP => "top_p",
            SamplerField::MinP => "min_p",
            SamplerField::RepeatPenalty => "repeat_penalty",
            SamplerField::DryMultiplier => "dry_multiplier",
            SamplerField::DryBase => "dry_base",
            SamplerField::XtcProbability => "xtc_probability",
            SamplerField::Seed => "seed",
        }
    }

    pub const ALL: [SamplerField; 8] = [
        SamplerField::Temperature,
        SamplerField::TopP,
        SamplerField::MinP,
        SamplerField::RepeatPenalty,
        SamplerField::DryMultiplier,
        SamplerField::DryBase,
        SamplerField::XtcProbability,
        SamplerField::Seed,
    ];
}

/// Resolved sampler values for one request. A `None` field was not set by the
/// profile or the global config and is never sent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SamplerSettings {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub min_p: Option<f32>,
    pub repeat_penalty: Option<f32>,
    pub dry_multiplier: Option<f32>,
    pub dry_base: Option<f32>,
    pub xtc_probability: Option<f32>,
    pub seed: Option<i64>,
}

impl SamplerSettings {
    /// Resolve global `[model]` values and overlay the active model profile.
    pub fn resolve(model: &ModelConfig, profile: &ModelProfile) -> Self {
        Self {
            temperature: profile.temperature.or(Some(model.temperature)),
            top_p: profile.top_p.or(Some(model.top_p)),
            min_p: profile.min_p.or(model.min_p),
            repeat_penalty: profile.repeat_penalty.or(model.repeat_penalty),
            dry_multiplier: profile.dry_multiplier.or(model.dry_multiplier),
            dry_base: profile.dry_base.or(model.dry_base),
            xtc_probability: profile.xtc_probability.or(model.xtc_probability),
            seed: profile.seed.or(model.seed),
        }
    }

    /// A request with only a temperature, for internal one-shot calls such as
    /// image descriptions.
    pub fn with_temperature(temperature: f32) -> Self {
        Self {
            temperature: Some(temperature),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        Self::ALL_FIELDS
            .iter()
            .all(|field| field_value(self, *field).is_none())
    }

    const ALL_FIELDS: [SamplerField; 8] = SamplerField::ALL;

    /// Fields set by this profile, in documented order.
    pub fn requested(&self) -> Vec<SamplerField> {
        Self::ALL_FIELDS
            .iter()
            .copied()
            .filter(|field| field_value(self, *field).is_some())
            .collect()
    }

    /// Fields the engine cannot accept, with the profile values left out of
    /// the body. Sorted and deduplicated for stable diagnostics.
    pub fn unsupported(&self, engine: Engine) -> Vec<SamplerField> {
        self.requested()
            .into_iter()
            .filter(|field| engine.sampler_key(*field).is_none())
            .collect()
    }

    /// Wire fields with the key names this engine accepts.
    pub fn body_fields(&self, engine: Engine) -> Map<String, Value> {
        let mut fields = Map::new();
        for field in self.requested() {
            let Some(key) = engine.sampler_key(field) else {
                continue;
            };
            fields.insert(
                key.to_string(),
                field_value(self, field).unwrap_or(Value::Null),
            );
        }
        fields
    }

    /// One warning per unsupported field, naming the action.
    pub fn warnings(&self, engine: Engine) -> Vec<String> {
        self.unsupported(engine)
            .into_iter()
            .map(|field| {
                format!(
                    "sampler profile sets {}, but {} does not accept it on the OpenAI-compatible API; the field is omitted. Remove {} from the model profile or [model] config, or set [model] engine to the real server if this is a gateway.",
                    field.name(),
                    engine.label(),
                    field.name()
                )
            })
            .collect()
    }

    /// Merge `over` on top of `self`; used by presets and tests.
    pub fn overlay(&self, over: &SamplerSettings) -> SamplerSettings {
        let mut merged = self.clone();
        for field in Self::ALL_FIELDS {
            if let Some(value) = field_value(over, field) {
                set_field(&mut merged, field, value);
            }
        }
        merged
    }
}

fn field_value(settings: &SamplerSettings, field: SamplerField) -> Option<Value> {
    fn float(value: f32) -> Value {
        // f32 -> f64 conversion exposes binary noise (`0.2` becomes
        // `0.20000000298023224`). The shortest decimal that round-trips the
        // f32 is what the user typed and what engines expect.
        value
            .to_string()
            .parse::<f64>()
            .map(|value| json!(value))
            .unwrap_or_else(|_| json!(value))
    }
    match field {
        SamplerField::Temperature => settings.temperature.map(float),
        SamplerField::TopP => settings.top_p.map(float),
        SamplerField::MinP => settings.min_p.map(float),
        SamplerField::RepeatPenalty => settings.repeat_penalty.map(float),
        SamplerField::DryMultiplier => settings.dry_multiplier.map(float),
        SamplerField::DryBase => settings.dry_base.map(float),
        SamplerField::XtcProbability => settings.xtc_probability.map(float),
        SamplerField::Seed => settings.seed.map(|value| json!(value)),
    }
}

fn set_field(settings: &mut SamplerSettings, field: SamplerField, value: Value) {
    match field {
        SamplerField::Temperature => settings.temperature = value.as_f64().map(|v| v as f32),
        SamplerField::TopP => settings.top_p = value.as_f64().map(|v| v as f32),
        SamplerField::MinP => settings.min_p = value.as_f64().map(|v| v as f32),
        SamplerField::RepeatPenalty => {
            settings.repeat_penalty = value.as_f64().map(|v| v as f32);
        }
        SamplerField::DryMultiplier => settings.dry_multiplier = value.as_f64().map(|v| v as f32),
        SamplerField::DryBase => settings.dry_base = value.as_f64().map(|v| v as f32),
        SamplerField::XtcProbability => {
            settings.xtc_probability = value.as_f64().map(|v| v as f32);
        }
        SamplerField::Seed => settings.seed = value.as_i64(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelProfile, ToolMode};

    fn global() -> ModelConfig {
        ModelConfig {
            temperature: 0.2,
            top_p: 0.95,
            ..ModelConfig::default()
        }
    }

    fn preset() -> ModelProfile {
        ModelProfile {
            temperature: Some(0.33),
            top_p: Some(0.8),
            min_p: Some(0.05),
            repeat_penalty: Some(1.05),
            dry_multiplier: Some(0.8),
            dry_base: Some(1.75),
            xtc_probability: Some(0.0),
            seed: Some(7),
            tool_mode: ToolMode::Repair,
            ..ModelProfile::default()
        }
    }

    #[test]
    fn profile_values_override_global_values_field_by_field() {
        let settings = SamplerSettings::resolve(&global(), &preset());
        assert_eq!(settings.temperature, Some(0.33));
        assert_eq!(settings.top_p, Some(0.8));
        assert_eq!(settings.min_p, Some(0.05));
        assert_eq!(settings.seed, Some(7));

        let profile = ModelProfile {
            temperature: Some(0.4),
            ..ModelProfile::default()
        };
        let settings = SamplerSettings::resolve(&global(), &profile);
        assert_eq!(settings.temperature, Some(0.4));
        assert_eq!(settings.top_p, Some(0.95));
    }

    #[test]
    fn llama_cpp_and_vllm_receive_min_p_under_their_own_names() {
        let settings = SamplerSettings::resolve(&global(), &preset());
        let llama = settings.body_fields(Engine::LlamaCpp);
        assert_eq!(llama["min_p"], json!(0.05));
        assert_eq!(llama["repeat_penalty"], json!(1.05));
        assert_eq!(llama["dry_multiplier"], json!(0.8));

        let vllm = settings.body_fields(Engine::Vllm);
        assert_eq!(vllm["min_p"], json!(0.05));
        assert_eq!(vllm["repetition_penalty"], json!(1.05));
        assert!(!vllm.contains_key("dry_multiplier"));
        assert!(!vllm.contains_key("xtc_probability"));
    }

    #[test]
    fn an_engine_without_min_p_omits_it_and_warns() {
        let settings = SamplerSettings::resolve(&global(), &preset());
        let body = settings.body_fields(Engine::Ollama);
        assert!(!body.contains_key("min_p"));
        assert!(body.contains_key("repeat_penalty"));
        let unsupported = settings.unsupported(Engine::Ollama);
        assert!(unsupported.contains(&SamplerField::MinP));
        assert!(unsupported.contains(&SamplerField::DryMultiplier));
        let warnings = settings.warnings(Engine::Ollama);
        assert!(warnings.iter().any(|warning| warning.contains("min_p")));
        assert!(warnings.iter().any(|warning| warning.contains("ollama")));
    }

    #[test]
    fn generic_engines_only_get_the_safe_common_fields() {
        let settings = SamplerSettings::resolve(&global(), &preset());
        let body = settings.body_fields(Engine::Generic);
        assert_eq!(body.len(), 2);
        assert_eq!(body["temperature"], json!(0.33));
        assert_eq!(body["top_p"], json!(0.8));
        assert_eq!(settings.unsupported(Engine::Mlx).len(), 5);
    }

    #[test]
    fn overlay_is_used_by_named_presets() {
        let base = SamplerSettings {
            temperature: Some(0.2),
            ..SamplerSettings::default()
        };
        let merged = base.overlay(&SamplerSettings::resolve(&global(), &preset()));
        assert_eq!(merged.temperature, Some(0.33));
        assert_eq!(merged.seed, Some(7));
    }
}
