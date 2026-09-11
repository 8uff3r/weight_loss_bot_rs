use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub teloxide_token: String,
    pub ai_api_key: String,
    pub ai_base_url: String,
    pub ai_model: String,
    pub ai_comment_in_channel: bool,
    /// Append the breakdown to the user's post (edit-in-place) instead of
    /// posting a separate comment. Falls back to a comment when Telegram
    /// refuses the edit.
    pub ai_edit_posts: bool,
    pub ai_narrative: bool,
    /// Forced output language for AI texts (e.g. "fa"). Empty = follow each
    /// post's language for breakdowns, English for report narratives.
    pub ai_language: Option<String>,
    pub tracked_chat_id: Option<i64>,
    pub report_chat_id: Option<i64>,
    pub owner_id: Option<i64>,
    pub tz_offset_minutes: i32,
    pub daily_report_time: String,
    pub weekly_report_time: String,
    pub monthly_report_time: String,
    pub calorie_target: Option<u32>,
    pub data_dir: String,
}

fn var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn flag(key: &str, default: bool) -> bool {
    var(key)
        .map(|v| v != "false" && v != "0" && v != "no")
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let teloxide_token = var("TELOXIDE_TOKEN").ok_or("TELOXIDE_TOKEN is required")?;
        Ok(Config {
            teloxide_token,
            ai_api_key: var("AI_API_KEY").unwrap_or_default(),
            ai_base_url: var("AI_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".into()),
            ai_model: var("AI_MODEL").unwrap_or_else(|| "gpt-4o-mini".into()),
            ai_comment_in_channel: flag("AI_COMMENT_IN_CHANNEL", true),
            ai_edit_posts: flag("AI_EDIT_POSTS", true),
            ai_narrative: flag("AI_NARRATIVE", true),
            ai_language: var("AI_LANGUAGE"),
            tracked_chat_id: var("TRACKED_CHAT_ID").and_then(|v| v.parse().ok()),
            report_chat_id: var("REPORT_CHAT_ID").and_then(|v| v.parse().ok()),
            owner_id: var("OWNER_ID").and_then(|v| v.parse().ok()),
            tz_offset_minutes: var("TZ_OFFSET_MINUTES")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            daily_report_time: var("DAILY_REPORT_TIME").unwrap_or_else(|| "00:05".into()),
            weekly_report_time: var("WEEKLY_REPORT_TIME").unwrap_or_else(|| "00:05".into()),
            monthly_report_time: var("MONTHLY_REPORT_TIME").unwrap_or_else(|| "00:10".into()),
            calorie_target: var("CALORIE_TARGET").and_then(|v| v.parse().ok()),
            data_dir: var("DATA_DIR").unwrap_or_else(|| "data".into()),
        })
    }

    /// Local timezone as a fixed offset from UTC.
    pub fn tz(&self) -> chrono::FixedOffset {
        chrono::FixedOffset::east_opt(self.tz_offset_minutes * 60)
            .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).unwrap())
    }
}
