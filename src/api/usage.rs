use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UsageMetric {
    pub utilization: f64,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UsageFields {
    pub five_hour: UsageMetric,
    pub seven_day: UsageMetric,
    pub seven_day_sonnet: Option<UsageMetric>,
    pub seven_day_fable: Option<UsageMetric>,
}

pub(crate) fn extract_usage_fields(usage: &Value) -> Option<UsageFields> {
    let five_hour = metric_from_window(usage.get("five_hour"))
        .or_else(|| metric_from_limit(usage, "session"))?;
    let seven_day = metric_from_window(usage.get("seven_day"))
        .or_else(|| metric_from_limit(usage, "weekly_all"))?;
    let seven_day_sonnet = metric_from_window(usage.get("seven_day_sonnet"));
    let seven_day_fable = metric_from_window(usage.get("seven_day_fable"))
        .or_else(|| fable_metric_from_limits(usage));

    Some(UsageFields {
        five_hour,
        seven_day,
        seven_day_sonnet,
        seven_day_fable,
    })
}

fn metric_from_window(window: Option<&Value>) -> Option<UsageMetric> {
    metric_from_object(window?, "utilization")
}

fn metric_from_limit(usage: &Value, kind: &str) -> Option<UsageMetric> {
    usage
        .get("limits")?
        .as_array()?
        .iter()
        .find(|limit| limit.get("kind").and_then(Value::as_str) == Some(kind))
        .and_then(|limit| metric_from_object(limit, "percent"))
}

fn fable_metric_from_limits(usage: &Value) -> Option<UsageMetric> {
    usage
        .get("limits")?
        .as_array()?
        .iter()
        .filter(|limit| limit.get("kind").and_then(Value::as_str) == Some("weekly_scoped"))
        .find(|limit| is_fable_scope(limit.get("scope")))
        .and_then(|limit| metric_from_object(limit, "percent"))
}

fn metric_from_object(value: &Value, percent_key: &str) -> Option<UsageMetric> {
    Some(UsageMetric {
        utilization: value.get(percent_key)?.as_f64()?,
        resets_at: value
            .get("resets_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn is_fable_scope(scope: Option<&Value>) -> bool {
    let Some(scope) = scope else {
        return false;
    };
    let model = scope.get("model");
    [
        model
            .and_then(|model| model.get("display_name"))
            .and_then(Value::as_str),
        model
            .and_then(|model| model.get("id"))
            .and_then(Value::as_str),
        scope.get("surface").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .any(is_fable_label)
}

fn is_fable_label(label: &str) -> bool {
    let normalized = label.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    normalized == "fable"
        || normalized == "claude-fable-5"
        || normalized.starts_with("claude-fable-5-")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::extract_usage_fields;

    #[test]
    fn extracts_fable_scoped_window_from_live_usage_shape() {
        let usage = json!({
            "five_hour": {"utilization": 10, "resets_at": "2026-07-15T12:19:59Z"},
            "seven_day": {"utilization": 29, "resets_at": "2026-07-16T06:59:59Z"},
            "seven_day_sonnet": null,
            "limits": [
                {"kind": "weekly_all", "percent": 29, "resets_at": "2026-07-16T06:59:59Z"},
                {
                    "kind": "weekly_scoped",
                    "percent": 53,
                    "resets_at": "2026-07-16T06:59:59Z",
                    "scope": {"model": {"display_name": "Fable", "id": null}}
                }
            ]
        });

        let fields = extract_usage_fields(&usage).expect("usage fields");
        let fable = fields.seven_day_fable.expect("Fable usage");
        assert_eq!(fields.seven_day.utilization, 29.0);
        assert_eq!(fable.utilization, 53.0);
        assert_eq!(fable.resets_at.as_deref(), Some("2026-07-16T06:59:59Z"));
    }
}
