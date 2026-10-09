use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    pub segments: Vec<TimelineSegment>,
    pub total_duration: f64,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineSegment {
    pub id: Uuid,
    pub source_path: String,
    pub source_start: f64,
    pub source_end: f64,
    pub timeline_start: f64,
    pub timeline_end: f64,
}

impl Timeline {
    pub fn new(fps: u32, width: u32, height: u32) -> Self {
        Self {
            segments: Vec::new(),
            total_duration: 0.0,
            fps,
            width,
            height,
        }
    }

    pub fn add_segment(&mut self, source_path: String, source_start: f64, source_end: f64) {
        let timeline_start = self.total_duration;
        let timeline_end = timeline_start + (source_end - source_start);

        self.segments.push(TimelineSegment {
            id: Uuid::new_v4(),
            source_path,
            source_start,
            source_end,
            timeline_start,
            timeline_end,
        });

        self.total_duration = timeline_end;
    }

    pub fn cut_range(&mut self, cut_start: f64, cut_end: f64) {
        if cut_start >= cut_end {
            return;
        }

        let mut new_segments = Vec::new();

        for segment in &self.segments {
            let seg_start = segment.timeline_start;
            let seg_end = segment.timeline_end;

            if cut_end <= seg_start || cut_start >= seg_end {
                new_segments.push(segment.clone());
            } else {
                if cut_start > seg_start {
                    let mut left = segment.clone();
                    let source_duration = segment.source_end - segment.source_start;
                    let timeline_duration = segment.timeline_end - segment.timeline_start;
                    let ratio = source_duration / timeline_duration;
                    left.source_end = segment.source_start + (cut_start - seg_start) * ratio;
                    left.timeline_end = cut_start;
                    new_segments.push(left);
                }

                if cut_end < seg_end {
                    let mut right = segment.clone();
                    let source_duration = segment.source_end - segment.source_start;
                    let timeline_duration = segment.timeline_end - segment.timeline_start;
                    let ratio = source_duration / timeline_duration;
                    right.source_start = segment.source_start + (cut_end - seg_start) * ratio;
                    right.timeline_start = cut_end;
                    new_segments.push(right);
                }
            }
        }

        self.rebuild_timeline(new_segments);
    }

    pub fn delete_range(&mut self, delete_start: f64, delete_end: f64) {
        self.cut_range(delete_start, delete_end);
    }

    fn rebuild_timeline(&mut self, mut segments: Vec<TimelineSegment>) {
        segments.sort_by(|a, b| a.timeline_start.partial_cmp(&b.timeline_start).unwrap());

        let mut current_time = 0.0;
        for segment in &mut segments {
            let duration = segment.timeline_end - segment.timeline_start;
            segment.timeline_start = current_time;
            segment.timeline_end = current_time + duration;
            current_time = segment.timeline_end;
        }

        self.total_duration = current_time;
        self.segments = segments;
    }

    #[allow(dead_code)]
    pub fn get_segment_at_time(&self, time: f64) -> Option<&TimelineSegment> {
        self.segments.iter().find(|s| time >= s.timeline_start && time < s.timeline_end)
    }

    #[allow(dead_code)]
    pub fn get_segment_at_time_mut(&mut self, time: f64) -> Option<&mut TimelineSegment> {
        self.segments.iter_mut().find(|s| time >= s.timeline_start && time < s.timeline_end)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimelineTool {
    #[default]
    Select,
    #[allow(dead_code)]
    Cut,
}

#[derive(Debug, Clone, Default)]
pub struct TimelineState {
    pub selection_start: Option<f64>,
    pub selection_end: Option<f64>,
    pub hover_time: Option<f64>,
    #[allow(dead_code)]
    pub tool: TimelineTool,
    pub is_dragging_selection: bool,
    /// Playhead position in timeline seconds (blue marker + preview scrub).
    pub playhead: Option<f64>,
    pub is_playing: bool,
    /// Zoom: 1.0 = fit whole timeline in view; >1 zooms in.
    pub zoom: f32,
    /// Left edge of the visible window in timeline seconds.
    pub view_start: f64,
    #[allow(dead_code)]
    pub drag_start_time: Option<f64>,
    #[allow(dead_code)]
    pub drag_offset: f64,
    /// True while dragging the timeline's horizontal scrollbar.
    pub scrollbar_drag: bool,
    /// Pointer x offset from the thumb's left edge when the grab started.
    pub scrollbar_grab_dx: f32,
}

impl TimelineState {
    pub fn new() -> Self {
        Self {
            zoom: 1.0,
            ..Self::default()
        }
    }

    /// Visible window length given the full duration and current zoom.
    pub fn view_len(&self, total: f64) -> f64 {
        if total <= 0.0 {
            return 10.0;
        }
        (total / self.zoom.max(1.0) as f64).max(0.5)
    }

    pub fn clamp_view(&mut self, total: f64) {
        let len = self.view_len(total);
        let max_start = (total - len).max(0.0);
        self.view_start = self.view_start.clamp(0.0, max_start);
        if let Some(p) = self.playhead {
            if p < 0.0 || p > total {
                self.playhead = None;
            }
        }
    }

    pub fn show_all(&mut self) {
        self.zoom = 1.0;
        self.view_start = 0.0;
    }

    pub fn zoom_to_selection(&mut self, total: f64) {
        if let Some((s, e)) = self.get_selection() {
            let len = (e - s).max(0.5);
            self.zoom = (total / len).clamp(1.0, 20.0) as f32;
            self.view_start = s.max(0.0);
            self.clamp_view(total);
        } else if total > 0.0 {
            // No selection: zoom 2x centered on playhead.
            let center = self.playhead.unwrap_or(total / 2.0);
            self.zoom = 2.0;
            self.view_start = (center - total / 4.0).max(0.0);
            self.clamp_view(total);
        }
    }

    pub fn zoom_by(&mut self, factor: f32, total: f64) {
        let center = self.playhead
            .or(self.get_selection().map(|(s, e)| (s + e) / 2.0))
            .unwrap_or_else(|| self.view_start + self.view_len(total) / 2.0);
        self.zoom = (self.zoom * factor).clamp(1.0, 20.0);
        let len = self.view_len(total);
        self.view_start = (center - len / 2.0).clamp(0.0, (total - len).max(0.0));
    }

    pub fn start_selection(&mut self, time: f64) {
        self.selection_start = Some(time);
        self.selection_end = Some(time);
        self.is_dragging_selection = true;
    }

    pub fn update_selection(&mut self, time: f64) {
        if self.is_dragging_selection {
            self.selection_end = Some(time);
        }
    }

    pub fn end_selection(&mut self) {
        self.is_dragging_selection = false;
        if let (Some(start), Some(end)) = (self.selection_start, self.selection_end) {
            if start > end {
                self.selection_start = Some(end);
                self.selection_end = Some(start);
            }
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection_start = None;
        self.selection_end = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection_start.is_some() && self.selection_end.is_some()
    }

    pub fn get_selection(&self) -> Option<(f64, f64)> {
        match (self.selection_start, self.selection_end) {
            (Some(start), Some(end)) => Some((start.min(end), start.max(end))),
            _ => None,
        }
    }
}

/// Remove uniformly-sampled buckets whose center falls inside `[del_start, del_end)`.
/// `rate_per_sec` is buckets per second (e.g. waveform resolution).
pub fn splice_uniform<T>(data: &mut Vec<T>, rate_per_sec: f64, del_start: f64, del_end: f64) {
    if del_end <= del_start || data.is_empty() || rate_per_sec <= 0.0 {
        return;
    }
    let mut kept = Vec::with_capacity(data.len());
    for (i, v) in data.drain(..).enumerate() {
        let t = (i as f64 + 0.5) / rate_per_sec;
        if t < del_start || t >= del_end {
            kept.push(v);
        }
    }
    *data = kept;
}

/// Remove timestamped entries in `[del_start, del_end)` and shift later
/// entries left so the track stays compact (filmstrip thumbnails).
pub fn splice_stamped<T>(data: &mut Vec<(f64, T)>, del_start: f64, del_end: f64) {
    if del_end <= del_start || data.is_empty() {
        return;
    }
    let len = del_end - del_start;
    let mut kept = Vec::with_capacity(data.len());
    for (t, v) in data.drain(..) {
        if t < del_start {
            kept.push((t, v));
        } else if t >= del_end {
            kept.push((t - len, v));
        }
    }
    *data = kept;
}

/// Keep only timestamped entries in `[s, e)`, re-based to start at 0.
#[allow(dead_code)]
pub fn trim_stamped<T>(data: &mut Vec<(f64, T)>, s: f64, e: f64) {
    if e <= s || data.is_empty() {
        return;
    }
    let mut kept = Vec::with_capacity(data.len());
    for (t, v) in data.drain(..) {
        if t >= s && t < e {
            kept.push((t - s, v));
        }
    }
    *data = kept;
}

/// Keep only uniform buckets in `[s, e)`.
#[allow(dead_code)]
pub fn trim_uniform<T: Clone>(data: &mut Vec<T>, rate_per_sec: f64, s: f64, e: f64) {
    if e <= s || data.is_empty() || rate_per_sec <= 0.0 {
        return;
    }
    let mut kept = Vec::with_capacity(data.len());
    for (i, v) in data.drain(..).enumerate() {
        let t = (i as f64 + 0.5) / rate_per_sec;
        if t >= s && t < e {
            kept.push(v);
        }
    }
    *data = kept;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_uniform_removes_middle_buckets() {
        // 10 buckets at 1Hz => centers 0.5..9.5
        let mut v: Vec<i32> = (0..10).collect();
        splice_uniform(&mut v, 1.0, 3.0, 6.0);
        // centers 3.5,4.5,5.5 removed
        assert_eq!(v, vec![0, 1, 2, 6, 7, 8, 9]);
    }

    #[test]
    fn splice_stamped_shifts_later_entries() {
        let mut v = vec![(0.0, "a"), (1.0, "b"), (2.0, "c"), (3.0, "d")];
        splice_stamped(&mut v, 1.0, 2.0);
        assert_eq!(v, vec![(0.0, "a"), (1.0, "c"), (2.0, "d")]);
    }

    #[test]
    fn trim_stamped_rebases_to_zero() {
        let mut v = vec![(0.0, "a"), (1.0, "b"), (2.0, "c"), (3.0, "d")];
        trim_stamped(&mut v, 1.0, 3.0);
        assert_eq!(v, vec![(0.0, "b"), (1.0, "c")]);
    }

    #[test]
    fn timeline_delete_compacts_segments() {
        let mut tl = Timeline::new(30, 640, 480);
        tl.add_segment("a.mp4".into(), 0.0, 10.0);
        tl.delete_range(3.0, 6.0);
        assert!((tl.total_duration - 7.0).abs() < 1e-6);
        assert_eq!(tl.segments.len(), 2);
    }
}