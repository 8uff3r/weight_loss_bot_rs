use serde_json::{Value, json};

use crate::store::{MealAnalysis, MealItem};
use crate::util::truncate;

/// Minimal OpenAI-compatible chat-completions client
/// (works with OpenAI, OpenRouter, Groq, Gemini's OpenAI endpoint, Ollama, LM Studio…).
pub struct Ai {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    user_agent: String,
    /// opencode (opencode.ai) requires a stable session id per conversation.
    is_opencode: bool,
    session_id: String,
    /// Optional forced output language (AI_LANGUAGE). When unset, breakdowns
    /// follow the language of each post and narratives stay English.
    language: Option<String>,
}

const MEAL_SYSTEM_PROMPT: &str = r#"You analyze posts from a personal food-diary channel. Each post contains a short text description and/or a photo of food. Identify every distinct food or drink, estimate its portion using common household or gram measures, and estimate calories. Combine information from the text and the image; if they conflict, mention it in caveats. Estimate what is actually visible or described — do not invent hidden side dishes, but note likely hidden calorie sources (oil, butter, sauce, dressing) in caveats when relevant.

Reply with ONLY a JSON object — no markdown fences, no extra text — in exactly this shape:
{"summary": "<one short sentence>", "items": [{"name": "<item>", "detail": "<portion note, e.g. 'about 250 g' or '1 cup'>", "kcal": 123, "kcal_low": 100, "kcal_high": 150}], "total_kcal": 123, "kcal_low": 100, "kcal_high": 150, "protein_g": 0, "fat_g": 0, "carbs_g": 0, "confidence": "low|medium|high", "caveats": "<short note or null>"}

Rules:
- every kcal value is a plain number without units;
- items must be non-empty when food is present;
- use null for unknown macro fields;
- totals must be realistic for the described/visible portions;
- if nothing edible is visible or described, return an empty items list, total_kcal 0, confidence "low", and explain in caveats.

Language: write all human-readable text — the summary, item names, portion details and caveats — in the same language the post is written in (a Persian post gets a Persian breakdown, an English post an English one, and so on). If the post has no text at all (photo only), use English. JSON keys and the confidence value always stay in English."#;

const NARRATOR_SYSTEM_PROMPT: &str = "You are a concise nutrition coach reviewing a personal food diary. Write 2-4 short sentences of practical observations and, if useful, one concrete suggestion. No headings, no bullet points, no markdown, no emojis. Be honest, specific and encouraging.";

impl Ai {
    pub fn new(
        api_key: String,
        base_url: String,
        model: String,
        session_id: String,
        language: Option<String>,
    ) -> Ai {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        let is_opencode = base_url.contains("opencode.ai");
        Ai {
            http,
            base_url,
            api_key,
            model,
            user_agent: user_agent(),
            is_opencode,
            session_id,
            language,
        }
    }

    /// Analyze one meal post: optional caption/text + optional image data-URLs.
    pub async fn analyze(
        &self,
        text: Option<&str>,
        images: &[String],
    ) -> Result<MealAnalysis, String> {
        if images.is_empty() && text.map(str::trim).unwrap_or("").is_empty() {
            return Err("nothing to analyze (no text, no usable image)".into());
        }
        let user_content = if images.is_empty() {
            json!(text.unwrap_or(""))
        } else {
            let mut parts = Vec::new();
            if let Some(t) = text.map(str::trim).filter(|t| !t.is_empty()) {
                parts.push(json!({ "type": "text", "text": t }));
            }
            for img in images {
                parts.push(json!({ "type": "image_url", "image_url": { "url": img } }));
            }
            json!(parts)
        };
        // Per-call system prompt: the base rules already say "match the post's
        // language"; AI_LANGUAGE (when set) overrides it.
        let system = match &self.language {
            Some(lang) => format!(
                "{MEAL_SYSTEM_PROMPT}\n\nLanguage override: regardless of the post's language, \
write ALL human-readable text (summary, item names, portion details, caveats) strictly in {lang}. \
JSON keys and the confidence value stay in English."
            ),
            None => MEAL_SYSTEM_PROMPT.to_string(),
        };
        let body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user_content }
            ],
            "temperature": 0.2,
            "max_tokens": 1000
        });
        let content = self.chat(body).await?;
        parse_meal(&content)
    }

    /// Short narrative commentary for aggregated report stats.
    pub async fn narrate(&self, stats: &str) -> Result<String, String> {
        let system = match &self.language {
            Some(lang) => format!("{NARRATOR_SYSTEM_PROMPT} Write your reply in {lang}."),
            None => NARRATOR_SYSTEM_PROMPT.to_string(),
        };
        let body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": stats }
            ],
            "temperature": 0.7,
            "max_tokens": 300
        });
        self.chat(body).await
    }

    async fn chat(&self, body: Value) -> Result<String, String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut req = self
            .http
            .post(&url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        if self.is_opencode {
            req = req.header("x-opencode-session", &self.session_id);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("AI request failed: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("AI response read failed: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "AI HTTP {}: {}",
                status.as_u16(),
                truncate(&text, 300)
            ));
        }
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("AI returned invalid JSON: {e}"))?;
        v.pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                format!(
                    "AI response has no message content: {}",
                    truncate(&text, 300)
                )
            })
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches("kcal").trim().parse().ok(),
        _ => None,
    }
}

/// A self-identifying User-Agent (some providers, like opencode, reject
/// generic SDK/HTTP-library default agents).
fn user_agent() -> String {
    format!("tg_wl_bot/{}", env!("CARGO_PKG_VERSION"))
}

/// Generate a stable-per-installation session id for providers that want one.
pub fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("tg-wl-bot-{nanos:x}")
}

/// Parse the AI's reply into a MealAnalysis, tolerating markdown fences and
/// loose numeric types.
fn parse_meal(content: &str) -> Result<MealAnalysis, String> {
    let start = content
        .find('{')
        .ok_or("AI reply contains no JSON object")?;
    let end = content.rfind('}').ok_or("AI reply JSON is unterminated")?;
    if end <= start {
        return Err("AI reply JSON is malformed".into());
    }
    let v: Value = serde_json::from_str(&content[start..=end]).map_err(|e| {
        format!(
            "AI JSON parse error: {e} — reply was: {}",
            truncate(content, 200)
        )
    })?;

    let mut items = Vec::new();
    if let Some(arr) = v.get("items").and_then(Value::as_array) {
        for (i, it) in arr.iter().enumerate() {
            let name = it
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Item {}", i + 1));
            items.push(MealItem {
                name,
                detail: it
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                kcal: num(it.get("kcal")).unwrap_or(0.0),
                kcal_low: num(it.get("kcal_low")),
                kcal_high: num(it.get("kcal_high")),
            });
        }
    }
    let sum_items: f64 = items.iter().map(|i| i.kcal).sum();
    let sum_lo: f64 = items.iter().map(|i| i.kcal_low.unwrap_or(i.kcal)).sum();
    let sum_hi: f64 = items.iter().map(|i| i.kcal_high.unwrap_or(i.kcal)).sum();
    let total = num(v.get("total_kcal")).unwrap_or(sum_items);
    let confidence = v
        .get("confidence")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_lowercase())
        .filter(|c| matches!(c.as_str(), "low" | "medium" | "high"))
        .unwrap_or_else(|| {
            if items.is_empty() {
                "low".into()
            } else {
                "medium".into()
            }
        });
    let caveats = v
        .get("caveats")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "null")
        .map(str::to_string);

    Ok(MealAnalysis {
        summary: v
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        items,
        total_kcal: total,
        kcal_low: num(v.get("kcal_low")).or_else(|| (sum_lo > 0.0).then_some(sum_lo)),
        kcal_high: num(v.get("kcal_high")).or_else(|| (sum_hi > 0.0).then_some(sum_hi)),
        protein_g: num(v.get("protein_g")),
        fat_g: num(v.get("fat_g")),
        carbs_g: num(v.get("carbs_g")),
        confidence,
        caveats,
    })
}
