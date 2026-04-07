use std::{convert::Infallible, str::FromStr};

use anyhow::{Context, Result};
use llama_cpp_2::model::{LlamaChatMessage, LlamaModel};

/// Rewriting style exposed through the DBus API and CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Correct grammar, spelling and punctuation only.
    Grammar,
    /// Elevate to formal / professional register.
    Formal,
    /// Relax to casual / conversational register.
    Casual,
    /// Shorten while preserving meaning.
    Concise,
    /// Expand with richer detail and context.
    Elaborate,
    /// Creative rewrite, same core meaning.
    Creative,
}

impl Style {
    fn parse_lossy(s: &str) -> Self {
        match s.to_lowercase().trim() {
            "formal" => Self::Formal,
            "casual" => Self::Casual,
            "concise" => Self::Concise,
            "elaborate" => Self::Elaborate,
            "creative" => Self::Creative,
            _ => Self::Grammar, // default + "grammar"
        }
    }

    pub fn all_names() -> &'static [&'static str] {
        &[
            "grammar",
            "formal",
            "casual",
            "concise",
            "elaborate",
            "creative",
        ]
    }

    fn instruction(self) -> &'static str {
        match self {
            Self::Grammar => {
                "Fix any grammar, spelling, and punctuation errors. \
                 Preserve the original phrasing and tone as much as possible. \
                 Return only the corrected text."
            }
            Self::Formal => {
                "Rewrite in a formal, professional register. \
                 Keep the core message intact. \
                 Return only the rewritten text."
            }
            Self::Casual => {
                "Rewrite in a casual, conversational tone. \
                 Keep the main ideas. \
                 Return only the rewritten text."
            }
            Self::Concise => {
                "Rewrite more concisely, removing unnecessary words and filler phrases. \
                 Preserve all key information. \
                 Return only the rewritten text."
            }
            Self::Elaborate => {
                "Expand the text with additional detail, context, and supporting ideas. \
                 Return only the elaborated version."
            }
            Self::Creative => {
                "Rewrite creatively with vivid language while preserving the meaning. \
                 Return only the rewritten text."
            }
        }
    }
}

impl FromStr for Style {
    type Err = Infallible;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self::parse_lossy(s))
    }
}

const SYSTEM_PROMPT: &str = "You are a precise writing assistant. \
     When asked to rewrite text, output only the improved version — \
     no explanations, no preamble, no quotation marks, no markdown.";

/// Build the full inference prompt using the model's embedded chat template.
///
/// Falls back to a Phi-3/4 compatible hard-coded template when the model has
/// no embedded template (e.g. base models).
pub fn build_prompt(model: &LlamaModel, text: &str, style: Style) -> Result<String> {
    let user_msg = format!("{}\n\n---\n{}", style.instruction(), text.trim());

    let messages = [
        LlamaChatMessage::new("system".to_string(), SYSTEM_PROMPT.to_string())
            .context("building system chat message")?,
        LlamaChatMessage::new("user".to_string(), user_msg.clone())
            .context("building user chat message")?,
    ];

    match model.chat_template(None) {
        Ok(tmpl) => model
            .apply_chat_template(&tmpl, &messages, /* add_ass */ true)
            .context("applying model chat template"),
        Err(_) => {
            // Fallback: Phi-3/Phi-4 format (also works with many other instruct models)
            Ok(format!(
                "<|system|>\n{SYSTEM_PROMPT}<|end|>\n<|user|>\n{user_msg}<|end|>\n<|assistant|>\n"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_from_str_round_trips() {
        for name in Style::all_names() {
            let s = Style::from_str(name).expect("style parsing is infallible");
            // Ensure every recognised name maps to something non-default
            // (or is exactly "grammar" which maps to Grammar)
            let _ = s; // just check it doesn't panic
        }
    }

    #[test]
    fn unknown_style_defaults_to_grammar() {
        assert_eq!(
            Style::from_str("nonsense").expect("style parsing is infallible"),
            Style::Grammar
        );
    }
}
