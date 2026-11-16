//! What each model takes as input besides text: a table, as the context
//! windows and effort levels are, not a probe. Read v2 sends media only to
//! a model that takes it; anything else gets a short text placeholder
//! instead (see `koda_core`'s request assembly).
//!
//! The provider and the model both matter, Koda's two layers: Anthropic's
//! API takes images and PDFs for every current Claude model; Gemini's and
//! OpenAI's take images. A model Koda doesn't know, such as a local one,
//! is text only, the safe default: the config file's
//! `model_input_modalities` says otherwise for a local vision model. Codex
//! keeps the same on each model as `input_modalities`.

use crate::provider_catalog::ProviderType;
pub use koda_config::InputModality;

const TEXT: &[InputModality] = &[InputModality::Text];
const TEXT_IMAGE: &[InputModality] = &[InputModality::Text, InputModality::Image];
const TEXT_IMAGE_PDF: &[InputModality] = &[
    InputModality::Text,
    InputModality::Image,
    InputModality::Pdf,
];

/// What `model` on `provider` takes, by Koda's table.
///
/// | Provider | Model | Takes |
/// |---|---|---|
/// | Anthropic | any | text, images, PDFs |
/// | Gemini, OpenAI | any | text, images |
/// | any other | a Claude or GPT model (as through OpenRouter) | text, images |
/// | any other | anything else, local models included | text |
pub fn input_modalities(provider: ProviderType, model: &str) -> &'static [InputModality] {
    let model = model.to_ascii_lowercase();
    match provider {
        ProviderType::Anthropic => TEXT_IMAGE_PDF,
        ProviderType::Gemini | ProviderType::OpenAI => TEXT_IMAGE,
        _ if model.contains("claude") || model.contains("gpt-") => TEXT_IMAGE,
        _ => TEXT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_takes_images_and_pdfs_on_anthropic() {
        let takes = input_modalities(ProviderType::Anthropic, "claude-haiku-5-5");
        assert!(takes.contains(&InputModality::Image));
        assert!(takes.contains(&InputModality::Pdf));
    }

    #[test]
    fn a_known_model_through_another_provider_takes_images_not_pdfs() {
        let takes = input_modalities(ProviderType::OpenRouter, "anthropic/claude-sonnet-5-5");
        assert_eq!(takes, TEXT_IMAGE);
        assert_eq!(input_modalities(ProviderType::OpenAI, "gpt-6"), TEXT_IMAGE);
    }

    /// The safe default: a local model takes text until the config says
    /// otherwise.
    #[test]
    fn an_unknown_or_local_model_is_text_only() {
        assert_eq!(input_modalities(ProviderType::LMStudio, "qwen3-next"), TEXT);
        assert_eq!(input_modalities(ProviderType::LlamaCpp, "flashnext"), TEXT);
    }
}
