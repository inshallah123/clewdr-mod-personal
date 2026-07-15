use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::{
    api,
    i18n::use_i18n,
    types::CookieUsageSummary,
    utils::{format_cost, format_tokens, model_label},
};

/// Modal showing lifetime per-model usage & cost for a single cookie.
#[component]
pub fn UsageModal(cookie: String, on_close: Callback<()>) -> impl IntoView {
    let i18n = use_i18n();
    let data = RwSignal::new(Option::<CookieUsageSummary>::None);
    let loading = RwSignal::new(true);
    let error = RwSignal::new(Option::<String>::None);

    {
        let cookie = cookie.clone();
        spawn_local(async move {
            match api::get_usage_summary().await {
                Ok(summary) => {
                    let entry = summary.cookies.into_iter().find(|c| c.cookie == cookie);
                    data.set(entry);
                }
                Err(e) => error.set(Some(e)),
            }
            loading.set(false);
        });
    }

    let masked = crate::utils::mask_str(&cookie, 6);

    view! {
        <div class="modal-backdrop" on:click=move |_| on_close.run(())>
            <div class="modal" on:click=|ev| ev.stop_propagation()>
                <div class="row-btw" style="margin-bottom:0.75rem">
                    <div>
                        <h3 style="margin:0">{move || i18n.t("usage.title")}</h3>
                        <span class="text-mono text-xs text-dim">{masked.clone()}</span>
                    </div>
                    <button class="icon-del" on:click=move |_| on_close.run(())>"✕"</button>
                </div>

                <Show when=move || loading.get()>
                    <p class="text-dim text-sm">{move || i18n.t("common.loading")}</p>
                </Show>
                <Show when=move || error.get().is_some()>
                    <div class="alert alert-error">{move || error.get().unwrap_or_default()}</div>
                </Show>

                {move || {
                    if loading.get() || error.get().is_some() {
                        return None;
                    }
                    let Some(entry) = data.get() else {
                        return Some(view! {
                            <p class="text-dim text-sm">{i18n.t("usage.empty")}</p>
                        }.into_any());
                    };
                    let any_estimated = entry
                        .models
                        .iter()
                        .any(|m| m.usage.estimated_requests > 0);
                    let total = entry.total;
                    let total_cost = entry.total_cost;
                    Some(view! {
                        <div class="usage-table-wrap">
                            <table class="usage-table">
                                <thead>
                                    <tr>
                                        <th>{i18n.t("usage.model")}</th>
                                        <th>{i18n.t("usage.requests")}</th>
                                        <th>{i18n.t("usage.input")}</th>
                                        <th>{i18n.t("usage.output")}</th>
                                        <th>{i18n.t("usage.cacheRead")}</th>
                                        <th>{i18n.t("usage.cacheWrite")}</th>
                                        <th>{i18n.t("usage.cost")}</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {entry.models.iter().map(|row| {
                                        let est = row.usage.estimated_requests > 0;
                                        view! {
                                            <tr>
                                                <td>
                                                    <span title=row.model.clone()>{model_label(&row.model)}</span>
                                                    {est.then(|| view! { <span class="text-dim" title=i18n.t("usage.estimatedNote")>" ≈"</span> })}
                                                </td>
                                                <td>{row.usage.requests.to_string()}</td>
                                                <td>{format_tokens(row.usage.input)}</td>
                                                <td>{format_tokens(row.usage.output)}</td>
                                                <td>{format_tokens(row.usage.cache_read)}</td>
                                                <td>{format_tokens(row.usage.cache_write)}</td>
                                                <td class="usage-cost">{format_cost(row.cost)}</td>
                                            </tr>
                                        }
                                    }).collect::<Vec<_>>()}
                                </tbody>
                                <tfoot>
                                    <tr>
                                        <td>{i18n.t("usage.total")}</td>
                                        <td>{total.requests.to_string()}</td>
                                        <td>{format_tokens(total.input)}</td>
                                        <td>{format_tokens(total.output)}</td>
                                        <td>{format_tokens(total.cache_read)}</td>
                                        <td>{format_tokens(total.cache_write)}</td>
                                        <td class="usage-cost">{format_cost(total_cost)}</td>
                                    </tr>
                                </tfoot>
                            </table>
                        </div>
                        {any_estimated.then(|| view! {
                            <p class="text-xs text-mute" style="margin-top:0.5rem">
                                {i18n.t("usage.estimatedNote")}
                            </p>
                        })}
                    }.into_any())
                }}
            </div>
        </div>
    }
}
