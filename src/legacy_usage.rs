use crate::domain::ProfileId;

/// A verified snapshot of the legacy shared provider at a local transaction boundary.
/// `legacy_profile_id` is present only when the saved state fingerprint matches the snapshot and
/// the snapshot used the original shared `codex_switch` provider ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegacyUsageObservation {
    pub captured_at_unix_ms: u64,
    pub legacy_profile_id: Option<ProfileId>,
}

/// Backup observations are ordered from oldest to newest. `live` is the current validated state
/// captured after the final backup observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyUsageHistory {
    pub backups: Vec<LegacyUsageObservation>,
    pub live: LegacyUsageObservation,
}

/// A historical interval that can be assigned to one saved profile without exposing any relay
/// credentials or raw configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileLegacyUsageWindow {
    pub profile_id: ProfileId,
    pub start_unix_ms: u64,
    pub end_exclusive_unix_ms: u64,
}

/// Closed intervals can be retained in SQLite. The final live interval is intentionally kept
/// transient and open-ended until a later backup provides its closing boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LegacyUsageTimeline {
    pub durable_windows: Vec<ProfileLegacyUsageWindow>,
    pub live_windows: Vec<ProfileLegacyUsageWindow>,
}

impl LegacyUsageTimeline {
    pub fn all_windows(&self) -> Vec<ProfileLegacyUsageWindow> {
        let mut windows = self.durable_windows.clone();
        windows.extend(self.live_windows.iter().copied());
        normalize_profile_windows(windows)
    }
}

/// Reconstructs only the intervals that are supported by a valid right-hand snapshot.
///
/// A backup is written before its corresponding switch commits. Therefore the profile recorded in
/// the *right* snapshot owns the interval after the preceding backup, never the one following
/// itself. The time before the first backup is deliberately left unattributed.
pub fn reconstruct_legacy_usage(history: &LegacyUsageHistory) -> LegacyUsageTimeline {
    let mut durable_windows = Vec::new();
    let mut previous: Option<LegacyUsageObservation> = None;

    for observation in history.backups.iter().copied() {
        let Some(left) = previous else {
            previous = Some(observation);
            continue;
        };

        if left.captured_at_unix_ms < observation.captured_at_unix_ms {
            if let Some(profile_id) = observation.legacy_profile_id {
                durable_windows.push(ProfileLegacyUsageWindow {
                    profile_id,
                    start_unix_ms: left.captured_at_unix_ms,
                    end_exclusive_unix_ms: observation.captured_at_unix_ms,
                });
            }
            previous = Some(observation);
        } else {
            // A non-monotonic timestamp is not a reliable boundary. Do not bridge across it.
            previous = None;
        }
    }

    let mut live_windows = Vec::new();
    if let (Some(last_backup), Some(profile_id)) = (previous, history.live.legacy_profile_id)
        && last_backup.captured_at_unix_ms < history.live.captured_at_unix_ms
    {
        live_windows.push(ProfileLegacyUsageWindow {
            profile_id,
            start_unix_ms: last_backup.captured_at_unix_ms,
            end_exclusive_unix_ms: u64::MAX,
        });
    }

    LegacyUsageTimeline {
        durable_windows: normalize_profile_windows(durable_windows),
        live_windows: normalize_profile_windows(live_windows),
    }
}

pub fn normalize_profile_windows(
    mut windows: Vec<ProfileLegacyUsageWindow>,
) -> Vec<ProfileLegacyUsageWindow> {
    windows.retain(|window| window.start_unix_ms < window.end_exclusive_unix_ms);
    windows.sort_by(|left, right| {
        left.profile_id
            .as_uuid()
            .cmp(&right.profile_id.as_uuid())
            .then(left.start_unix_ms.cmp(&right.start_unix_ms))
            .then(left.end_exclusive_unix_ms.cmp(&right.end_exclusive_unix_ms))
    });

    let mut normalized: Vec<ProfileLegacyUsageWindow> = Vec::with_capacity(windows.len());
    for window in windows {
        if let Some(previous) = normalized.last_mut()
            && previous.profile_id == window.profile_id
            && window.start_unix_ms <= previous.end_exclusive_unix_ms
        {
            previous.end_exclusive_unix_ms = previous
                .end_exclusive_unix_ms
                .max(window.end_exclusive_unix_ms);
        } else {
            normalized.push(window);
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(value: &str) -> ProfileId {
        ProfileId::from_uuid(uuid::Uuid::parse_str(value).unwrap())
    }

    #[test]
    fn uses_the_right_hand_backup_state_to_recover_legacy_windows() {
        let openai = profile("761a7f20-bbeb-463c-8606-b4ac09d92853");
        let casdao = profile("e519bc8f-120c-43c3-96b5-a7799f6eec18");
        let timeline = reconstruct_legacy_usage(&LegacyUsageHistory {
            backups: vec![
                LegacyUsageObservation {
                    captured_at_unix_ms: 100,
                    legacy_profile_id: Some(openai),
                },
                LegacyUsageObservation {
                    captured_at_unix_ms: 200,
                    legacy_profile_id: Some(casdao),
                },
                LegacyUsageObservation {
                    captured_at_unix_ms: 300,
                    legacy_profile_id: Some(openai),
                },
            ],
            live: LegacyUsageObservation {
                captured_at_unix_ms: 400,
                legacy_profile_id: Some(openai),
            },
        });

        assert_eq!(
            timeline.durable_windows,
            vec![
                ProfileLegacyUsageWindow {
                    profile_id: openai,
                    start_unix_ms: 200,
                    end_exclusive_unix_ms: 300,
                },
                ProfileLegacyUsageWindow {
                    profile_id: casdao,
                    start_unix_ms: 100,
                    end_exclusive_unix_ms: 200,
                },
            ]
        );
        assert_eq!(
            timeline.live_windows,
            vec![ProfileLegacyUsageWindow {
                profile_id: openai,
                start_unix_ms: 300,
                end_exclusive_unix_ms: u64::MAX,
            }]
        );
    }

    #[test]
    fn never_bridges_an_invalid_timestamp_boundary() {
        let profile = profile("761a7f20-bbeb-463c-8606-b4ac09d92853");
        let timeline = reconstruct_legacy_usage(&LegacyUsageHistory {
            backups: vec![
                LegacyUsageObservation {
                    captured_at_unix_ms: 200,
                    legacy_profile_id: Some(profile),
                },
                LegacyUsageObservation {
                    captured_at_unix_ms: 100,
                    legacy_profile_id: Some(profile),
                },
                LegacyUsageObservation {
                    captured_at_unix_ms: 300,
                    legacy_profile_id: Some(profile),
                },
            ],
            live: LegacyUsageObservation {
                captured_at_unix_ms: 400,
                legacy_profile_id: None,
            },
        });

        assert!(timeline.durable_windows.is_empty());
        assert!(timeline.live_windows.is_empty());
    }
}
