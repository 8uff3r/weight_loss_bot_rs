use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::Utc;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{FileId, MessageId, ParseMode, UserId};
use tokio::sync::Mutex;

use crate::ai::Ai;
use crate::config::Config;
use crate::store::{MealAnalysis, MealRecord, Origin, RecordPhoto, Store, key_of};
use crate::util::{escape_html, fmt_num, send_html_reply, truncate};

/// Shared application state.
pub struct App {
    pub cfg: Arc<Config>,
    pub ai: Arc<Ai>,
    pub store: Mutex<Store>,
    pub bot_user_id: UserId,
    pub media_dir: PathBuf,
    /// Media-group (album) buckets waiting to be processed, keyed by "{chat}:{group}".
    pub albums: Mutex<HashMap<String, Vec<PendingPost>>>,
}

/// How the analysis result should be delivered.
#[derive(Clone, Debug)]
pub enum Delivery {
    /// Append the breakdown to the user's own post by editing it in place.
    /// If Telegram refuses (no admin rights, post too old, caption too long),
    /// fall back to a separate comment in `fallback_chat`.
    ChannelEdit {
        chat_id: i64,
        message_id: i64,
        is_photo: bool,
        fallback_chat: i64,
    },
    /// Post (or edit) the breakdown as a separate channel post.
    ChannelComment {
        chat_id: i64,
        edit_post: Option<i64>,
    },
    /// Reply to the original message in a private chat.
    DmReply { chat_id: i64, message_id: i64 },
    /// Store the analysis without messaging anyone.
    Silent,
}

/// Seam inserted between the user's original text and our appended breakdown
/// when editing a post in place.
pub const BREAKDOWN_SEAM: &str = "\n\n----\n\n";
/// What we search for to detect (and strip) an already-appended breakdown.
/// The trailing 🔍 is part of the marker so that user text containing dashes
/// can never false-match.
const BREAKDOWN_SEAM_MARK: &str = "\n\n----\n\n🔍";

/// If our appended breakdown is present in `text`, return the user's original
/// text without it.
pub fn strip_breakdown(text: &str) -> Option<&str> {
    let idx = text.rfind(BREAKDOWN_SEAM_MARK)?;
    Some(&text[..idx])
}

/// In-memory photo reference.
#[derive(Clone, Debug)]
pub struct PhotoInput {
    pub file_id: String,
    pub unique_id: String,
}

impl PhotoInput {
    pub fn to_record(&self) -> RecordPhoto {
        RecordPhoto {
            file_id: self.file_id.clone(),
            unique_id: self.unique_id.clone(),
        }
    }

    pub fn from_record(p: &RecordPhoto) -> PhotoInput {
        PhotoInput {
            file_id: p.file_id.clone(),
            unique_id: p.unique_id.clone(),
        }
    }
}

/// One album member waiting for its siblings before a combined analysis.
#[derive(Clone, Debug)]
pub struct PendingPost {
    pub chat_id: i64,
    pub message_id: i64,
    pub text: Option<String>,
    pub photos: Vec<PhotoInput>,
    pub delivery: Delivery,
}

/// Analyze a single post and deliver the result.
pub async fn process_single(bot: Bot, app: Arc<App>, key: String, delivery: Delivery) {
    let (text, photos) = {
        let store = app.store.lock().await;
        match store.get(&key) {
            Some(r) => (r.text.clone(), r.photos.clone()),
            None => return,
        }
    };
    let photos: Vec<PhotoInput> = photos.iter().map(PhotoInput::from_record).collect();

    let images = load_images(&bot, &app, &photos).await;
    match app.ai.analyze(text.as_deref(), &images).await {
        Ok(analysis) => {
            if analysis.items.is_empty() && analysis.total_kcal <= 0.0 {
                // The AI found nothing edible — drop the record instead of
                // logging a zero-calorie entry.
                let mut store = app.store.lock().await;
                store.remove(&key);
                store.save();
                drop(store);
                if let Delivery::DmReply {
                    chat_id,
                    message_id,
                } = &delivery
                {
                    send_html_reply(
                        &bot,
                        ChatId(*chat_id),
                        *message_id,
                        "🤔 No food detected in that post — nothing logged.",
                    )
                    .await;
                }
                return;
            }
            {
                let mut store = app.store.lock().await;
                if let Some(r) = store.get_mut(&key) {
                    r.analysis = Some(analysis.clone());
                    r.error = None;
                    r.analyzed_at = Some(Utc::now().timestamp());
                    store.save();
                }
            }
            deliver_analysis(&bot, &app, &key, &analysis, &delivery).await;
        }
        Err(e) => {
            log::error!("analysis failed for {key}: {e}");
            {
                let mut store = app.store.lock().await;
                if let Some(r) = store.get_mut(&key) {
                    r.error = Some(truncate(&e, 300));
                    store.save();
                }
            }
            if let Delivery::DmReply {
                chat_id,
                message_id,
            } = &delivery
            {
                send_html_reply(
                    &bot,
                    ChatId(*chat_id),
                    *message_id,
                    &format!(
                        "⚠️ Couldn't analyze that post: {}",
                        escape_html(&truncate(&e, 200))
                    ),
                )
                .await;
            }
        }
    }
}

/// Analyze an album (media group) as one meal: all photos + captions in a single AI call.
/// The first member with a caption (or the first member) is the "head" record that
/// keeps the analysis; other members are marked as merged.
pub async fn process_album(bot: Bot, app: Arc<App>, batch: Vec<PendingPost>) {
    if batch.is_empty() {
        return;
    }
    let head = batch
        .iter()
        .find(|p| p.text.as_deref().is_some_and(|t| !t.trim().is_empty()))
        .unwrap_or(&batch[0])
        .clone();
    let head_key = key_of(head.chat_id, head.message_id);

    let mut combined = String::new();
    for (i, p) in batch.iter().enumerate() {
        if let Some(t) = p.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&format!("photo {}: {t}", i + 1));
        }
    }
    let combined_text = if combined.is_empty() {
        None
    } else {
        Some(combined)
    };

    let mut photos = Vec::new();
    for p in &batch {
        for ph in &p.photos {
            photos.push(ph.clone());
        }
    }

    let images = load_images(&bot, &app, &photos).await;
    match app.ai.analyze(combined_text.as_deref(), &images).await {
        Ok(analysis) => {
            if analysis.items.is_empty() && analysis.total_kcal <= 0.0 {
                let mut store = app.store.lock().await;
                for p in &batch {
                    store.remove(&key_of(p.chat_id, p.message_id));
                }
                store.save();
                drop(store);
                if let Delivery::DmReply {
                    chat_id,
                    message_id,
                } = &head.delivery
                {
                    send_html_reply(
                        &bot,
                        ChatId(*chat_id),
                        *message_id,
                        "🤔 No food detected in that album — nothing logged.",
                    )
                    .await;
                }
                return;
            }
            {
                let mut store = app.store.lock().await;
                for p in &batch {
                    let k = key_of(p.chat_id, p.message_id);
                    if k == head_key {
                        if let Some(r) = store.get_mut(&k) {
                            r.analysis = Some(analysis.clone());
                            r.error = None;
                            r.analyzed_at = Some(Utc::now().timestamp());
                        }
                    } else if let Some(r) = store.get_mut(&k) {
                        r.merged_into = Some(head_key.clone());
                        r.error = None;
                    }
                }
                store.save();
            }
            deliver_analysis(&bot, &app, &head_key, &analysis, &head.delivery).await;
        }
        Err(e) => {
            log::error!("album analysis failed ({head_key}): {e}");
            {
                let mut store = app.store.lock().await;
                for p in &batch {
                    if let Some(r) = store.get_mut(&key_of(p.chat_id, p.message_id)) {
                        r.error = Some(truncate(&e, 300));
                    }
                }
                store.save();
            }
            if let Delivery::DmReply {
                chat_id,
                message_id,
            } = &head.delivery
            {
                send_html_reply(
                    &bot,
                    ChatId(*chat_id),
                    *message_id,
                    &format!(
                        "⚠️ Couldn't analyze that album: {}",
                        escape_html(&truncate(&e, 200))
                    ),
                )
                .await;
            }
        }
    }
}

/// Periodic retry of posts whose analysis never succeeded (network hiccups,
/// bot restarts mid-album, provider outages). Max 3 attempts per record.
pub async fn janitor_once(bot: &Bot, app: &Arc<App>) {
    let cutoff = Utc::now().timestamp() - 300;
    let candidates: Vec<MealRecord> = {
        let store = app.store.lock().await;
        store
            .records_snapshot()
            .into_iter()
            .filter(|r| {
                r.analysis.is_none()
                    && r.merged_into.is_none()
                    && r.attempts < 3
                    && r.created_at < cutoff
            })
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    log::info!("janitor: retrying {} unanalyzed post(s)", candidates.len());

    let mut singles = Vec::new();
    let mut albums: HashMap<String, Vec<PendingPost>> = HashMap::new();
    {
        let mut store = app.store.lock().await;
        for r in candidates {
            let key = r.key();
            if let Some(rec) = store.get_mut(&key) {
                rec.attempts += 1;
            }
            let delivery = match &r.reply {
                Some((chat_id, message_id)) => Delivery::DmReply {
                    chat_id: *chat_id,
                    message_id: *message_id,
                },
                None => match r.origin {
                    Origin::Dm | Origin::Forwarded => Delivery::Silent,
                    Origin::Channel | Origin::ForwardedChannel => {
                        if app.cfg.ai_edit_posts {
                            Delivery::ChannelEdit {
                                chat_id: r.chat_id,
                                message_id: r.message_id,
                                is_photo: !r.photos.is_empty(),
                                fallback_chat: r.comment_chat.unwrap_or(r.chat_id),
                            }
                        } else {
                            Delivery::ChannelComment {
                                chat_id: r.comment_chat.unwrap_or(r.chat_id),
                                edit_post: r.analysis_post_id,
                            }
                        }
                    }
                },
            };
            let pending = PendingPost {
                chat_id: r.chat_id,
                message_id: r.message_id,
                text: r.text.clone(),
                photos: r.photos.iter().map(PhotoInput::from_record).collect(),
                delivery,
            };
            match &r.album_group {
                Some(g) => albums.entry(g.clone()).or_default().push(pending),
                None => singles.push(pending),
            }
        }
        store.save();
    }
    for p in singles {
        process_single(
            bot.clone(),
            app.clone(),
            key_of(p.chat_id, p.message_id),
            p.delivery.clone(),
        )
        .await;
    }
    for (_, batch) in albums {
        process_album(bot.clone(), app.clone(), batch).await;
    }
}

// ---------------------------------------------------------------------------
// Delivery helpers
// ---------------------------------------------------------------------------

async fn deliver_analysis(
    bot: &Bot,
    app: &App,
    key: &str,
    analysis: &MealAnalysis,
    delivery: &Delivery,
) {
    let bare = render_analysis(analysis);
    match delivery {
        Delivery::ChannelEdit {
            chat_id,
            message_id,
            is_photo,
            fallback_chat,
        } => {
            if !app.cfg.ai_comment_in_channel {
                return;
            }
            let chat = ChatId(*chat_id);
            let original = {
                let store = app.store.lock().await;
                store.get(key).and_then(|r| r.text.clone())
            };
            match edit_post_in_place(
                bot,
                chat,
                *message_id,
                *is_photo,
                original.as_deref(),
                &bare,
            )
            .await
            {
                Ok(()) => {
                    // If an earlier attempt fell back to a comment, clean it up.
                    let stale = {
                        let mut store = app.store.lock().await;
                        let stale = store.get(key).and_then(|r| r.analysis_post_id);
                        if stale.is_some() {
                            if let Some(r) = store.get_mut(key) {
                                r.analysis_post_id = None;
                            }
                            store.save();
                        }
                        stale
                    };
                    if let Some(pid) = stale {
                        let _ = bot.delete_message(chat, MessageId(pid as i32)).await;
                    }
                }
                Err(e) => {
                    log::warn!(
                        "editing post {message_id} in chat {chat_id} failed: {e} — falling back to a comment"
                    );
                    fallback_comment(bot, app, key, *fallback_chat, &bare).await;
                }
            }
        }
        Delivery::ChannelComment { chat_id, edit_post } => {
            if !app.cfg.ai_comment_in_channel {
                return;
            }
            let chat = ChatId(*chat_id);
            match edit_post {
                Some(pid) => {
                    if let Err(e) = bot
                        .edit_message_text(chat, MessageId(*pid as i32), bare.clone())
                        .parse_mode(ParseMode::Html)
                        .await
                    {
                        log::warn!("editing analysis post {pid} failed: {e} — posting fresh");
                        post_new(bot, app, key, chat, &bare).await;
                    }
                }
                None => post_new(bot, app, key, chat, &bare).await,
            }
        }
        Delivery::DmReply {
            chat_id,
            message_id,
        } => {
            send_html_reply(bot, ChatId(*chat_id), *message_id, &bare).await;
            // Backfilled from a channel? Also append the breakdown to the
            // original post, best effort.
            if app.cfg.ai_edit_posts {
                let rec = {
                    let store = app.store.lock().await;
                    store.get(key).cloned()
                };
                if let Some(r) = rec.filter(|r| r.origin == Origin::ForwardedChannel)
                    && let Err(e) = edit_post_in_place(
                        bot,
                        ChatId(r.chat_id),
                        r.message_id,
                        !r.photos.is_empty(),
                        r.text.as_deref(),
                        &bare,
                    )
                    .await
                {
                    log::debug!(
                        "couldn't append breakdown to original post {}:{}: {e}",
                        r.chat_id,
                        r.message_id
                    );
                }
            }
        }
        Delivery::Silent => {}
    }
}

/// Rewrite the user's post: original text + seam + breakdown. Photos use
/// `editMessageCaption`, plain text uses `editMessageText`.
async fn edit_post_in_place(
    bot: &Bot,
    chat: ChatId,
    message_id: i64,
    is_photo: bool,
    original_text: Option<&str>,
    breakdown_html: &str,
) -> Result<(), teloxide::RequestError> {
    let composed = match original_text.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!("{t}{BREAKDOWN_SEAM}{breakdown_html}"),
        None => breakdown_html.to_string(),
    };
    if is_photo {
        bot.edit_message_caption(chat, MessageId(message_id as i32))
            .caption(composed)
            .parse_mode(ParseMode::Html)
            .await?;
    } else {
        bot.edit_message_text(chat, MessageId(message_id as i32), composed)
            .parse_mode(ParseMode::Html)
            .await?;
    }
    Ok(())
}

/// Post (or update) the separate-comment fallback for a failed in-place edit.
async fn fallback_comment(bot: &Bot, app: &App, key: &str, chat_id: i64, bare: &str) {
    let chat = ChatId(chat_id);
    let existing = {
        let store = app.store.lock().await;
        store.get(key).and_then(|r| r.analysis_post_id)
    };
    match existing {
        Some(pid) => {
            if let Err(e) = bot
                .edit_message_text(chat, MessageId(pid as i32), bare.to_string())
                .parse_mode(ParseMode::Html)
                .await
            {
                log::warn!("editing fallback comment {pid} failed: {e} — posting fresh");
                post_new(bot, app, key, chat, bare).await;
            }
        }
        None => post_new(bot, app, key, chat, bare).await,
    }
}

async fn post_new(bot: &Bot, app: &App, key: &str, chat: ChatId, html: &str) {
    match bot
        .send_message(chat, html)
        .parse_mode(ParseMode::Html)
        .await
    {
        Ok(sent) => {
            let mut store = app.store.lock().await;
            if let Some(r) = store.get_mut(key) {
                r.analysis_post_id = Some(sent.id.0 as i64);
                store.save();
            }
        }
        Err(e) => log::error!("posting analysis to channel failed: {e}"),
    }
}

pub fn conf_badge(c: &str) -> &'static str {
    match c {
        "high" => "✅ high",
        "low" => "🟠 low",
        _ => "🟡 medium",
    }
}

/// The breakdown comment the bot posts under a meal.
pub fn render_analysis(a: &MealAnalysis) -> String {
    let mut h = String::from("🔍 <b>Meal breakdown</b>");
    h.push_str(&format!(" — est. <b>~{} kcal</b>", fmt_num(a.total_kcal)));
    if let (Some(l), Some(hi)) = (a.kcal_low, a.kcal_high)
        && hi > l
    {
        h.push_str(&format!(" ({}–{})", fmt_num(l), fmt_num(hi)));
    }
    h.push_str(&format!(
        "\n<i>confidence: {}</i>",
        conf_badge(&a.confidence)
    ));

    if !a.items.is_empty() {
        h.push('\n');
        let shown = a.items.len().min(12);
        for i in &a.items[..shown] {
            let det = i
                .detail
                .as_deref()
                .filter(|d| !d.trim().is_empty())
                .map(|d| format!(" — {}", escape_html(&truncate(d, 60))))
                .unwrap_or_default();
            let range = match (i.kcal_low, i.kcal_high) {
                (Some(l), Some(hi)) if hi > l => format!(" ({}–{})", fmt_num(l), fmt_num(hi)),
                _ => String::new(),
            };
            h.push_str(&format!(
                "\n• <b>{}</b>{}: ~{} kcal{}",
                escape_html(&truncate(&i.name, 60)),
                det,
                fmt_num(i.kcal),
                range
            ));
        }
        if a.items.len() > shown {
            h.push_str(&format!("\n• …and {} more", a.items.len() - shown));
        }
    }

    if !a.summary.is_empty() {
        h.push_str(&format!(
            "\n\n💬 <i>{}</i>",
            escape_html(&truncate(a.summary.trim(), 200))
        ));
    }
    if let Some(c) = a.caveats.as_deref().filter(|c| !c.trim().is_empty()) {
        h.push_str(&format!(
            "\n⚠️ <i>{}</i>",
            escape_html(&truncate(c.trim(), 200))
        ));
    }
    let mut macros = Vec::new();
    if let Some(p) = a.protein_g {
        macros.push(format!("P {} g", fmt_num(p)));
    }
    if let Some(f) = a.fat_g {
        macros.push(format!("F {} g", fmt_num(f)));
    }
    if let Some(c) = a.carbs_g {
        macros.push(format!("C {} g", fmt_num(c)));
    }
    if !macros.is_empty() {
        h.push_str(&format!("\n🧪 {}", macros.join(" · ")));
    }
    h
}

// ---------------------------------------------------------------------------
// Media helpers
// ---------------------------------------------------------------------------

/// Download photos (cached on disk by unique id) and return data-URLs for the AI.
async fn load_images(bot: &Bot, app: &App, photos: &[PhotoInput]) -> Vec<String> {
    let mut urls = Vec::new();
    for p in photos.iter().take(6) {
        let path = app.media_dir.join(format!("{}.jpg", p.unique_id));
        if !path.exists()
            && let Err(e) = download_photo(bot, &p.file_id, &path).await
        {
            log::warn!("photo {} download failed: {e}", p.unique_id);
            continue;
        }
        match tokio::fs::read(&path).await {
            Ok(bytes) if !bytes.is_empty() && bytes.len() <= 6_000_000 => {
                urls.push(format!("data:image/jpeg;base64,{}", B64.encode(&bytes)));
            }
            _ => log::warn!("photo {} unreadable or too large, skipped", p.unique_id),
        }
    }
    urls
}

async fn download_photo(bot: &Bot, file_id: &str, path: &PathBuf) -> Result<(), String> {
    let file = bot
        .get_file(FileId(file_id.to_string()))
        .await
        .map_err(|e| format!("get_file: {e}"))?;
    let mut dst = tokio::fs::File::create(path)
        .await
        .map_err(|e| format!("create file: {e}"))?;
    bot.download_file(&file.path, &mut dst)
        .await
        .map_err(|e| format!("download: {e}"))?;
    Ok(())
}
