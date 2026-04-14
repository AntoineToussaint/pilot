use chrono::{DateTime, Utc};

/// Human-readable relative time ("2h ago", "3d ago", "just now").
pub fn time_ago(dt: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = now.signed_duration_since(dt);

    let secs = diff.num_seconds();
    if secs < 60 {
        return "just now".to_string();
    }

    let mins = diff.num_minutes();
    if mins < 60 {
        return format!("{mins}m ago");
    }

    let hours = diff.num_hours();
    if hours < 24 {
        return format!("{hours}h ago");
    }

    let days = diff.num_days();
    if days < 30 {
        return format!("{days}d ago");
    }

    let months = days / 30;
    if months < 12 {
        return format!("{months}mo ago");
    }

    let years = days / 365;
    format!("{years}y ago")
}

/// Staleness indicator for PRs that have been open too long or idle.
pub enum Staleness {
    /// Fresh — updated recently.
    Fresh,
    /// Getting stale — no activity for a while.
    Stale { idle_days: i64 },
    /// Very stale — been open a long time with no activity.
    Abandoned { open_days: i64, idle_days: i64 },
}

pub fn staleness(created_at: &DateTime<Utc>, updated_at: &DateTime<Utc>) -> Staleness {
    let now = Utc::now();
    let open_days = now.signed_duration_since(created_at).num_days();
    let idle_days = now.signed_duration_since(updated_at).num_days();

    if idle_days > 14 && open_days > 30 {
        Staleness::Abandoned {
            open_days,
            idle_days,
        }
    } else if idle_days > 3 {
        Staleness::Stale { idle_days }
    } else {
        Staleness::Fresh
    }
}
