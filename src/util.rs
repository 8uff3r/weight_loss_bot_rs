use teloxide::prelude::*;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::types::{MessageId, ParseMode};

/// Escape text for Telegram HTML parse mode.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Cut a string to `max` chars (char-based, not bytes), appending an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Format a number with thousands separators, rounded to an integer.
pub fn fmt_num(n: f64) -> String {
    let r = n.round() as i64;
    let neg = r < 0;
    let digits = r.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if neg { format!("-{out}") } else { out }
}

/// Send an HTML message, splitting at newlines if it exceeds Telegram's limit.
pub async fn send_html(bot: &Bot, chat: ChatId, html: &str) {
    for chunk in split_html(html, 3800) {
        if let Err(e) = bot
            .send_message(chat, chunk)
            .parse_mode(ParseMode::Html)
            .await
        {
            log::error!("send_message failed: {e}");
        }
    }
}

/// Send an HTML message replying to `reply_to`.
pub async fn send_html_reply(bot: &Bot, chat: ChatId, reply_to: i64, html: &str) {
    for (i, chunk) in split_html(html, 3800).into_iter().enumerate() {
        let mut req = bot.send_message(chat, chunk).parse_mode(ParseMode::Html);
        if i == 0 {
            req = req.reply_to(MessageId(reply_to as i32));
        }
        if let Err(e) = req.await {
            log::error!("send_message failed: {e}");
        }
    }
}

/// Split HTML text into chunks below `max` chars, cutting at line boundaries.
/// Our own HTML tags never span lines, so this is tag-safe.
fn split_html(html: &str, max: usize) -> Vec<String> {
    if html.chars().count() <= max {
        return vec![html.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in html.split_inclusive('\n') {
        if line.chars().count() > max {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let mut buf = String::new();
            for ch in line.chars() {
                buf.push(ch);
                if buf.chars().count() >= max {
                    out.push(std::mem::take(&mut buf));
                }
            }
            cur = buf;
        } else if cur.chars().count() + line.chars().count() > max && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur.push_str(line);
        } else {
            cur.push_str(line);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
