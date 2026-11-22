//! Conservative placement load: history plus work not yet present in that history.
use super::Member;
use crate::management::v2::profile_auto_pick::{Spend, window_hours};
pub fn working(
    online: bool,
    busy: bool,
    paused: bool,
    queued: usize,
    pending_launch: bool,
) -> bool {
    !paused && ((online && (busy || queued > 0)) || pending_launch)
}
pub fn rate(
    history: Option<f64>,
    observed_at: i64,
    label: Option<&str>,
    members: &[Member],
) -> f64 {
    let mut prior = 0.0;
    let mut recent = 0.0;
    for member in members {
        let weight = Spend::new(member.model.as_deref(), member.reasoning.as_deref())
            .percent_per_hour(window_hours(label));
        // Queued work and starts in the minimum slope span are not fully in its curve.
        if !member.busy
            || member
                .started_at
                .is_none_or(|at| at >= observed_at.saturating_sub(600))
        {
            recent += weight
        } else {
            prior += weight
        }
    }
    history.filter(|v| v.is_finite()).unwrap_or(0.0).max(prior) + recent
}
