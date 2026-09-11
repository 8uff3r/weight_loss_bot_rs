use std::sync::Arc;

use chrono::{TimeZone, Utc};
use teloxide::prelude::*;
use teloxide::types::{MediaKind, MessageKind, MessageOrigin, UserId};

use crate::pipeline::{
    App, Delivery, PendingPost, PhotoInput, process_album, process_single, strip_breakdown,
};
use crate::reporting;
use crate::store::{MealRecord, Origin, key_of};
use crate::util::{escape_html, send_html};

const HELP: &str = "\
🥗 <b>Food-diary bot</b>

<b>Channel (automatic)</b>
• Add me to your food-log channel as an <b>admin</b> (needs <i>Edit messages</i> so I can append the breakdown to your posts).
• Post what you eat — text and/or a photo — and I'll append a breakdown (items, portions, calories) directly to your post, after a <code>----</code> seam.
• If editing isn't possible (missing right, post too old, caption too long), I post the breakdown as a separate comment instead.
• I re-analyze posts when you edit them and rewrite the appended breakdown.
• Reports arrive automatically: daily shortly after midnight, weekly on Monday morning, monthly on the 1st.

<b>Commands</b> — here in DM, or posted directly in the channel
/today — today so far
/report day|week|month — current period
/report day|week|month last — previous period
/rescan — how to import old posts
/status — what I know about your setup

<b>Quick logging</b>
Send me a photo or text of what you're eating right now — I'll analyze it and count it in the reports.

Telegram bots cannot read channel history, so to import old posts see /rescan.";

const BACKFILL_HELP: &str = "\
🔙 <b>Importing old posts</b>

Bots can't read channel history, so do this instead:
1. Open your channel, long-press an old post (or several) and <b>forward</b> it — into the channel or to me in DM.
2. I'll log it under its <b>original date/time</b> and append the breakdown to the original post (if I have the edit right there).
3. Posts that are already in the log are skipped automatically — forwarding twice is safe.

Tip: you can also just <i>edit</i> an old post in the channel — I treat edits as a signal to log and analyze it.";

// ---------------------------------------------------------------------------
// Public entry points used by the dispatcher
// ---------------------------------------------------------------------------

pub async fn channel_post(bot: Bot, app: Arc<App>, msg: Message) -> ResponseResult<()> {
    if is_self(&app, &msg) {
        return Ok(());
    }
    // Commands posted in the channel run there too (and are never logged).
    if let Some(tracked) = app.cfg.tracked_chat_id
        && msg.chat.id.0 != tracked
    {
        return Ok(());
    }
    if let Some(text) = msg.text().filter(|t| t.trim_start().starts_with('/')) {
        handle_command(&bot, &app, &msg, text).await;
        return Ok(());
    }
    ingest(bot, app, msg, false).await
}

pub async fn channel_post_edited(bot: Bot, app: Arc<App>, msg: Message) -> ResponseResult<()> {
    if is_self(&app, &msg) {
        return Ok(());
    }
    // Command posts are never logged; editing one is ignored.
    if msg.text().is_some_and(|t| t.trim_start().starts_with('/')) {
        return Ok(());
    }
    ingest(bot, app, msg, true).await
}

pub async fn private_message(bot: Bot, app: Arc<App>, msg: Message) -> ResponseResult<()> {
    if is_self(&app, &msg) {
        return Ok(());
    }

    // The first person to message the bot in a private chat becomes the owner
    // (unless OWNER_ID is configured).
    let chat = msg.chat.id.0;
    let from_id = msg.from.as_ref().map(|u| u.id);
    let registered_owner = {
        let mut store = app.store.lock().await;
        if app.cfg.owner_id.is_none()
            && store.owner_chat_id().is_none()
            && let Some(uid) = from_id
            && chat == uid.0 as i64
        {
            store.set_owner_chat_id(chat);
            store.save();
            log::info!("registered owner chat {chat}");
        }
        store.owner_chat_id()
    };

    let authorized = match app.cfg.owner_id {
        Some(owner) => chat == owner && from_id == Some(UserId(owner as u64)),
        None => registered_owner.is_none_or(|oc| chat == oc),
    };
    if !authorized {
        log::info!("ignoring unauthorized DM from chat {chat}");
        return Ok(());
    }

    if let Some(text) = msg.text()
        && text.trim_start().starts_with('/')
    {
        handle_command(&bot, &app, &msg, text).await;
        return Ok(());
    }

    // Any other content in DM = quick log / backfill forward.
    ingest(bot, app, msg, false).await
}

// ---------------------------------------------------------------------------
// Ingest: channel posts, edits, forwards and DM quick logs (shared)
// ---------------------------------------------------------------------------

async fn ingest(bot: Bot, app: Arc<App>, msg: Message, is_edit: bool) -> ResponseResult<()> {
    if is_self(&app, &msg) {
        return Ok(());
    }
    if let Some(tracked) = app.cfg.tracked_chat_id
        && msg.chat.id.0 != tracked
    {
        return Ok(());
    }

    let Some(ex) = extract(&msg) else {
        return Ok(());
    };
    if ex
        .text
        .as_deref()
        .is_some_and(|t| t.trim_start().starts_with("🔍"))
    {
        return Ok(()); // our own breakdown posts
    }

    // If our appended breakdown is already part of the text (an echo of our own
    // edit, the user editing an analyzed post, or a forward of one), strip it so
    // we always work with the user's original text only.
    let text = ex
        .text
        .as_deref()
        .map(|t| strip_breakdown(t).unwrap_or(t).to_string());

    let (chat_id, message_id, posted_at, origin) = identity(&msg);
    let key = key_of(chat_id, message_id);
    let dm = msg.chat.is_private();

    let existing = app.store.lock().await.get(&key).cloned();
    if let Some(rec) = existing {
        if is_edit {
            if rec.merged_into.is_some() {
                return Ok(()); // album member — head owns the analysis
            }
            if rec.text == text && rec.analysis.is_some() {
                return Ok(()); // nothing meaningful changed
            }
            let edit_post = {
                let mut store = app.store.lock().await;
                let mut edit_post = None;
                if let Some(r) = store.get_mut(&key) {
                    r.text = text.clone();
                    if r.photos.is_empty() && !ex.photos.is_empty() {
                        r.photos = ex.photos.iter().map(|p| p.to_record()).collect();
                        r.photo_count = ex.photos.len();
                    }
                    edit_post = r.analysis_post_id;
                    store.save();
                }
                edit_post
            };
            log::info!("post {key} edited — re-analyzing");
            let delivery = if app.cfg.ai_edit_posts {
                Delivery::ChannelEdit {
                    chat_id: msg.chat.id.0,
                    message_id: msg.id.0 as i64,
                    is_photo: !ex.photos.is_empty(),
                    fallback_chat: msg.chat.id.0,
                }
            } else {
                Delivery::ChannelComment {
                    chat_id: msg.chat.id.0,
                    edit_post,
                }
            };
            tokio::spawn(process_single(bot.clone(), app.clone(), key, delivery));
            return Ok(());
        }

        // Duplicate of something already logged (re-delivery or re-forward).
        if dm {
            let note = if rec.analysis.is_some() {
                "✅ Already in the log — nothing new to analyze."
            } else if rec.error.is_some() {
                "⏳ Already in the log — its analysis failed earlier; I'll retry automatically."
            } else {
                "⏳ Already in the log — still being analyzed."
            };
            send_html(&bot, msg.chat.id, note).await;
        }
        return Ok(());
    }

    // New record.
    {
        let mut store = app.store.lock().await;
        store.upsert(MealRecord {
            chat_id,
            message_id,
            posted_at,
            text: text.clone(),
            photos: ex.photos.iter().map(|p| p.to_record()).collect(),
            photo_count: ex.photos.len(),
            album_group: ex.album.clone(),
            origin,
            analysis: None,
            error: None,
            analyzed_at: None,
            analysis_post_id: None,
            merged_into: None,
            reply: dm.then_some((msg.chat.id.0, msg.id.0 as i64)),
            comment_chat: (!dm).then_some(msg.chat.id.0),
            created_at: Utc::now().timestamp(),
            attempts: 1,
        });
        store.save();
    }
    log::info!("logged new post {key}");

    let delivery = if dm {
        Delivery::DmReply {
            chat_id: msg.chat.id.0,
            message_id: msg.id.0 as i64,
        }
    } else if app.cfg.ai_edit_posts {
        Delivery::ChannelEdit {
            chat_id,
            message_id,
            is_photo: !ex.photos.is_empty(),
            fallback_chat: msg.chat.id.0,
        }
    } else {
        Delivery::ChannelComment {
            chat_id,
            edit_post: None,
        }
    };

    match ex.album.clone() {
        Some(group) => {
            // Albums arrive as several messages sharing a media group; buffer
            // briefly and analyze the whole group in one AI call.
            let bucket = format!("{chat_id}:{group}");
            {
                let mut map = app.albums.lock().await;
                map.entry(bucket.clone()).or_default().push(PendingPost {
                    chat_id,
                    message_id,
                    text: text.clone(),
                    photos: ex.photos.clone(),
                    delivery: delivery.clone(),
                });
            }
            let bot2 = bot.clone();
            let app2 = app.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                let batch = { app2.albums.lock().await.remove(&bucket).unwrap_or_default() };
                if !batch.is_empty() {
                    process_album(bot2, app2, batch).await;
                }
            });
        }
        None => {
            tokio::spawn(process_single(bot.clone(), app.clone(), key, delivery));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

async fn handle_command(bot: &Bot, app: &App, msg: &Message, text: &str) {
    let mut words = text.split_whitespace();
    let raw = words.next().unwrap_or("");
    let cmd = raw
        .trim_start_matches('/')
        .split('@')
        .next()
        .unwrap_or("")
        .to_lowercase();
    let args: Vec<String> = words.map(|w| w.to_lowercase()).collect();

    let today = Utc::now().with_timezone(&app.cfg.tz()).date_naive();

    match cmd.as_str() {
        "start" | "help" => send_html(bot, msg.chat.id, HELP).await,
        "today" => {
            let spec = reporting::daily(today);
            reporting::send_period_report(bot, app, msg.chat.id, &spec).await;
        }
        "report" => {
            let kind = args
                .iter()
                .find(|a| matches!(a.as_str(), "day" | "week" | "month"))
                .map(String::as_str)
                .unwrap_or("day");
            let last = args
                .iter()
                .any(|a| matches!(a.as_str(), "last" | "prev" | "previous" | "yesterday"));
            let spec = match (kind, last) {
                ("day", false) => reporting::daily(today),
                ("day", true) => match today.pred_opt() {
                    Some(y) => reporting::daily(y),
                    None => reporting::daily(today),
                },
                ("week", false) => reporting::weekly(reporting::monday_of(today)),
                ("week", true) => {
                    reporting::weekly(reporting::monday_of(today) - chrono::Duration::weeks(1))
                }
                ("month", false) => reporting::monthly(reporting::first_of_month(today)),
                ("month", true) => reporting::monthly(reporting::prev_month_first(today)),
                _ => {
                    send_html(
                        bot,
                        msg.chat.id,
                        "Usage: <code>/report day|week|month [last]</code>",
                    )
                    .await;
                    return;
                }
            };
            reporting::send_period_report(bot, app, msg.chat.id, &spec).await;
        }
        "rescan" => send_html(bot, msg.chat.id, BACKFILL_HELP).await,
        "status" => status_command(bot, app, msg).await,
        _ => send_html(bot, msg.chat.id, "Unknown command — try /help").await,
    }
}

async fn status_command(bot: &Bot, app: &App, msg: &Message) {
    let tz = app.cfg.tz();
    let (total, analyzed, failed, range) = {
        let store = app.store.lock().await;
        let recs = store.records_snapshot();
        let total = recs.len();
        let analyzed = recs.iter().filter(|r| r.analysis.is_some()).count();
        let failed = recs.iter().filter(|r| r.error.is_some()).count();
        let range = match (
            recs.iter().map(|r| r.posted_at).min(),
            recs.iter().map(|r| r.posted_at).max(),
        ) {
            (Some(a), Some(b)) => format!(
                "{} → {}",
                reporting_daily_date(a, tz),
                reporting_daily_date(b, tz)
            ),
            _ => "no records yet".into(),
        };
        (total, analyzed, failed, range)
    };
    let target = app
        .cfg
        .calorie_target
        .map(|t| format!("{} kcal", t))
        .unwrap_or_else(|| "not set".into());
    let updates_mode = if app.cfg.ai_edit_posts {
        "edit-in-place (fallback: separate comment)"
    } else {
        "separate comments"
    };
    let text = format!(
        "ℹ️ <b>Status</b>\n\
Records: {total} (analyzed {analyzed}, failed {failed})\n\
Range: {range}\n\
AI model: <code>{}</code> @ <code>{}</code>\n\
Channel updates: {updates_mode} · AI narrative: {}\n\
Reports: daily {} · weekly Mon {} · monthly 1st {}\n\
Calorie target: {target}\n\
TZ offset: {} min",
        escape_html(&app.cfg.ai_model),
        escape_html(&app.cfg.ai_base_url),
        app.cfg.ai_narrative,
        app.cfg.daily_report_time,
        app.cfg.weekly_report_time,
        app.cfg.monthly_report_time,
        app.cfg.tz_offset_minutes,
    );
    send_html(bot, msg.chat.id, &text).await;
}

fn reporting_daily_date(ts: i64, tz: chrono::FixedOffset) -> String {
    tz.timestamp_opt(ts, 0).unwrap().date_naive().to_string()
}

// ---------------------------------------------------------------------------
// Extraction & identity
// ---------------------------------------------------------------------------

struct Extracted {
    text: Option<String>,
    photos: Vec<PhotoInput>,
    album: Option<String>,
}

fn extract(msg: &Message) -> Option<Extracted> {
    let MessageKind::Common(common) = &msg.kind else {
        return None;
    };

    let photos = match &common.media_kind {
        MediaKind::Photo(mp) => mp
            .photo
            .iter()
            .max_by_key(|p| p.width as u64 * p.height as u64)
            .map(|p| PhotoInput {
                file_id: p.file.id.0.clone(),
                unique_id: p.file.unique_id.0.clone(),
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    };
    let text = match &common.media_kind {
        MediaKind::Text(_) => msg.text().map(str::to_string),
        _ => msg.caption().map(str::to_string),
    };
    let album = msg.media_group_id().map(|g| g.0.clone());

    if text.is_none() && photos.is_empty() {
        return None;
    }
    Some(Extracted {
        text,
        photos,
        album,
    })
}

/// Resolve the identity of a post: forwarding from a channel maps back to the
/// original chat + message id, so backfilled posts dedupe against live ones.
fn identity(msg: &Message) -> (i64, i64, i64, Origin) {
    match msg.forward_origin() {
        Some(MessageOrigin::Channel {
            chat,
            message_id,
            date,
            ..
        }) => (
            chat.id.0,
            message_id.0 as i64,
            date.timestamp(),
            Origin::ForwardedChannel,
        ),
        Some(o) => (
            msg.chat.id.0,
            msg.id.0 as i64,
            o.date().timestamp(),
            Origin::Forwarded,
        ),
        None => {
            let origin = if msg.chat.is_channel() {
                Origin::Channel
            } else {
                Origin::Dm
            };
            (msg.chat.id.0, msg.id.0 as i64, msg.date.timestamp(), origin)
        }
    }
}

fn is_self(app: &App, msg: &Message) -> bool {
    msg.from.as_ref().is_some_and(|u| u.id == app.bot_user_id)
}
