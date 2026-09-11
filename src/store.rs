use std::collections::HashMap;
use std::path::Path;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Where a logged post came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Origin {
    /// A regular post in the tracked channel.
    #[default]
    Channel,
    /// Something forwarded from elsewhere (original date is preserved).
    Forwarded,
    /// Content sent directly to the bot in a private chat.
    Dm,
}

/// A photo reference as stored in the JSON log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordPhoto {
    pub file_id: String,
    pub unique_id: String,
}

/// AI breakdown of a single meal post.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MealItem {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub kcal: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kcal_low: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kcal_high: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MealAnalysis {
    pub summary: String,
    pub items: Vec<MealItem>,
    pub total_kcal: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kcal_low: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kcal_high: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protein_g: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fat_g: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carbs_g: Option<f64>,
    pub confidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caveats: Option<String>,
}

/// One logged meal post.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MealRecord {
    pub chat_id: i64,
    pub message_id: i64,
    /// Unix timestamp of the original posting time (forward date for forwards).
    pub posted_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub photos: Vec<RecordPhoto>,
    pub photo_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_group: Option<String>,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis: Option<MealAnalysis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyzed_at: Option<i64>,
    /// Message id of the bot's breakdown post in the channel (for edits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analysis_post_id: Option<i64>,
    /// For albums: key of the record this member was merged into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<String>,
    /// DM chat + message to reply to when re-delivering analysis (backfill forwards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<(i64, i64)>,
    /// When the record was created (unix ts); used by the retry janitor.
    #[serde(default)]
    pub created_at: i64,
    pub attempts: u32,
}

impl MealRecord {
    pub fn key(&self) -> String {
        key_of(self.chat_id, self.message_id)
    }
}

pub fn key_of(chat_id: i64, message_id: i64) -> String {
    format!("{chat_id}:{message_id}")
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    records: HashMap<String, MealRecord>,
    /// Private chat id of the first user who messaged the bot (owner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_chat_id: Option<i64>,
    /// Local date for which the last daily report was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_daily_covered: Option<String>,
    /// Local date (a Monday) for which the last weekly report was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_weekly_covered: Option<String>,
    /// Local date (a 1st of month) for which the last monthly report was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_monthly_covered: Option<String>,
    /// Stable session id for AI providers that want one (e.g. opencode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ai_session_id: Option<String>,
}

/// JSON-file backed store: `data/log.json`, atomically rewritten on change.
pub struct Store {
    path: Box<Path>,
    data: Persisted,
}

impl Store {
    pub fn load(path: &Path) -> Store {
        let data = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Store {
            path: path.into(),
            data,
        }
    }

    pub fn save(&self) {
        let tmp = self.path.with_extension("json.tmp");
        match serde_json::to_string_pretty(&self.data) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&tmp, s) {
                    log::error!("store write failed: {e}");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, &*self.path) {
                    log::error!("store rename failed: {e}");
                }
            }
            Err(e) => log::error!("store serialize failed: {e}"),
        }
    }

    pub fn upsert(&mut self, record: MealRecord) {
        self.data.records.insert(record.key(), record);
    }

    pub fn get(&self, key: &str) -> Option<&MealRecord> {
        self.data.records.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut MealRecord> {
        self.data.records.get_mut(key)
    }

    pub fn remove(&mut self, key: &str) {
        self.data.records.remove(key);
    }

    pub fn records_snapshot(&self) -> Vec<MealRecord> {
        self.data.records.values().cloned().collect()
    }

    pub fn owner_chat_id(&self) -> Option<i64> {
        self.data.owner_chat_id
    }

    pub fn set_owner_chat_id(&mut self, id: i64) {
        self.data.owner_chat_id = Some(id);
    }

    fn parse_date(s: Option<&String>) -> Option<NaiveDate> {
        s.and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok())
    }

    pub fn last_daily_covered(&self) -> Option<NaiveDate> {
        Self::parse_date(self.data.last_daily_covered.as_ref())
    }

    pub fn set_last_daily_covered(&mut self, d: NaiveDate) {
        self.data.last_daily_covered = Some(d.format("%Y-%m-%d").to_string());
    }

    pub fn last_weekly_covered(&self) -> Option<NaiveDate> {
        Self::parse_date(self.data.last_weekly_covered.as_ref())
    }

    pub fn set_last_weekly_covered(&mut self, d: NaiveDate) {
        self.data.last_weekly_covered = Some(d.format("%Y-%m-%d").to_string());
    }

    pub fn last_monthly_covered(&self) -> Option<NaiveDate> {
        Self::parse_date(self.data.last_monthly_covered.as_ref())
    }

    pub fn set_last_monthly_covered(&mut self, d: NaiveDate) {
        self.data.last_monthly_covered = Some(d.format("%Y-%m-%d").to_string());
    }

    pub fn ai_session_id(&self) -> Option<&str> {
        self.data.ai_session_id.as_deref()
    }

    pub fn set_ai_session_id(&mut self, id: &str) {
        self.data.ai_session_id = Some(id.to_string());
    }
}
