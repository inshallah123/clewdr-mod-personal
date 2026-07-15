use leptos::prelude::*;

use crate::{i18n::use_i18n, types::CookieStatus, utils::format_iso};

#[component]
pub fn UsageDetails(cookie: CookieStatus) -> impl IntoView {
    let i = use_i18n();

    let quotas: Vec<(String, f64, Option<String>)> = [
        (
            cookie.session_utilization,
            cookie.session_resets_at.as_deref(),
            "cookieStatus.quota.session",
        ),
        (
            cookie.seven_day_utilization,
            cookie.seven_day_resets_at.as_deref(),
            "cookieStatus.quota.sevenDay",
        ),
        (
            cookie.seven_day_fable_utilization,
            cookie.seven_day_fable_resets_at.as_deref(),
            "cookieStatus.quota.sevenDayFable",
        ),
    ]
    .into_iter()
    .filter_map(|(val, reset, key)| {
        val.map(|v| {
            (
                i.t(key),
                v,
                reset.map(|s| format!("{} {}", i.t("cookieStatus.quota.resetsAt"), format_iso(s))),
            )
        })
    })
    .collect();

    if quotas.is_empty() {
        return None;
    }

    Some(view! {
        {quotas.into_iter().map(|(label, pct, reset)| {
            let capped = pct.min(100.0);
            let level = if capped < 70.0 { "low" } else if capped < 90.0 { "mid" } else { "high" };
            view! {
                <div>
                    <div class="progress-label">
                        <span>{label}</span>
                        <span>
                            {format!("{pct:.0}%")}
                            {reset.map(|r| format!(" · {r}"))}
                        </span>
                    </div>
                    <div class="progress">
                        <div
                            class=format!("progress-bar {level}")
                            style=format!("width:{capped:.0}%")
                        />
                    </div>
                </div>
            }
        }).collect::<Vec<_>>()}
    })
}
