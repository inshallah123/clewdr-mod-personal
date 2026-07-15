pub fn mask_str(s: &str, visible: usize) -> String {
    if s.len() > visible * 2 {
        format!("{}...{}", &s[..visible], &s[s.len() - visible..])
    } else {
        s.to_string()
    }
}

pub fn format_timestamp(ts: i64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64((ts * 1000) as f64));
    to_locale_string(&date)
}

pub fn format_iso(iso: &str) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(iso));
    to_locale_string(&date)
}

fn to_locale_string(date: &js_sys::Date) -> String {
    date.to_locale_string("default", &wasm_bindgen::JsValue::UNDEFINED)
        .as_string()
        .unwrap_or_else(|| "N/A".into())
}

pub fn copy_to_clipboard(text: String) {
    wasm_bindgen_futures::spawn_local(async move {
        let window = web_sys::window().unwrap();
        let clipboard = window.navigator().clipboard();
        let _ = wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&text)).await;
    });
}

/// "claude-opus-4-6-20260205-thinking" -> "Opus 4.6"
pub fn model_label(model: &str) -> String {
    let mut m = model.trim().to_ascii_lowercase();
    for suffix in ["-thinking", "-1m"] {
        if let Some(s) = m.strip_suffix(suffix) {
            m = s.to_string();
        }
    }
    // strip trailing date like -20250929
    if let Some((base, tail)) = m.rsplit_once('-')
        && tail.len() == 8
        && tail.starts_with("20")
        && tail.chars().all(|c| c.is_ascii_digit())
    {
        m = base.to_string();
    }
    let parts: Vec<&str> = m.strip_prefix("claude-").unwrap_or(&m).split('-').collect();
    if parts.is_empty() {
        return model.to_string();
    }
    let family = {
        let f = parts[0];
        let mut c = f.chars();
        match c.next() {
            Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
            None => f.to_string(),
        }
    };
    let version: Vec<&str> = parts[1..]
        .iter()
        .copied()
        .filter(|p| p.chars().all(|c| c.is_ascii_digit()))
        .collect();
    if version.is_empty() {
        family
    } else {
        format!("{} {}", family, version.join("."))
    }
}

/// 1234567 -> "1.23M", 4500 -> "4.5K"
pub fn format_tokens(v: u64) -> String {
    let v = v as f64;
    if v >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1}K", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// USD cost with adaptive precision
pub fn format_cost(v: f64) -> String {
    if v == 0.0 {
        "$0".into()
    } else if v < 0.01 {
        format!("${v:.4}")
    } else if v < 100.0 {
        format!("${v:.2}")
    } else {
        format!("${v:.0}")
    }
}

/// epoch secs -> "MM-DD" (UTC)
pub fn format_day_short(ts: i64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64((ts * 1000) as f64));
    format!(
        "{:02}-{:02}",
        date.get_utc_month() + 1,
        date.get_utc_date()
    )
}

/// epoch secs -> "DD HH:00" (local time)
pub fn format_hour_short(ts: i64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64((ts * 1000) as f64));
    format!("{:02}日{:02}时", date.get_date(), date.get_hours())
}
