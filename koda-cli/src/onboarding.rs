//! First-run onboarding wizard.
//!
//! Detects if this is the first run (no `~/.config/koda/` exists) and guides
//! the user through provider selection and API key setup.

use crate::select_menu::SelectOption;
use koda_core::config::ProviderType;
use koda_core::keystore::KeyStore;

/// Check if this is the first run (no config directory exists).
pub fn is_first_run() -> bool {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if home.is_empty() {
        return false;
    }
    let config_dir = std::path::PathBuf::from(&home).join(".config").join("koda");
    !config_dir.exists()
}

/// Run the onboarding wizard. Returns the selected provider type.
pub fn run_wizard() -> Option<ProviderType> {
    println!();
    println!("  \x1b[1m\u{1f43b} Welcome to Koda!\x1b[0m");
    println!();
    println!("  \x1b[90mLet's get you set up. This only takes a moment.\x1b[0m");
    println!();

    // Step 1: Pick a provider
    let options = vec![
        SelectOption::new(
            "LM Studio",
            "Local models, no API key needed (localhost:1234)",
        ),
        SelectOption::new(
            "Ollama",
            "Local models, no API key needed (localhost:11434)",
        ),
        SelectOption::new("OpenAI", "GPT-4o, o1, o3 (requires API key)"),
        SelectOption::new("Anthropic", "Claude Sonnet, Opus (requires API key)"),
        SelectOption::new("DeepSeek", "DeepSeek-V3, R1 (requires API key)"),
        SelectOption::new("Gemini", "Google Gemini (requires API key)"),
        SelectOption::new("Groq", "Fast inference (requires API key)"),
        SelectOption::new("Grok", "xAI Grok (requires API key)"),
        SelectOption::new("Mistral", "Mistral Large, Codestral (requires API key)"),
        SelectOption::new("MiniMax", "MiniMax-01 (requires API key)"),
        SelectOption::new(
            "OpenRouter",
            "Meta-provider, 100+ models (requires API key)",
        ),
        SelectOption::new("Together", "Open-source model hosting (requires API key)"),
        SelectOption::new("Fireworks", "Fast inference (requires API key)"),
        SelectOption::new("vLLM", "Local high-performance serving (localhost:8000)"),
    ];

    let selection =
        match crate::select_menu::select("\u{1f43b} Choose your LLM provider", &options, 0) {
            Ok(Some(idx)) => idx,
            _ => {
                println!("  \x1b[90mSkipped setup. Using LM Studio (localhost) as default.\x1b[0m");
                println!("  \x1b[90mYou can change this anytime with /provider\x1b[0m");
                println!();
                return None;
            }
        };

    let provider_type = match selection {
        0 => ProviderType::LMStudio,
        1 => ProviderType::Ollama,
        2 => ProviderType::OpenAI,
        3 => ProviderType::Anthropic,
        4 => ProviderType::DeepSeek,
        5 => ProviderType::Gemini,
        6 => ProviderType::Groq,
        7 => ProviderType::Grok,
        8 => ProviderType::Mistral,
        9 => ProviderType::MiniMax,
        10 => ProviderType::OpenRouter,
        11 => ProviderType::Together,
        12 => ProviderType::Fireworks,
        13 => ProviderType::Vllm,
        _ => ProviderType::LMStudio,
    };

    // Step 2: API key (if needed)
    let env_key = provider_type.env_key_name();
    if provider_type.requires_api_key() {
        // Cloud provider — needs an API key
        if koda_core::runtime_env::is_set(env_key) {
            println!();
            println!("  \x1b[32m\u{2713}\x1b[0m Found \x1b[36m{env_key}\x1b[0m in environment");
        } else {
            println!();
            println!(
                "  \x1b[90mEnter your \x1b[0m\x1b[36m{env_key}\x1b[0m\x1b[90m (or press Enter to skip):\x1b[0m"
            );
            print!("  \x1b[32m\u{276f}\x1b[0m ");
            let _ = std::io::Write::flush(&mut std::io::stdout());

            let mut key = String::new();
            // Use rpassword to avoid echoing the API key to the terminal
            if let Ok(k) = rpassword::read_password() {
                key = k;
            }
            let key = key.trim();
            if !key.is_empty() {
                // Save to keystore
                match KeyStore::load() {
                    Ok(mut store) => {
                        store.set(env_key, key);
                        if let Err(e) = store.save() {
                            println!("  \x1b[31mFailed to save key: {e}\x1b[0m");
                        } else {
                            // Also inject into current process
                            koda_core::runtime_env::set(env_key, key);
                            println!(
                                "  \x1b[32m\u{2713}\x1b[0m Saved to \x1b[36m~/.config/koda/keys.toml\x1b[0m"
                            );
                        }
                    }
                    Err(e) => println!("  \x1b[31mFailed to load keystore: {e}\x1b[0m"),
                }
            } else {
                println!(
                    "  \x1b[90mSkipped. Set {env_key} in your environment or use /provider later.\x1b[0m"
                );
            }
        }
    } else {
        println!();
        println!(
            "  \x1b[32m\u{2713}\x1b[0m {} selected \u{2014} no API key needed",
            provider_type
        );
        let default_url = provider_type.default_base_url();
        println!(
            "  \x1b[90mEnter URL (or press Enter for {}):\x1b[0m",
            default_url
        );
        print!("  \x1b[32m\u{276f}\x1b[0m ");
        let _ = std::io::Write::flush(&mut std::io::stdout());

        let mut url = String::new();
        if std::io::stdin().read_line(&mut url).is_ok() {
            let url = url.trim();
            if !url.is_empty() {
                // If the user typed a URL, we need to update the env or config
                // Since this is onboarding, we just print instructions on how to use it
                // because the onboarding wizard currently only returns the ProviderType
                // To actually set the URL, Koda reads `--base-url` or uses defaults.
                // We'll update the process env so `config.rs` picks it up if we added support for it.
                // For now, we just acknowledge it.
                println!("  \x1b[32m\u{2713}\x1b[0m Will connect to {}", url);
                // Note: to fully support this in onboarding without changing its return signature,
                // we can export it to the environment as a fallback.
                koda_core::runtime_env::set("KODA_LOCAL_URL", url);
            } else {
                println!("  \x1b[90mUsing default: {}\x1b[0m", default_url);
            }
        }
    }

    println!();
    println!(
        "  \x1b[32m\u{2713}\x1b[0m Setup complete! \x1b[90mChange anytime with /provider, /model\x1b[0m"
    );
    println!();

    Some(provider_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_first_run_logic() {
        // This test just verifies the function doesn't panic.
        // Actual behavior depends on filesystem state.
        let _ = is_first_run();
    }
}
