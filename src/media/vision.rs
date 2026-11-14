//! What the harness may do with images for the active model.
//!
//! A model profile with `vision = true` (or a llama.cpp server whose
//! `/props` reports vision) receives images as OpenAI image content parts.
//! Otherwise an optional `[vision] model` describes each image once and the
//! description is attached as text, labelled with the describing model.
//! Images never go to a non-local endpoint unless `[vision] allow_remote`
//! (or `--allow-remote-images`) is set.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::{Config, ModelConfig};
use crate::provider::{
    ChatMessage, ChatRequest, ImageData, OpenAiCompatProvider, StreamEvent, collect_stream,
};

use super::image::LoadedImage;

/// How long one image description may take.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(180);

const DESCRIBE_PROMPT: &str = "Describe this image for a coding assistant that cannot see it. \
Transcribe all visible text exactly, including error messages, labels, and code. \
Describe the layout, UI elements and their positions, colors, and anything that looks \
misaligned, clipped, or broken. Be specific and concise; do not speculate beyond what is visible.";

/// Runtime image settings, resolved from config, the model profile, and the
/// endpoint.
#[derive(Debug, Clone)]
pub struct VisionSettings {
    /// The active model accepts image content parts.
    pub native: bool,
    /// Why images may not be sent to the active endpoint (non-local without
    /// opt-in). `None` means allowed.
    pub blocked: Option<String>,
    /// A separate model that describes images for a model without vision.
    pub describer: Option<Describer>,
    pub max_bytes: u64,
    pub keep_exif: bool,
}

impl Default for VisionSettings {
    fn default() -> Self {
        Self {
            native: false,
            blocked: None,
            describer: None,
            max_bytes: super::image::DEFAULT_MAX_BYTES,
            keep_exif: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Describer {
    pub config: ModelConfig,
    pub model: String,
    /// Why images may not be sent to the describer's endpoint.
    pub blocked: Option<String>,
}

/// How an image will reach the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Send the image itself.
    Native,
    /// Describe it with this model first.
    Describe(String),
    /// It cannot be used; the reason names the fix.
    Refused(String),
}

impl VisionSettings {
    /// Resolve settings for `config` and the active model profile.
    /// `detected` is the server's own vision report (llama.cpp `/props`),
    /// used when the profile does not say.
    pub fn resolve(config: &Config, profile_vision: Option<bool>, detected: Option<bool>) -> Self {
        let vision = &config.vision;
        let allow_remote = vision.allow_remote;
        let blocked = |endpoint: &str| {
            (!allow_remote && !endpoint_is_local(endpoint)).then(|| {
                format!(
                    "{endpoint} is not a local endpoint; images are only sent to loopback or private-network endpoints unless [vision] allow_remote = true (or --allow-remote-images)"
                )
            })
        };
        let describer = (!vision.model.trim().is_empty()).then(|| {
            let mut model_config = config.model.clone();
            if !vision.endpoint.trim().is_empty() {
                model_config.endpoint = vision.endpoint.trim().to_string();
                model_config.api_key = vision.api_key.clone();
            } else if !vision.api_key.is_empty() {
                model_config.api_key = vision.api_key.clone();
            }
            Describer {
                blocked: blocked(&model_config.endpoint),
                model: vision.model.trim().to_string(),
                config: model_config,
            }
        });
        Self {
            native: profile_vision.or(detected).unwrap_or(false),
            blocked: blocked(&config.model.endpoint),
            describer,
            max_bytes: if vision.max_bytes == 0 {
                super::image::DEFAULT_MAX_BYTES
            } else {
                vision.max_bytes
            },
            keep_exif: vision.keep_exif,
        }
    }

    /// Decide how an image reaches the model, before anything is stored.
    pub fn route(&self) -> Route {
        if self.native {
            return match &self.blocked {
                None => Route::Native,
                Some(reason) => Route::Refused(reason.clone()),
            };
        }
        match &self.describer {
            Some(describer) => match &describer.blocked {
                None => Route::Describe(describer.model.clone()),
                Some(reason) => Route::Refused(reason.clone()),
            },
            None => Route::Refused(
                "the current model has no vision input. Set `vision = true` in its [model_profile] if it accepts images, or set [vision] model to a local vision model that describes images"
                    .into(),
            ),
        }
    }
}

/// True for endpoints on this machine or a private network: loopback,
/// `localhost`, RFC 1918, link-local, and IPv6 unique-local addresses.
/// Anything else, including public hostnames, is remote.
pub fn endpoint_is_local(endpoint: &str) -> bool {
    let rest = endpoint
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(endpoint);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_private())
        }
        Err(_) => false,
    }
}

/// The image as a provider content part.
pub fn image_data(image: &LoadedImage, label: impl Into<String>) -> ImageData {
    ImageData {
        mime: image.format.mime().to_string(),
        width: image.width,
        height: image.height,
        data: Arc::new(image.bytes.clone()),
        label: label.into(),
    }
}

/// Ask the describer model for a text description of one image.
pub async fn describe(
    describer: &Describer,
    image: ImageData,
    cancel: CancellationToken,
) -> Result<String, String> {
    let provider = OpenAiCompatProvider::new(&describer.config, describer.model.clone())
        .map_err(|error| format!("cannot reach the vision model: {error}"))?;
    let request = ChatRequest {
        model: describer.model.clone(),
        messages: vec![ChatMessage::User {
            content: DESCRIBE_PROMPT.into(),
            images: vec![image],
        }],
        tools: Vec::new(),
        tool_choice: None,
        parallel_tool_calls: None,
        temperature: 0.1,
        top_p: 0.95,
        max_tokens: 1024,
        stop: Vec::new(),
        extra_body: None,
        include_usage: false,
        reasoning_field: None,
    };
    let events = tokio::time::timeout(DESCRIBE_TIMEOUT, collect_stream(&provider, request, cancel))
        .await
        .map_err(|_| {
            format!(
                "the vision model {} did not answer within {}s",
                describer.model,
                DESCRIBE_TIMEOUT.as_secs()
            )
        })?;
    let mut text = String::new();
    for event in events {
        match event {
            Ok(StreamEvent::Delta(delta)) => text.push_str(&delta),
            Ok(_) => {}
            Err(error) => return Err(format!("vision model {}: {error}", describer.model)),
        }
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(format!(
            "the vision model {} returned no description",
            describer.model
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locality_covers_loopback_and_private_networks_only() {
        for local in [
            "http://127.0.0.1:8080/v1",
            "http://localhost:11434",
            "https://[::1]:8000/v1",
            "http://10.0.0.5:8080",
            "http://192.168.1.20:1234/v1",
            "http://172.16.4.4",
            "http://[fd12::1]:8080",
            "http://user:pw@127.0.0.1:8080",
        ] {
            assert!(endpoint_is_local(local), "{local}");
        }
        for remote in [
            "https://api.example.com/v1",
            "http://8.8.8.8:8080",
            "http://172.32.0.1",
            "http://gpu-box:8080",
            "http://[2001:db8::1]/v1",
        ] {
            assert!(!endpoint_is_local(remote), "{remote}");
        }
    }

    #[test]
    fn routes_native_describe_or_refuse() {
        let mut config = Config::builtin();
        config.model.endpoint = "http://127.0.0.1:8080".into();
        assert!(matches!(
            VisionSettings::resolve(&config, None, None).route(),
            Route::Refused(reason) if reason.contains("no vision input")
        ));
        assert_eq!(
            VisionSettings::resolve(&config, Some(true), None).route(),
            Route::Native
        );
        // The profile wins over the server's report.
        assert!(!VisionSettings::resolve(&config, Some(false), Some(true)).native);
        assert!(VisionSettings::resolve(&config, None, Some(true)).native);

        config.vision.model = "vl-model".into();
        assert_eq!(
            VisionSettings::resolve(&config, None, None).route(),
            Route::Describe("vl-model".into())
        );

        // A remote endpoint needs an explicit opt-in.
        config.model.endpoint = "https://api.example.com/v1".into();
        assert!(matches!(
            VisionSettings::resolve(&config, Some(true), None).route(),
            Route::Refused(reason) if reason.contains("allow_remote")
        ));
        config.vision.allow_remote = true;
        assert_eq!(
            VisionSettings::resolve(&config, Some(true), None).route(),
            Route::Native
        );
    }
}
