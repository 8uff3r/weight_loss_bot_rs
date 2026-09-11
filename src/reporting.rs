use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::{
    Datelike, Duration as CDuration, FixedOffset, NaiveDate, NaiveTime, TimeZone, Utc, Weekday,
};
use teloxide::prelude::*;

use crate::config::Config;
use crate::pipeline::App;
use crate::store::MealRecord;
use crate::util::{escape_html, fmt_num, send_html};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PeriodKind {
    Daily,
    Weekly,
    Monthly,
}

/// A reportable period: inclusive range of local dates.
pub struct PeriodSpec {
    pub kind: PeriodKind,
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl PeriodSpec {
    pub fn title(&self) -> String {
        match self.kind {
            PeriodKind::Daily => {
                format!(
                    "📅 <b>Daily report — {}</b>",
                    self.start.format("%a, %d %b %Y")
                )
            }
            PeriodKind::Weekly => format!(
                "🗓️ <b>Weekly report — {} – {}</b>",
                self.start.format("%d %b"),
                self.end.format("%d %b %Y")
            ),
            PeriodKind::Monthly => {
                format!("📊 <b>Monthly report — {}</b>", self.start.format("%B %Y"))
            }
        }
    }

    /// Inclusive-start / exclusive-end unix bounds of the period in local time.
    pub fn bounds_tz(&self, tz: FixedOffset) -> (i64, i64) {
        let end_date = self.end.succ_opt().unwrap_or(self.end);
        (local_midnight(self.start, tz), local_midnight(end_date, tz))
    }
}

pub fn daily(d: NaiveDate) -> PeriodSpec {
    PeriodSpec {
        kind: PeriodKind::Daily,
        start: d,
        end: d,
    }
}

/// Week starting on `monday`.
pub fn weekly(monday: NaiveDate) -> PeriodSpec {
    PeriodSpec {
        kind: PeriodKind::Weekly,
        start: monday,
        end: monday + CDuration::days(6),
    }
}

pub fn monthly(first_of_month: NaiveDate) -> PeriodSpec {
    PeriodSpec {
        kind: PeriodKind::Monthly,
        start: first_of_month,
        end: last_day_of_month(first_of_month),
    }
}

// ---------------------------------------------------------------------------
// Date helpers
// ---------------------------------------------------------------------------

fn local_midnight(d: NaiveDate, tz: FixedOffset) -> i64 {
    tz.from_local_datetime(&d.and_time(NaiveTime::MIN))
        .earliest()
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

fn local_date(ts: i64, tz: FixedOffset) -> NaiveDate {
    tz.timestamp_opt(ts, 0).unwrap().date_naive()
}

fn local_time_str(ts: i64, tz: FixedOffset) -> String {
    tz.timestamp_opt(ts, 0).unwrap().format("%H:%M").to_string()
}

fn last_day_of_month(first: NaiveDate) -> NaiveDate {
    first
        .checked_add_months(chrono::Months::new(1))
        .and_then(|next| next.pred_opt())
        .unwrap_or(first)
}

pub fn monday_of(d: NaiveDate) -> NaiveDate {
    d - CDuration::days(d.weekday().num_days_from_monday() as i64)
}

pub fn first_of_month(d: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d)
}

pub fn prev_month_first(d: NaiveDate) -> NaiveDate {
    let (y, m) = (d.year(), d.month());
    let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
    NaiveDate::from_ymd_opt(py, pm, 1).unwrap_or(d)
}

fn parse_hm(s: &str) -> NaiveTime {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").unwrap_or(NaiveTime::MIN)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render a report for a period. Returns (html, plain-text stats for the AI narrator).
pub fn render_report(spec: &PeriodSpec, records: &[MealRecord], cfg: &Config) -> (String, String) {
    let tz = cfg.tz();
    let today = Utc::now().with_timezone(&tz).date_naive();

    struct DayAgg {
        posts: usize,
        kcal: f64,
    }
    let mut per_day: BTreeMap<NaiveDate, DayAgg> = BTreeMap::new();
    let mut items: HashMap<String, (usize, String)> = HashMap::new();
    let mut total = 0f64;
    let mut lo = 0f64;
    let mut hi = 0f64;
    let mut posts = 0usize;
    let mut analyzed = 0usize;
    let mut failed = 0usize;
    let mut waiting = 0usize;
    let mut protein = 0f64;
    let mut fat = 0f64;
    let mut carbs = 0f64;
    let mut macros_n = 0usize;

    let mut sorted: Vec<&MealRecord> = records.iter().collect();
    sorted.sort_by_key(|r| r.posted_at);

    for r in &sorted {
        posts += 1;
        let d = local_date(r.posted_at, tz);
        let day = per_day.entry(d).or_insert(DayAgg {
            posts: 0,
            kcal: 0.0,
        });
        day.posts += 1;
        match &r.analysis {
            Some(a) => {
                analyzed += 1;
                day.kcal += a.total_kcal;
                total += a.total_kcal;
                lo += a.kcal_low.unwrap_or(a.total_kcal);
                hi += a.kcal_high.unwrap_or(a.total_kcal);
                if a.protein_g.is_some() || a.fat_g.is_some() || a.carbs_g.is_some() {
                    macros_n += 1;
                    protein += a.protein_g.unwrap_or(0.0);
                    fat += a.fat_g.unwrap_or(0.0);
                    carbs += a.carbs_g.unwrap_or(0.0);
                }
                for i in &a.items {
                    let key = i.name.trim().to_lowercase();
                    if key.is_empty() {
                        continue;
                    }
                    let entry = items
                        .entry(key)
                        .or_insert((0usize, i.name.trim().to_string()));
                    entry.0 += 1;
                }
            }
            None if r.error.is_some() => failed += 1,
            None => waiting += 1,
        }
    }

    let period_days = (spec.end - spec.start).num_days() as usize + 1;
    let elapsed = if today > spec.end {
        period_days
    } else if today >= spec.start {
        (today - spec.start).num_days() as usize + 1
    } else {
        0
    };

    let mut h = format!("{}\n", spec.title());
    if posts == 0 {
        h.push_str("\n📭 Nothing logged in this period.");
        return (h, String::new());
    }

    h.push_str(&format!(
        "\n<b>Posts:</b> {} · analyzed {}",
        posts, analyzed
    ));
    if failed > 0 {
        h.push_str(&format!(" · ⚠️ failed {failed}"));
    }
    if waiting > 0 {
        h.push_str(&format!(" · ⏳ pending {waiting}"));
    }
    h.push('\n');
    h.push_str(&format!("<b>Calories:</b> ~{} kcal", fmt_num(total)));
    if analyzed > 0 && hi > lo {
        h.push_str(&format!(" ({}–{})", fmt_num(lo), fmt_num(hi)));
    }
    h.push('\n');
    if let Some(target) = cfg.calorie_target {
        let avg = total / elapsed.max(1) as f64;
        let pct = if target > 0 {
            avg / target as f64 * 100.0
        } else {
            0.0
        };
        h.push_str(&format!(
            "<b>Avg/day:</b> ~{} vs target {} → {}%\n",
            fmt_num(avg),
            fmt_num(target as f64),
            fmt_num(pct)
        ));
    }
    if macros_n > 0 {
        let n = elapsed.max(1) as f64;
        h.push_str(&format!(
            "🧪 <b>Macros</b> (avg/day): P {} g · F {} g · C {} g\n",
            fmt_num(protein / n),
            fmt_num(fat / n),
            fmt_num(carbs / n)
        ));
    }

    match spec.kind {
        PeriodKind::Daily => {
            h.push_str("\n<b>Timeline:</b>\n");
            for r in &sorted {
                let time = local_time_str(r.posted_at, tz);
                let (kcal_s, conf) = match &r.analysis {
                    Some(a) => (
                        format!("~{} kcal", fmt_num(a.total_kcal)),
                        a.confidence.clone(),
                    ),
                    None if r.error.is_some() => ("? (failed)".into(), "-".into()),
                    None => ("…".into(), "-".into()),
                };
                let snippet = match &r.text {
                    Some(t) => escape_html(&crate::util::truncate(t.trim(), 60)),
                    None => "(photo)".into(),
                };
                h.push_str(&format!("• {time} — {kcal_s} — “{snippet}” ({conf})\n"));
            }
            let mut streak = 0usize;
            let mut d = spec.end;
            while per_day.contains_key(&d) {
                streak += 1;
                match d.pred_opt() {
                    Some(x) => d = x,
                    None => break,
                }
            }
            if streak > 1 {
                h.push_str(&format!("\n<b>Streak:</b> {} days 🔥\n", streak));
            }
        }
        _ => {
            h.push_str("\n<b>By day:</b>\n");
            let mut d = spec.start;
            let mut missing = Vec::new();
            loop {
                match per_day.get(&d) {
                    Some(a) => h.push_str(&format!(
                        "• {} — {} post(s) — ~{} kcal\n",
                        d.format("%a %d %b"),
                        a.posts,
                        fmt_num(a.kcal)
                    )),
                    None if d <= today => missing.push(d.format("%a %d %b").to_string()),
                    None => {}
                }
                if d >= spec.end {
                    break;
                }
                d = d.succ_opt().unwrap_or(spec.end);
            }
            if !missing.is_empty() {
                h.push_str(&format!(
                    "\n⚠️ <b>No posts:</b> {}\n",
                    escape_html(&missing.join(", "))
                ));
            }
            h.push_str(&format!(
                "✅ <b>Days logged:</b> {}/{}\n",
                per_day.len(),
                elapsed.min(period_days)
            ));
        }
    }

    if !items.is_empty() {
        let mut v: Vec<&(usize, String)> = items.values().collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.0));
        v.truncate(5);
        let joined = v
            .iter()
            .map(|(n, name)| format!("{} ×{}", escape_html(name), n))
            .collect::<Vec<_>>()
            .join(", ");
        h.push_str(&format!("\n<b>Most logged:</b> {}\n", joined));
    }

    let period_label = match spec.kind {
        PeriodKind::Daily => format!("day {}", spec.start.format("%Y-%m-%d")),
        PeriodKind::Weekly => format!(
            "week {} to {}",
            spec.start.format("%Y-%m-%d"),
            spec.end.format("%Y-%m-%d")
        ),
        PeriodKind::Monthly => format!("month {}", spec.start.format("%Y-%m")),
    };
    let mut stats = format!(
        "{period_label}: {posts} posts ({analyzed} analyzed, {failed} failed); total {} kcal (range {}-{}); avg {}/day",
        fmt_num(total),
        fmt_num(lo),
        fmt_num(hi),
        fmt_num(total / elapsed.max(1) as f64)
    );
    if let Some(target) = cfg.calorie_target {
        stats.push_str(&format!("; target {} kcal", target));
    }
    stats.push_str(&format!(
        "; days logged {}/{}",
        per_day.len(),
        elapsed.min(period_days)
    ));
    if !items.is_empty() {
        let mut v: Vec<&(usize, String)> = items.values().collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.0));
        v.truncate(5);
        let joined = v
            .iter()
            .map(|(n, name)| format!("{name} x{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        stats.push_str(&format!("; top foods: {joined}"));
    }

    (h, stats)
}

// ---------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------

/// Collect records for the period and send the report (with optional AI narrative).
pub async fn send_period_report(bot: &Bot, app: &App, chat: ChatId, spec: &PeriodSpec) {
    let tz = app.cfg.tz();
    let (start_ts, end_ts) = spec.bounds_tz(tz);
    let records: Vec<MealRecord> = {
        let store = app.store.lock().await;
        store
            .records_snapshot()
            .into_iter()
            .filter(|r| r.merged_into.is_none() && r.posted_at >= start_ts && r.posted_at < end_ts)
            .collect()
    };
    let (mut html, stats) = render_report(spec, &records, &app.cfg);
    if app.cfg.ai_narrative && !records.is_empty() {
        match app.ai.narrate(&stats).await {
            Ok(n) if !n.trim().is_empty() => {
                html.push_str(&format!(
                    "\n🤖 <i>{}</i>",
                    escape_html(&crate::util::truncate(n.trim(), 700))
                ));
            }
            Ok(_) => {}
            Err(e) => log::debug!("narrative skipped: {e}"),
        }
    }
    send_html(bot, chat, &html).await;
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// Report target: REPORT_CHAT_ID → TRACKED_CHAT_ID → owner DM (stored or config).
async fn report_target(app: &App) -> Option<ChatId> {
    let owner = app.store.lock().await.owner_chat_id();
    app.cfg
        .report_chat_id
        .or(app.cfg.tracked_chat_id)
        .or(owner)
        .or(app.cfg.owner_id)
        .map(ChatId)
}

pub fn spawn_scheduler(bot: Bot, app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tick(&bot, &app).await;
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    });
}

async fn tick(bot: &Bot, app: &Arc<App>) {
    let tz = app.cfg.tz();
    let now = Utc::now().with_timezone(&tz);
    let today = now.date_naive();
    let now_time = now.time();

    // Daily: shortly after midnight, covering the day that just ended.
    if let Some(yesterday) = today.pred_opt()
        && now_time >= parse_hm(&app.cfg.daily_report_time) {
            let due = { app.store.lock().await.last_daily_covered() != Some(yesterday) };
            if due
                && let Some(target) = report_target(app).await {
                    send_period_report(bot, app, target, &daily(yesterday)).await;
                    let mut store = app.store.lock().await;
                    store.set_last_daily_covered(yesterday);
                    store.save();
                }
        }

    // Weekly: Monday morning, covering the week that just ended.
    if today.weekday() == Weekday::Mon && now_time >= parse_hm(&app.cfg.weekly_report_time) {
        let last_monday = today - CDuration::weeks(1);
        let due = { app.store.lock().await.last_weekly_covered() != Some(last_monday) };
        if due
            && let Some(target) = report_target(app).await {
                send_period_report(bot, app, target, &weekly(last_monday)).await;
                let mut store = app.store.lock().await;
                store.set_last_weekly_covered(last_monday);
                store.save();
            }
    }

    // Monthly: on the 1st, covering the month that just ended.
    if today.day() == 1 && now_time >= parse_hm(&app.cfg.monthly_report_time) {
        let prev_month = prev_month_first(today);
        let due = { app.store.lock().await.last_monthly_covered() != Some(prev_month) };
        if due
            && let Some(target) = report_target(app).await {
                send_period_report(bot, app, target, &monthly(prev_month)).await;
                let mut store = app.store.lock().await;
                store.set_last_monthly_covered(prev_month);
                store.save();
            }
    }
}
