//! Gameplay mode: finds short-worthy moments in footage that has little or no speech
//! (console captures, game recordings) from the game's sound and the amount of action on screen.
//!
//! Every signal is measured locally with FFmpeg and turned into a per-second "intensity" curve.
//! Clips are built around the peaks of that curve so the action lands near the end of each short,
//! after a short build-up, which is what makes gameplay shorts feel satisfying to watch.

/// One clip suggestion from gameplay analysis. Times are seconds in the source.
#[derive(Debug, Clone, PartialEq)]
pub struct GameplayPick {
    pub start: f64,
    pub end: f64,
    pub score: f64,
    pub kind: MomentKind,
    pub reasons: Vec<String>,
}

/// The signal that dominates a moment; it decides the suggested title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MomentKind {
    /// A sudden jump in game sound: a crash, explosion or burst of gunfire.
    Impact,
    /// The game is loud for a sustained stretch.
    Loud,
    /// Fast movement on screen: a chase, a fight, a fast drive.
    Action,
}

/// How long a suggested clip may be, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipLength {
    pub min: f64,
    pub max: f64,
}

impl ClipLength {
    /// Clamps user-supplied lengths to what makes sense for shorts.
    pub fn new(min: Option<f64>, max: Option<f64>, default_min: f64, default_max: f64) -> Self {
        let min = min.filter(|v| v.is_finite()).unwrap_or(default_min).clamp(5.0, 170.0);
        let max = max.filter(|v| v.is_finite()).unwrap_or(default_max).clamp(min, 180.0);
        Self { min, max }
    }
}

/// Mean absolute luma change between sampled frames (FFmpeg `signalstats` YDIF) over time.
pub fn parse_motion(output: &str) -> Vec<(f64, f64)> {
    let mut points = Vec::new();
    let mut time = None;
    for line in output.lines() {
        if let Some(value) = line.split_once("pts_time:").and_then(|(_, v)| v.split_whitespace().next()) {
            time = value.parse::<f64>().ok();
        }
        if let Some(value) = line.split_once("lavfi.signalstats.YDIF=").map(|(_, v)| v.trim()) {
            if let (Some(t), Ok(diff)) = (time.take(), value.parse::<f64>()) {
                if t.is_finite() && diff.is_finite() && diff >= 0.0 {
                    points.push((t, diff));
                }
            }
        }
    }
    // The first sampled frame has nothing to compare with.
    if points.first().is_some_and(|(_, diff)| *diff == 0.0) {
        points.remove(0);
    }
    points
}

/// Averages time-stamped samples into one value per whole second; seconds with no sample are None.
fn per_second(samples: &[(f64, f64)], seconds: usize) -> Vec<Option<f64>> {
    let mut sums = vec![(0.0, 0usize); seconds];
    for (t, value) in samples {
        let index = t.floor();
        if index >= 0.0 && (index as usize) < seconds {
            let slot = &mut sums[index as usize];
            slot.0 += value;
            slot.1 += 1;
        }
    }
    sums.into_iter().map(|(sum, count)| (count > 0).then(|| sum / count as f64)).collect()
}

/// Rank of each value among all values, 0 (lowest) to 1 (highest). Makes signals comparable
/// across games and capture settings: a quiet game's loudest moment still ranks near 1.
fn percentile_ranks(values: &[Option<f64>]) -> Vec<f64> {
    let mut known: Vec<f64> = values.iter().flatten().copied().collect();
    if known.len() < 2 {
        return vec![0.0; values.len()];
    }
    known.sort_by(f64::total_cmp);
    let last = (known.len() - 1) as f64;
    values
        .iter()
        .map(|value| match value {
            None => 0.0,
            Some(v) => {
                // Midpoint of the equal run, so a flat signal ranks everything at 0.5, not 1.
                let below = known.partition_point(|x| x < v) as f64;
                let through = known.partition_point(|x| x <= v) as f64;
                ((below + through - 1.0) / 2.0 / last).clamp(0.0, 1.0)
            }
        })
        .collect()
}

/// Per-second signals, each 0..1.
#[derive(Debug, Clone, Default)]
pub struct Intensity {
    pub loud: Vec<f64>,
    pub impact: Vec<f64>,
    pub motion: Vec<f64>,
    pub total: Vec<f64>,
}

/// Combines loudness (LUFS samples) and on-screen motion (YDIF samples) into per-second intensity.
pub fn intensity(duration: f64, loudness: &[(f64, f64)], motion: &[(f64, f64)]) -> Intensity {
    let seconds = duration.max(0.0).ceil() as usize;
    let level = per_second(loudness, seconds);
    let loud = percentile_ranks(&level);
    // An impact is the sound jumping well above where it was a moment ago.
    let impact: Vec<f64> = (0..seconds)
        .map(|i| {
            let Some(now) = level[i] else { return 0.0 };
            let before = level[i.saturating_sub(3)..i].iter().flatten().copied().fold(f64::INFINITY, f64::min);
            if before.is_finite() { ((now - before - 3.0) / 9.0).clamp(0.0, 1.0) } else { 0.0 }
        })
        .collect();
    let motion = percentile_ranks(&per_second(motion, seconds));
    let has_sound = level.iter().any(Option::is_some);
    let has_motion = motion.iter().any(|v| *v > 0.0);
    let total = (0..seconds)
        .map(|i| match (has_sound, has_motion) {
            (true, true) => 0.35 * loud[i] + 0.35 * impact[i] + 0.3 * motion[i],
            (true, false) => 0.5 * loud[i] + 0.5 * impact[i],
            (false, true) => motion[i],
            (false, false) => 0.0,
        })
        .collect();
    Intensity { loud, impact, motion, total }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 }
}

/// How much more a stretch is replayed than average, 0..1 (YouTube's "Most replayed" data).
pub type ReplayLift<'a> = &'a dyn Fn(f64, f64) -> f64;

/// Scores one window [start, end) of the intensity curve.
fn score_window(curve: &Intensity, start: usize, end: usize, replay: ReplayLift) -> (f64, MomentKind, Vec<String>) {
    let total = &curve.total[start..end];
    let average = mean(total);
    let peak = total.iter().copied().fold(0.0, f64::max);
    let half = total.len() / 2;
    // Action that builds and pays off late keeps people watching to the end.
    let build_up = ((mean(&total[half..]) - mean(&total[..half])) * 2.0).clamp(0.0, 1.0);
    let dead = total.iter().filter(|v| **v < 0.15).count() as f64 / total.len().max(1) as f64;
    let replayed = replay(start as f64, end as f64);
    // Ending while the action is still going feels like a mistake, not a cliffhanger.
    let cut_off = end < curve.total.len() && curve.total[end] >= 0.5 && curve.total[end - 1] >= 0.5;
    // Shorts lose viewers in the first seconds: a long calm intro before anything happens costs points.
    let wait = total.iter().position(|v| *v >= peak * 0.6).unwrap_or(0) as f64;
    let slow_start = ((wait - 6.0) * 0.02).clamp(0.0, 0.2);
    let score = (0.35 * average + 0.3 * peak + 0.15 * build_up + 0.3 * replayed - 0.25 * dead
        - if cut_off { 0.1 } else { 0.0 }
        - slow_start)
    .clamp(0.0, 1.0);

    let strongest_impact = curve.impact[start..end].iter().copied().fold(0.0, f64::max);
    let loud = mean(&curve.loud[start..end]);
    let motion = mean(&curve.motion[start..end]);
    let kind = if strongest_impact >= 0.5 {
        MomentKind::Impact
    } else if motion >= loud {
        MomentKind::Action
    } else {
        MomentKind::Loud
    };
    let mut reasons = Vec::new();
    if replayed >= 0.4 {
        reasons.push("most replayed part on YouTube".to_string());
    }
    if strongest_impact >= 0.5 {
        reasons.push("sudden burst of game sound".to_string());
    }
    if loud >= 0.65 {
        reasons.push("louder than most of the session".to_string());
    }
    if motion >= 0.65 {
        reasons.push("lots of movement on screen".to_string());
    }
    if build_up >= 0.3 {
        reasons.push("builds up to a peak".to_string());
    }
    if reasons.is_empty() {
        reasons.push("steady action".to_string());
    }
    (score, kind, reasons)
}

/// Picks up to `count` non-overlapping clips around the strongest moments of the session.
pub fn pick_gameplay(curve: &Intensity, duration: f64, count: usize, length: ClipLength, replay: ReplayLift) -> Vec<GameplayPick> {
    let seconds = curve.total.len();
    let min_len = length.min.round().max(1.0) as usize;
    if seconds < min_len || count == 0 {
        return Vec::new();
    }
    let max_len = (length.max.round() as usize).clamp(min_len, seconds);
    let lengths: Vec<usize> = {
        let mut list = vec![min_len, (min_len + max_len) / 2, max_len];
        list.dedup();
        list
    };
    // Peaks: the strongest second within 5 s either side.
    let peaks: Vec<usize> = (0..seconds)
        .filter(|&i| {
            let value = curve.total[i];
            value > 0.0 && curve.total[i.saturating_sub(5)..(i + 6).min(seconds)].iter().all(|v| *v <= value)
        })
        .collect();

    let mut candidates = Vec::new();
    for &peak in &peaks {
        // The action keeps going after its first spike: follow it until it has calmed down for 2 s.
        let threshold = curve.total[peak] * 0.5;
        let mut calm_from = seconds;
        for i in peak + 1..seconds.min(peak + 1 + max_len) {
            if curve.total[i] < threshold && curve.total.get(i + 1).is_none_or(|next| *next < threshold) {
                calm_from = i;
                break;
            }
        }
        // The natural span of the moment, from just before the spike to just after it calms down.
        let fitted_len = (calm_from + 1).saturating_sub(peak.saturating_sub(2)).clamp(min_len, max_len);
        let fitted_start = peak.saturating_sub(2).min(seconds.saturating_sub(fitted_len));
        let fitted_end = (fitted_start + fitted_len).min(seconds);
        if fitted_end - fitted_start >= min_len {
            let (score, kind, reasons) = score_window(curve, fitted_start, fitted_end, replay);
            candidates.push(GameplayPick { start: fitted_start as f64, end: (fitted_end as f64).min(duration), score, kind, reasons });
        }
        for &len in &lengths {
            let tail = ((len as f64 * 0.1).round() as usize).clamp(1, 3);
            // Two framings per length: ending just after the action calms down (shows the whole
            // payoff), and starting just before the spike (shows the moment it kicks off).
            let ends_after_action = (calm_from + tail).min(seconds).max(len.min(seconds));
            let starts_before_spike = peak.saturating_sub(3).min(seconds.saturating_sub(len));
            for start in [ends_after_action.saturating_sub(len), starts_before_spike] {
                let end = (start + len).min(seconds);
                if end - start < min_len {
                    continue;
                }
                let (score, kind, reasons) = score_window(curve, start, end, replay);
                candidates.push(GameplayPick { start: start as f64, end: (end as f64).min(duration), score, kind, reasons });
            }
        }
    }
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.start.total_cmp(&b.start)));
    let mut picked: Vec<GameplayPick> = Vec::new();
    for candidate in candidates {
        if picked.len() >= count {
            break;
        }
        if picked.iter().all(|clip| candidate.end <= clip.start || candidate.start >= clip.end) {
            picked.push(candidate);
        }
    }
    picked
}

fn timestamp(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// A short title like "GTA 6: Total chaos at 12:04". The user edits it before posting.
pub fn gameplay_title(game: Option<&str>, kind: MomentKind, start: f64) -> String {
    let moment = match kind {
        MomentKind::Impact => "Total chaos",
        MomentKind::Loud => "It got loud",
        MomentKind::Action => "Non-stop action",
    };
    match game.map(str::trim).filter(|g| !g.is_empty()) {
        Some(game) => format!("{game}: {moment} at {}", timestamp(start)),
        None => format!("{moment} at {}", timestamp(start)),
    }
}

/// A post caption for a gameplay clip.
pub fn gameplay_description(game: Option<&str>, kind: MomentKind) -> String {
    let line = match kind {
        MomentKind::Impact => "Did not see that coming.",
        MomentKind::Loud => "Things escalated fast.",
        MomentKind::Action => "No time to breathe.",
    };
    match game.map(str::trim).filter(|g| !g.is_empty()) {
        Some(game) => format!("{line} {game} gameplay."),
        None => line.to_string(),
    }
}

/// Hashtags from the game's name ("GTA 6" gives #gta6 #gta6gameplay) plus gaming and platform tags.
pub fn gameplay_hashtags(game: Option<&str>) -> String {
    let mut tags = Vec::new();
    if let Some(game) = game {
        let compact: String = game.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect();
        if !compact.is_empty() {
            tags.push(format!("#{compact}"));
            tags.push(format!("#{compact}gameplay"));
        }
    }
    for tag in ["#gaming", "#gameplay", "#shorts", "#reels", "#fyp"] {
        if !tags.iter().any(|t| t == tag) {
            tags.push(tag.to_string());
        }
    }
    tags.join(" ")
}

/// True when speech covers so little of the source that clips should come from the gameplay instead.
pub fn speech_is_sparse(speech: &[(f64, f64)], duration: f64) -> bool {
    if duration <= 0.0 {
        return true;
    }
    let spoken: f64 = speech.iter().map(|(start, end)| (end - start).max(0.0)).sum();
    spoken / duration < 0.15
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_replay(_: f64, _: f64) -> f64 {
        0.0
    }

    /// A quiet session with one loud, busy stretch at 60-70 s.
    fn session() -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        session_with(&[(60.0, 70.0)])
    }

    /// A quiet session with loud, busy stretches at the given times.
    fn session_with(hot_ranges: &[(f64, f64)]) -> (Vec<(f64, f64)>, Vec<(f64, f64)>) {
        let mut loudness = Vec::new();
        let mut motion = Vec::new();
        for tenth in 0..1200 {
            let t = tenth as f64 / 10.0;
            let hot = hot_ranges.iter().any(|(a, b)| (*a..*b).contains(&t));
            loudness.push((t, if hot { -12.0 } else { -30.0 + (tenth % 7) as f64 * 0.3 }));
            if tenth % 2 == 0 {
                motion.push((t, if hot { 30.0 } else { 2.0 + (tenth % 5) as f64 * 0.2 }));
            }
        }
        (loudness, motion)
    }

    #[test]
    fn motion_parser_reads_ydif_and_skips_the_first_frame() {
        let output = "frame:0 pts:0 pts_time:0\nlavfi.signalstats.YDIF=0.000000\nframe:1 pts:1 pts_time:0.2\nlavfi.signalstats.YDIF=4.5\nframe:2 pts:2 pts_time:0.4\nlavfi.signalstats.YDIF=nan\n";
        assert_eq!(parse_motion(output), vec![(0.2, 4.5)]);
    }

    #[test]
    fn percentile_ranks_are_flat_for_a_flat_signal() {
        let ranks = percentile_ranks(&[Some(1.0), Some(1.0), Some(1.0), None]);
        assert_eq!(ranks, vec![0.5, 0.5, 0.5, 0.0]);
    }

    #[test]
    fn loud_busy_stretch_is_picked_with_the_peak_near_the_end() {
        let (loudness, motion) = session();
        let curve = intensity(120.0, &loudness, &motion);
        let picks = pick_gameplay(&curve, 120.0, 1, ClipLength { min: 15.0, max: 30.0 }, &no_replay);
        assert_eq!(picks.len(), 1);
        let clip = &picks[0];
        // The whole 60-70 s burst, with only a short lead-in.
        assert!(clip.start <= 60.0 && clip.start >= 55.0 && clip.end >= 70.0, "clip {clip:?} misses the action");
        assert!(clip.end - clip.start >= 15.0 && clip.end - clip.start <= 30.0);
        assert_eq!(clip.kind, MomentKind::Impact);
        assert!(clip.reasons.iter().any(|r| r.contains("burst")));
    }

    #[test]
    fn picks_never_overlap_and_respect_the_count() {
        let (loudness, motion) = session();
        let curve = intensity(120.0, &loudness, &motion);
        let picks = pick_gameplay(&curve, 120.0, 4, ClipLength { min: 10.0, max: 20.0 }, &no_replay);
        assert!(!picks.is_empty() && picks.len() <= 4);
        for (i, a) in picks.iter().enumerate() {
            for b in &picks[i + 1..] {
                assert!(a.end <= b.start || a.start >= b.end, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn works_on_silent_footage_from_motion_alone() {
        let (_, motion) = session();
        let curve = intensity(120.0, &[], &motion);
        let picks = pick_gameplay(&curve, 120.0, 1, ClipLength { min: 15.0, max: 15.0 }, &no_replay);
        assert_eq!(picks.len(), 1);
        assert!(picks[0].start <= 60.0 && picks[0].end >= 61.0);
        assert_eq!(picks[0].kind, MomentKind::Action);
    }

    #[test]
    fn replays_break_the_tie_between_equal_moments() {
        let (loudness, motion) = session_with(&[(20.0, 30.0), (90.0, 100.0)]);
        let curve = intensity(120.0, &loudness, &motion);
        let replay = |start: f64, end: f64| if end > 90.0 && start < 100.0 { 1.0 } else { 0.0 };
        let picks = pick_gameplay(&curve, 120.0, 1, ClipLength { min: 15.0, max: 15.0 }, &replay);
        assert!(picks[0].start >= 75.0, "{picks:?}");
        assert!(picks[0].reasons[0].contains("replayed"));
    }

    #[test]
    fn too_short_sources_give_nothing() {
        let curve = intensity(8.0, &[(1.0, -20.0)], &[]);
        assert!(pick_gameplay(&curve, 8.0, 3, ClipLength { min: 15.0, max: 30.0 }, &no_replay).is_empty());
    }

    #[test]
    fn titles_and_tags_use_the_game_name() {
        assert_eq!(gameplay_title(Some("GTA 6"), MomentKind::Impact, 724.0), "GTA 6: Total chaos at 12:04");
        assert_eq!(gameplay_title(None, MomentKind::Action, 3725.0), "Non-stop action at 1:02:05");
        assert_eq!(gameplay_hashtags(Some("GTA 6")), "#gta6 #gta6gameplay #gaming #gameplay #shorts #reels #fyp");
        assert_eq!(gameplay_hashtags(None), "#gaming #gameplay #shorts #reels #fyp");
    }

    #[test]
    fn clip_length_is_clamped() {
        assert_eq!(ClipLength::new(Some(2.0), Some(500.0), 15.0, 60.0), ClipLength { min: 5.0, max: 180.0 });
        assert_eq!(ClipLength::new(Some(40.0), Some(20.0), 15.0, 60.0), ClipLength { min: 40.0, max: 40.0 });
        assert_eq!(ClipLength::new(None, None, 10.0, 30.0), ClipLength { min: 10.0, max: 30.0 });
    }

    #[test]
    fn sparse_speech_is_detected() {
        assert!(speech_is_sparse(&[(10.0, 20.0)], 600.0));
        assert!(!speech_is_sparse(&[(0.0, 300.0)], 600.0));
    }
}
