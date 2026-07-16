//! Statistics tab: summary cards, stacked bar chart (pure SVG) and a detail
//! table over the usage time series.

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::{
    api,
    i18n::use_i18n,
    types::{TokenUsage, UsageSeriesApi},
    utils::{
        format_cost, format_day_short, format_hour_short, format_tokens, mask_str, model_label,
    },
};

const PALETTE: [&str; 8] = [
    "#5269d8", "#e8834a", "#4aa56b", "#c95c9c", "#6ec2d8", "#b8a53c", "#8a6fd6", "#d86a5c",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Cost,
    Tokens,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BucketKind {
    Day,
    Hour,
}

#[component]
pub fn StatsTab() -> impl IntoView {
    let i18n = use_i18n();
    let metric = RwSignal::new(Metric::Cost);
    let bucket = RwSignal::new(BucketKind::Day);
    let days = RwSignal::new(30i64);
    let cookie_filter = RwSignal::new(String::new());

    // chart series (respects bucket/days/cookie)
    let series = RwSignal::new(Option::<UsageSeriesApi>::None);
    // full-range daily series for summary cards (respects cookie filter)
    let full = RwSignal::new(Option::<UsageSeriesApi>::None);
    let loading = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);

    let fetch = move || {
        loading.set(true);
        error.set(None);
        let (b, d) = match bucket.get_untracked() {
            BucketKind::Day => ("day", days.get_untracked()),
            BucketKind::Hour => ("hour", 3),
        };
        let cookie = cookie_filter.get_untracked();
        spawn_local(async move {
            let c = (!cookie.is_empty()).then_some(cookie);
            let chart = api::get_usage_series(b, d, c.as_deref()).await;
            let cards = api::get_usage_series("day", 3650, c.as_deref()).await;
            match (chart, cards) {
                (Ok(s), Ok(f)) => {
                    series.set(Some(s));
                    full.set(Some(f));
                }
                (Err(e), _) | (_, Err(e)) => error.set(Some(e)),
            }
            loading.set(false);
        });
    };

    let _effect = Effect::new(move || {
        // re-fetch when any control changes
        bucket.get();
        days.get();
        cookie_filter.get();
        fetch();
    });

    view! {
        <div class="stack">
            <div class="row-btw">
                <h3>{move || i18n.t("usage.statsTitle")}</h3>
                <div class="row-sm">
                    // cookie filter
                    {move || {
                        let cookies = full.get().map(|f| f.cookies).unwrap_or_default();
                        view! {
                            <select
                                class="stats-select"
                                on:change=move |ev| cookie_filter.set(event_target_value(&ev))
                                prop:value=move || cookie_filter.get()
                            >
                                <option value="">{use_i18n().t("usage.allCookies")}</option>
                                {cookies.into_iter().map(|c| {
                                    let label = mask_str(&c, 6);
                                    view! { <option value=c>{label}</option> }
                                }).collect::<Vec<_>>()}
                            </select>
                        }
                    }}
                </div>
            </div>

            <Show when=move || error.get().is_some()>
                <div class="alert alert-error">{move || error.get().unwrap_or_default()}</div>
            </Show>

            // summary cards
            {move || {
                let f = full.get()?;
                let now_ms = js_sys::Date::now();
                let now = (now_ms / 1000.0) as i64;
                let today_start = now - now.rem_euclid(86400);
                let sum_since = |since: i64| {
                    let mut usage = TokenUsage::default();
                    let mut cost = 0.0;
                    for b in f.buckets.iter().filter(|b| b.start >= since) {
                        for u in b.models.values() {
                            usage.add(u);
                        }
                        cost += b.cost;
                    }
                    (usage, cost)
                };
                let cards = [
                    ("usage.today", sum_since(today_start)),
                    ("usage.last7d", sum_since(now - 7 * 86400)),
                    ("usage.last30d", sum_since(now - 30 * 86400)),
                    ("usage.allTime", sum_since(0)),
                ];
                Some(view! {
                    <div class="stat-cards">
                        {cards.into_iter().map(|(key, (usage, cost))| view! {
                            <div class="stat-card">
                                <div class="stat-card-label">{use_i18n().t(key)}</div>
                                <div class="stat-card-value">{format_cost(cost)}</div>
                                <div class="stat-card-sub">
                                    {format_tokens(usage.total_tokens())} " tokens · "
                                    {usage.requests.to_string()} " req"
                                </div>
                            </div>
                        }).collect::<Vec<_>>()}
                    </div>
                })
            }}

            // chart controls
            <div class="row-btw stats-controls">
                <div class="row-sm">
                    <ToggleGroup
                        options=vec![("cost", "usage.metricCost"), ("tokens", "usage.metricTokens")]
                        active=Signal::derive(move || match metric.get() {
                            Metric::Cost => "cost",
                            Metric::Tokens => "tokens",
                        })
                        on_pick=Callback::new(move |v: String| {
                            metric.set(if v == "tokens" { Metric::Tokens } else { Metric::Cost })
                        })
                    />
                </div>
                <div class="row-sm">
                    <ToggleGroup
                        options=vec![("day", "usage.bucketDay"), ("hour", "usage.bucketHour")]
                        active=Signal::derive(move || match bucket.get() {
                            BucketKind::Day => "day",
                            BucketKind::Hour => "hour",
                        })
                        on_pick=Callback::new(move |v: String| {
                            bucket.set(if v == "hour" { BucketKind::Hour } else { BucketKind::Day })
                        })
                    />
                    <Show when=move || bucket.get() == BucketKind::Day>
                        <ToggleGroup
                            options=vec![("7", "usage.d7"), ("30", "usage.d30"), ("90", "usage.d90")]
                            active=Signal::derive(move || match days.get() {
                                7 => "7",
                                90 => "90",
                                _ => "30",
                            })
                            on_pick=Callback::new(move |v: String| {
                                days.set(v.parse().unwrap_or(30))
                            })
                        />
                    </Show>
                </div>
            </div>

            // chart
            {move || {
                let s = series.get()?;
                if s.buckets.iter().all(|b| b.models.is_empty()) {
                    return Some(view! { <p class="text-dim text-sm">{use_i18n().t("usage.empty")}</p> }.into_any());
                }
                Some(view! { <Chart series=s metric=metric.get() /> }.into_any())
            }}

            // detail table (most recent buckets first)
            {move || {
                let s = series.get()?;
                if s.buckets.is_empty() {
                    return None;
                }
                let is_day = s.bucket == "day";
                let rows = s.buckets.iter().rev().take(50).map(|b| {
                    let mut usage = TokenUsage::default();
                    for u in b.models.values() {
                        usage.add(u);
                    }
                    let label = if is_day { format_day_short(b.start) } else { format_hour_short(b.start) };
                    let est = usage.estimated_requests > 0;
                    view! {
                        <tr>
                            <td>{label}{est.then_some(" ≈")}</td>
                            <td>{usage.requests.to_string()}</td>
                            <td>{format_tokens(usage.input)}</td>
                            <td>{format_tokens(usage.output)}</td>
                            <td>{format_tokens(usage.cache_read)}</td>
                            <td>{format_tokens(usage.cache_write)}</td>
                            <td class="usage-cost">{format_cost(b.cost)}</td>
                        </tr>
                    }
                }).collect::<Vec<_>>();
                Some(view! {
                    <div class="usage-table-wrap">
                        <table class="usage-table">
                            <thead>
                                <tr>
                                    <th>{use_i18n().t("usage.time")}</th>
                                    <th>{use_i18n().t("usage.requests")}</th>
                                    <th>{use_i18n().t("usage.input")}</th>
                                    <th>{use_i18n().t("usage.output")}</th>
                                    <th>{use_i18n().t("usage.cacheRead")}</th>
                                    <th>{use_i18n().t("usage.cacheWrite")}</th>
                                    <th>{use_i18n().t("usage.cost")}</th>
                                </tr>
                            </thead>
                            <tbody>{rows}</tbody>
                        </table>
                    </div>
                })
            }}
        </div>
    }
}

#[component]
fn ToggleGroup(
    options: Vec<(&'static str, &'static str)>,
    active: Signal<&'static str>,
    on_pick: Callback<String>,
) -> impl IntoView {
    view! {
        <div class="toggle-group">
            {options.into_iter().map(|(value, key)| {
                view! {
                    <button
                        class=move || if active.get() == value { "toggle-btn active" } else { "toggle-btn" }
                        on:click=move |_| on_pick.run(value.to_string())
                    >
                        {move || use_i18n().t(key)}
                    </button>
                }
            }).collect::<Vec<_>>()}
        </div>
    }
}

/// Pure-SVG stacked bar chart with native tooltips.
#[component]
fn Chart(series: UsageSeriesApi, metric: Metric) -> impl IntoView {
    let i18n = use_i18n();
    let is_day = series.bucket == "day";
    let models = series.models.clone();
    let color_of = |model: &str| -> &'static str {
        models
            .iter()
            .position(|m| m == model)
            .map(|i| PALETTE[i % PALETTE.len()])
            .unwrap_or(PALETTE[0])
    };
    let value_of = |usage: &TokenUsage, cost: f64| -> f64 {
        match metric {
            Metric::Cost => cost,
            Metric::Tokens => usage.total_tokens() as f64,
        }
    };

    const W: f64 = 860.0;
    const H: f64 = 260.0;
    const PAD_L: f64 = 52.0;
    const PAD_B: f64 = 22.0;
    const PAD_T: f64 = 8.0;
    let plot_w = W - PAD_L - 8.0;
    let plot_h = H - PAD_T - PAD_B;

    let buckets = &series.buckets;
    let n = buckets.len().max(1) as f64;
    let max_val = buckets
        .iter()
        .map(|b| {
            b.models
                .iter()
                .map(|(m, u)| value_of(u, b.costs.get(m).copied().unwrap_or(0.0)))
                .sum::<f64>()
        })
        .fold(0.0f64, f64::max)
        .max(1e-9);

    let fmt_val = move |v: f64| -> String {
        match metric {
            Metric::Cost => format_cost(v),
            Metric::Tokens => format_tokens(v as u64),
        }
    };

    // y gridlines at 0%, 25%, 50%, 75%, 100%
    let grid = (0..=4)
        .map(|i| {
            let frac = i as f64 / 4.0;
            let y = PAD_T + plot_h * (1.0 - frac);
            let label = fmt_val(max_val * frac);
            view! {
                <g>
                    <line x1=PAD_L y1=y x2=W - 8.0 y2=y class="chart-grid" />
                    <text x=PAD_L - 6.0 y=y + 3.0 class="chart-axis" text-anchor="end">{label}</text>
                </g>
            }
        })
        .collect::<Vec<_>>();

    let slot = plot_w / n;
    let bar_w = (slot * 0.72).min(46.0);
    let label_every = (buckets.len() / 10).max(1);

    let bars = buckets
        .iter()
        .enumerate()
        .map(|(idx, b)| {
            let x = PAD_L + slot * idx as f64 + (slot - bar_w) / 2.0;
            let time_label = if is_day {
                format_day_short(b.start)
            } else {
                format_hour_short(b.start)
            };
            let mut y = PAD_T + plot_h;
            // tooltip text: time + per-model breakdown + total
            let mut tip = format!("{time_label}\n");
            let mut total = 0.0;
            // stack in legend order (largest models at the bottom)
            let mut segs: Vec<_> = Vec::new();
            let ordered: Vec<&String> = models.iter().filter(|m| b.models.contains_key(*m)).collect();
            for model in ordered {
                let usage = &b.models[model];
                let cost = b.costs.get(model).copied().unwrap_or(0.0);
                let v = value_of(usage, cost);
                total += v;
                if v <= 0.0 {
                    continue;
                }
                let h = (v / max_val) * plot_h;
                y -= h;
                tip.push_str(&format!("{}: {}\n", model_label(model), fmt_val(v)));
                segs.push(view! {
                    <rect
                        x=x
                        y=y
                        width=bar_w
                        height=h.max(0.5)
                        fill=color_of(model)
                        rx=1.5
                    />
                });
            }
            tip.push_str(&format!("{}: {}", i18n.t("usage.total"), fmt_val(total)));
            let show_label = idx % label_every == 0;
            view! {
                <g class="chart-bar">
                    <title>{tip}</title>
                    {segs}
                    // invisible hover target covering the whole column
                    <rect x=PAD_L + slot * idx as f64 y=PAD_T width=slot height=plot_h fill="transparent" />
                    {show_label.then(|| view! {
                        <text
                            x=x + bar_w / 2.0
                            y=H - 6.0
                            class="chart-axis"
                            text-anchor="middle"
                        >{time_label.clone()}</text>
                    })}
                </g>
            }
        })
        .collect::<Vec<_>>();

    // legend
    let legend = models
        .iter()
        .map(|m| {
            let color = color_of(m);
            view! {
                <span class="legend-item">
                    <span class="legend-dot" style=format!("background:{color}")></span>
                    <span title=m.clone()>{model_label(m)}</span>
                </span>
            }
        })
        .collect::<Vec<_>>();

    view! {
        <div class="chart-wrap">
            <svg viewBox=format!("0 0 {W} {H}") class="usage-chart" preserveAspectRatio="none">
                {grid}
                {bars}
            </svg>
            <div class="chart-legend">{legend}</div>
        </div>
    }
}
