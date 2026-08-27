//! Session cost accounting — port of `RealtimeTokenAccumulator`, with local
//! price tables instead of backend-supplied pricing (the macOS app got these
//! numbers from its Supabase grant; here they're compiled-in defaults).

use serde_json::Value;

use crate::config::ProviderKind;

/// USD per 1M tokens.
#[derive(Debug, Clone, Copy)]
pub struct TokenPrices {
    pub audio_in_per_m: f64,
    pub audio_cached_in_per_m: f64,
    pub audio_out_per_m: f64,
    pub text_in_per_m: f64,
    pub text_cached_in_per_m: f64,
    pub text_out_per_m: f64,
    pub image_in_per_m: f64,
    pub image_cached_in_per_m: f64,
}

impl TokenPrices {
    pub fn for_model(kind: ProviderKind, model: &str) -> Self {
        match kind {
            // Prices from the OpenAI model pages. Unknown OpenAI model ids use
            // the full 2.1 table so the estimate errs conservatively.
            ProviderKind::OpenAi if model == "gpt-realtime-2.1-mini" => Self {
                audio_in_per_m: 10.0,
                audio_cached_in_per_m: 0.30,
                audio_out_per_m: 20.0,
                text_in_per_m: 0.60,
                text_cached_in_per_m: 0.06,
                text_out_per_m: 2.40,
                image_in_per_m: 0.80,
                image_cached_in_per_m: 0.08,
            },
            ProviderKind::OpenAi => Self {
                audio_in_per_m: 32.0,
                audio_cached_in_per_m: 0.40,
                audio_out_per_m: 64.0,
                text_in_per_m: 4.0,
                text_cached_in_per_m: 0.40,
                text_out_per_m: 24.0,
                image_in_per_m: 5.0,
                image_cached_in_per_m: 0.50,
            },
            // No published realtime pricing — report 0 rather than guess.
            ProviderKind::Grok | ProviderKind::Gemini => Self {
                audio_in_per_m: 0.0,
                audio_cached_in_per_m: 0.0,
                audio_out_per_m: 0.0,
                text_in_per_m: 0.0,
                text_cached_in_per_m: 0.0,
                text_out_per_m: 0.0,
                image_in_per_m: 0.0,
                image_cached_in_per_m: 0.0,
            },
        }
    }

    /// USD for one `response.done` usage block. Same math as the Swift
    /// accumulator: (tokens × per-1M price) / 1_000_000 per bucket.
    pub fn usd_for_usage(&self, usage: &Value) -> f64 {
        let g = |path: &str| usage.pointer(path).and_then(|v| v.as_u64()).unwrap_or(0) as f64;
        let bucket = |name: &str, regular: f64, cached: f64| {
            let total = g(&format!("/input_token_details/{name}_tokens"));
            let cached_tokens = g(&format!(
                "/input_token_details/cached_tokens_details/{name}_tokens"
            ))
            .min(total);
            (total - cached_tokens) * regular + cached_tokens * cached
        };
        let audio = bucket(
            "audio",
            self.audio_in_per_m,
            self.audio_cached_in_per_m,
        ) + g("/output_token_details/audio_tokens") * self.audio_out_per_m;
        let text = bucket(
            "text",
            self.text_in_per_m,
            self.text_cached_in_per_m,
        ) + g("/output_token_details/text_tokens") * self.text_out_per_m;
        let image = bucket(
            "image",
            self.image_in_per_m,
            self.image_cached_in_per_m,
        );
        (audio + text + image) / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_math_matches_accumulator() {
        let prices = TokenPrices::for_model(ProviderKind::OpenAi, "gpt-realtime-2.1");
        let usage = json!({
            "input_token_details": { "text_tokens": 1000, "audio_tokens": 500 },
            "output_token_details": { "text_tokens": 200, "audio_tokens": 300 },
        });
        // (500*32 + 300*64) audio + (1000*4 + 200*24) text = 35200 + 8800
        let usd = prices.usd_for_usage(&usage);
        assert!((usd - 0.044).abs() < 1e-9);
    }

    #[test]
    fn cached_tokens_use_the_discounted_rate() {
        let prices = TokenPrices::for_model(ProviderKind::OpenAi, "gpt-realtime-2.1-mini");
        let usage = json!({
            "input_token_details": {
                "text_tokens": 1000,
                "audio_tokens": 1000,
                "image_tokens": 1000,
                "cached_tokens_details": {
                    "text_tokens": 800,
                    "audio_tokens": 800,
                    "image_tokens": 800
                }
            },
            "output_token_details": { "text_tokens": 0, "audio_tokens": 0 },
        });
        // Uncached: 200*(.6 + 10 + .8), cached: 800*(.06 + .3 + .08).
        assert!((prices.usd_for_usage(&usage) - 0.002632).abs() < 1e-12);
    }

    #[test]
    fn missing_fields_cost_nothing() {
        let prices = TokenPrices::for_model(ProviderKind::OpenAi, "gpt-realtime-2.1-mini");
        assert_eq!(prices.usd_for_usage(&json!({})), 0.0);
    }
}
