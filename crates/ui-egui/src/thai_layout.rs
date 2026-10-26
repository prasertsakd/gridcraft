//! Thai text line wrapping for committed worksheet cells.
//!
//! Clean-room implementation providing word boundary-aware line wrapping for Thai script in
//! spreadsheet cells. Uses ICU4X LSTM-based LineSegmenter for complex script boundary detection
//! and egui's resolved font metrics for greedy line placement.
//!
//! Follows the strict repository safety policy: `#![deny(clippy::unwrap_used)]`, no panics,
//! checked arithmetic, finite geometry validation, bounded budgets, and memory-capped LRU cache.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use egui::{Color32, FontId, Painter, Rect, Vec2, pos2, vec2};
use gridcraft_engine::core::Value;
use gridcraft_engine::model::{HAlign, Style, VAlign};
use icu_segmenter::options::LineBreakOptions;
use icu_segmenter::{LineSegmenter, LineSegmenterBorrowed};

/// Maximum number of Unicode scalars permitted in a Thai text cell before falling back.
pub const MAX_THAI_SCALARS: usize = 32_767;

/// Maximum candidate measurement operations allowed per individual cell.
pub const MAX_CELL_MEASUREMENTS: usize = 512;

/// Maximum cumulative candidate bytes measured per individual cell (1 MiB).
pub const MAX_CELL_MEASURED_BYTES: usize = 1024 * 1024;

/// Maximum cache-miss candidate measurements allowed across a single frame.
pub const MAX_FRAME_MEASUREMENTS: usize = 4_096;

/// Maximum cumulative cache-miss bytes measured across a single frame (4 MiB).
pub const MAX_FRAME_MEASURED_BYTES: usize = 4 * 1024 * 1024;

/// Maximum entries in the Thai layout LRU cache.
pub const MAX_CACHE_ENTRIES: usize = 1_024;

/// Maximum payload memory consumed by the Thai layout LRU cache (8 MiB).
pub const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Global frame measurement counter for cache-miss budget accounting.
static FRAME_MEASUREMENTS: AtomicUsize = AtomicUsize::new(0);
static FRAME_MEASURED_BYTES: AtomicUsize = AtomicUsize::new(0);
static FONT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Increments font generation to invalidate cached layouts when font definitions change.
pub fn bump_font_generation() {
    FONT_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Reset per-frame measurement budgets at the start of a frame.
pub fn begin_frame_budget() {
    FRAME_MEASUREMENTS.store(0, Ordering::Relaxed);
    FRAME_MEASURED_BYTES.store(0, Ordering::Relaxed);
}

/// Thai script segmenter using ICU4X LSTM model with compiled complex scripts data.
static THAI_SEGMENTER: OnceLock<LineSegmenterBorrowed<'static>> = OnceLock::new();

fn segmenter() -> LineSegmenterBorrowed<'static> {
    *THAI_SEGMENTER.get_or_init(|| LineSegmenter::new_lstm(LineBreakOptions::default()))
}

/// Returns true if `text` contains at least one character in the Thai Unicode block (U+0E00..=U+0E7F).
#[inline]
pub fn has_thai_chars(text: &str) -> bool {
    text.chars().any(|c| ('\u{0E00}'..='\u{0E7F}').contains(&c))
}

/// The type of break ending a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakKind {
    /// Soft wrap chosen by layout calculation to fit within available width.
    SoftWrap,
    /// Explicit mandatory line break (`\n` or `\r\n`).
    HardBreak,
}

/// Exact UTF-8 byte ranges for a layout line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextLineRange {
    /// Byte range in original string consumed by this line, including trailing breaks/spaces.
    /// Concatenating `source_range` across all lines reconstructs the original text byte-for-byte.
    pub source_range: Range<usize>,
    /// Byte range in original string that is actually measured and painted.
    pub paint_range: Range<usize>,
    /// Break kind at the end of this line, if any.
    pub break_kind: Option<BreakKind>,
}

/// Represents one laid out line of Thai text.
#[derive(Clone, Debug)]
pub struct ThaiLayoutLine {
    pub range: TextLineRange,
    pub galley: Arc<egui::Galley>,
}

/// Completed Thai cell layout ready for painting.
#[derive(Clone, Debug)]
pub struct ThaiCellLayout {
    pub lines: Vec<ThaiLayoutLine>,
    pub size: Vec2,
}

/// Reason for falling back to legacy layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    NoThai,
    WrapDisabled,
    Rotated(i16),
    UnsupportedHAlign,
    UnsupportedVAlign,
    NumericOrDate,
    TextTooLong(usize),
    BudgetExhausted,
    InvalidWidth,
    FrameBudgetExhausted,
}

/// Result of evaluating Thai layout for a cell.
#[derive(Clone, Debug)]
pub enum ThaiLayoutResult {
    Ready(ThaiCellLayout),
    LegacyFallback(FallbackReason),
}

/// Returns whether a cell's styling and value meet all preconditions for Thai line wrapping.
pub fn is_eligible(text: &str, style: &Style, value: &Value, numeric: bool) -> Result<(), FallbackReason> {
    if !has_thai_chars(text) {
        return Err(FallbackReason::NoThai);
    }
    if !style.align.wrap {
        return Err(FallbackReason::WrapDisabled);
    }
    if style.align.rotation != 0 && style.align.rotation != 255 {
        return Err(FallbackReason::Rotated(style.align.rotation));
    }
    if numeric || matches!(value, Value::Number(_)) {
        return Err(FallbackReason::NumericOrDate);
    }
    match style.align.h {
        HAlign::General | HAlign::Left | HAlign::Center | HAlign::CenterAcross | HAlign::Right => {}
        _ => return Err(FallbackReason::UnsupportedHAlign),
    }
    match style.align.v {
        VAlign::Top | VAlign::Center | VAlign::Bottom => {}
        _ => return Err(FallbackReason::UnsupportedVAlign),
    }
    Ok(())
}

/// Breaks `text` into paragraph segments delimited by hard line breaks (`\n` or `\r\n`).
/// Each paragraph returns its byte range in `text` excluding the hard break, and the full range including the break.
fn split_hard_breaks(text: &str) -> Vec<(Range<usize>, Range<usize>, bool)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut i = 0;
    let len = bytes.len();

    while i < len {
        if bytes.get(i) == Some(&b'\n') {
            let content_end = if i > start && bytes.get(i - 1) == Some(&b'\r') { i - 1 } else { i };
            let full_end = i.saturating_add(1);
            out.push((start..content_end, start..full_end, true));
            start = full_end;
        }
        i = i.saturating_add(1);
    }

    if start < len {
        out.push((start..len, start..len, false));
    } else if len > 0 && bytes.get(len.saturating_sub(1)) == Some(&b'\n') {
        // Trailing newline produces an empty trailing line
        out.push((len..len, len..len, false));
    } else if len == 0 {
        out.push((0..0, 0..0, false));
    }

    out
}

/// Computes the break opportunity byte offsets within `para_text` using ICU4X line segmenter.
pub fn get_break_opportunities(para_text: &str) -> Vec<usize> {
    if para_text.is_empty() {
        return vec![0];
    }
    let mut breaks = Vec::new();
    let seg = segmenter();
    for offset in seg.segment_str(para_text) {
        if offset <= para_text.len() {
            breaks.push(offset);
        }
    }
    if breaks.first().copied() != Some(0) {
        breaks.insert(0, 0);
    }
    if breaks.last().copied() != Some(para_text.len()) {
        breaks.push(para_text.len());
    }
    breaks.dedup();
    breaks
}

/// Cached representation of laid out line boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedPlanLine {
    range: TextLineRange,
}

#[derive(Clone, Debug)]
struct CachedPlan {
    lines: Vec<CachedPlanLine>,
    total_size: Vec2,
    payload_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    text: String,
    family_name: String,
    font_size_bits: u32,
    avail_width_bits: u32,
    font_gen: u64,
    zoom_pct: u16,
    halign: u8,
}

struct LayoutCache {
    order: VecDeque<CacheKey>,
    entries: std::collections::HashMap<CacheKey, CachedPlan>,
    total_bytes: usize,
}

impl LayoutCache {
    fn new() -> Self {
        Self { order: VecDeque::new(), entries: std::collections::HashMap::new(), total_bytes: 0 }
    }

    fn get(&mut self, key: &CacheKey) -> Option<CachedPlan> {
        let plan = self.entries.get(key)?.clone();
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
            self.order.push_back(key.clone());
        }
        Some(plan)
    }

    fn insert(&mut self, key: CacheKey, plan: CachedPlan) {
        if let Some(old) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(old.payload_bytes);
            if let Some(pos) = self.order.iter().position(|k| k == &key) {
                self.order.remove(pos);
            }
        }

        while self.entries.len() >= MAX_CACHE_ENTRIES || self.total_bytes.saturating_add(plan.payload_bytes) > MAX_CACHE_BYTES {
            if let Some(lru_key) = self.order.pop_front() {
                if let Some(evicted) = self.entries.remove(&lru_key) {
                    self.total_bytes = self.total_bytes.saturating_sub(evicted.payload_bytes);
                }
            } else {
                break;
            }
        }

        self.total_bytes = self.total_bytes.saturating_add(plan.payload_bytes);
        self.order.push_back(key.clone());
        self.entries.insert(key, plan);
    }
}

static CACHE: Mutex<Option<LayoutCache>> = Mutex::new(None);

/// Pure layout algorithm: selects line break boundaries for `text` given `avail_width`.
/// Uses `measure_fn` to measure candidate substring widths.
pub fn select_line_ranges<F>(text: &str, avail_width: f32, mut measure_fn: F) -> Result<Vec<TextLineRange>, FallbackReason>
where
    F: FnMut(&str) -> Result<f32, FallbackReason>,
{
    if !avail_width.is_finite() || avail_width < 1.0 {
        return Err(FallbackReason::InvalidWidth);
    }

    let scalar_count = text.chars().count();
    if scalar_count > MAX_THAI_SCALARS {
        return Err(FallbackReason::TextTooLong(scalar_count));
    }

    let hard_lines = split_hard_breaks(text);
    let mut out_ranges = Vec::new();
    let mut cell_measurements = 0usize;
    let mut cell_measured_bytes = 0usize;

    for (content_range, full_range, has_hard_break) in hard_lines {
        let para_content = text.get(content_range.clone()).unwrap_or("");
        if para_content.is_empty() {
            out_ranges.push(TextLineRange {
                source_range: full_range,
                paint_range: content_range,
                break_kind: if has_hard_break { Some(BreakKind::HardBreak) } else { None },
            });
            continue;
        }

        let breaks = get_break_opportunities(para_content);
        let mut line_start_local = 0usize;

        while line_start_local < para_content.len() {
            let candidates: Vec<usize> = breaks.iter().copied().filter(|&b| b > line_start_local).collect();

            if candidates.is_empty() {
                break;
            }

            let mut best_fit: Option<(usize, usize)> = None;

            for &cand_local in &candidates {
                cell_measurements = cell_measurements.saturating_add(1);
                if cell_measurements > MAX_CELL_MEASUREMENTS {
                    return Err(FallbackReason::BudgetExhausted);
                }

                let cand_bytes = cand_local.saturating_sub(line_start_local);
                cell_measured_bytes = cell_measured_bytes.saturating_add(cand_bytes);
                if cell_measured_bytes > MAX_CELL_MEASURED_BYTES {
                    return Err(FallbackReason::BudgetExhausted);
                }

                let frame_m = FRAME_MEASUREMENTS.fetch_add(1, Ordering::Relaxed);
                if frame_m >= MAX_FRAME_MEASUREMENTS {
                    return Err(FallbackReason::FrameBudgetExhausted);
                }

                let frame_b = FRAME_MEASURED_BYTES.fetch_add(cand_bytes, Ordering::Relaxed);
                if frame_b >= MAX_FRAME_MEASURED_BYTES {
                    return Err(FallbackReason::FrameBudgetExhausted);
                }

                let candidate_slice = para_content.get(line_start_local..cand_local).unwrap_or("");
                let paint_trimmed = candidate_slice.trim_end_matches([' ', '\t']);
                let paint_len = paint_trimmed.len();
                let cand_paint_end = line_start_local.saturating_add(paint_len);

                let w = measure_fn(paint_trimmed)?;

                if w <= avail_width {
                    best_fit = Some((cand_local, cand_paint_end));
                } else {
                    break;
                }
            }

            let (chosen_break_local, chosen_paint_end_local, is_soft) = match best_fit {
                Some((b, p)) => {
                    let is_soft = b < para_content.len();
                    (b, p, is_soft)
                }
                None => {
                    // Even the first segment is wider than avail_width: take the first segment and clip
                    let first = candidates.first().copied().unwrap_or(para_content.len());
                    let cand_slice = para_content.get(line_start_local..first).unwrap_or("");
                    let paint_len = cand_slice.trim_end_matches([' ', '\t']).len();
                    let is_soft = first < para_content.len();
                    (first, line_start_local.saturating_add(paint_len), is_soft)
                }
            };

            let global_line_start = content_range.start.saturating_add(line_start_local);
            let global_line_end = content_range.start.saturating_add(chosen_break_local);
            let global_paint_end = content_range.start.saturating_add(chosen_paint_end_local);

            let (source_range, break_kind) = if chosen_break_local == para_content.len() {
                // End of this paragraph: include hard break bytes in source_range if present
                let final_end = if has_hard_break { full_range.end } else { global_line_end };
                (global_line_start..final_end, if has_hard_break { Some(BreakKind::HardBreak) } else { None })
            } else {
                (global_line_start..global_line_end, if is_soft { Some(BreakKind::SoftWrap) } else { None })
            };

            out_ranges.push(TextLineRange { source_range, paint_range: global_line_start..global_paint_end, break_kind });

            line_start_local = chosen_break_local;
        }
    }

    Ok(out_ranges)
}

/// Executes Thai layout for a cell, returning the ready layout or fallback reason.
pub fn layout_cell(p: &Painter, text: &str, font: &FontId, color: Color32, avail_width: f32, halign: HAlign, zoom: f32) -> ThaiLayoutResult {
    let font_gen = FONT_GENERATION.load(Ordering::Relaxed);
    let cache_key = CacheKey {
        text: text.to_string(),
        family_name: match &font.family {
            egui::FontFamily::Proportional => "Proportional".to_string(),
            egui::FontFamily::Monospace => "Monospace".to_string(),
            egui::FontFamily::Name(n) => n.to_string(),
        },
        font_size_bits: font.size.to_bits(),
        avail_width_bits: avail_width.to_bits(),
        font_gen,
        zoom_pct: (zoom * 100.0).clamp(0.0, 65535.0) as u16,
        halign: match halign {
            HAlign::Center | HAlign::CenterAcross => 1,
            HAlign::Right => 2,
            _ => 0,
        },
    };

    // Check cache
    if let Ok(mut cache_guard) = CACHE.lock() {
        let cache = cache_guard.get_or_insert_with(LayoutCache::new);
        if let Some(plan) = cache.get(&cache_key) {
            let mut lines = Vec::with_capacity(plan.lines.len());
            for l in &plan.lines {
                let paint_text = text.get(l.range.paint_range.clone()).unwrap_or("");
                let galley = p.layout_no_wrap(paint_text.to_string(), font.clone(), color);
                lines.push(ThaiLayoutLine { range: l.range.clone(), galley });
            }
            return ThaiLayoutResult::Ready(ThaiCellLayout { lines, size: plan.total_size });
        }
    }

    // Cache miss: run layout with measurement
    let ranges_res = select_line_ranges(text, avail_width, |slice| {
        let g = p.layout_no_wrap(slice.to_string(), font.clone(), color);
        Ok(g.size().x)
    });

    let ranges = match ranges_res {
        Ok(r) => r,
        Err(e) => return ThaiLayoutResult::LegacyFallback(e),
    };

    let mut lines = Vec::with_capacity(ranges.len());
    let mut plan_lines = Vec::with_capacity(ranges.len());
    let mut total_h = 0.0f32;
    let mut max_w = 0.0f32;

    for r in ranges {
        let paint_text = text.get(r.paint_range.clone()).unwrap_or("");
        let galley = p.layout_no_wrap(paint_text.to_string(), font.clone(), color);
        let sz = galley.size();
        total_h += sz.y;
        if sz.x > max_w {
            max_w = sz.x;
        }
        plan_lines.push(CachedPlanLine { range: r.clone() });
        lines.push(ThaiLayoutLine { range: r, galley });
    }

    let total_size = vec2(max_w, total_h);
    let payload_bytes = std::mem::size_of::<CachedPlan>()
        .saturating_add(plan_lines.len().saturating_mul(std::mem::size_of::<CachedPlanLine>()))
        .saturating_add(text.len());

    let plan = CachedPlan { lines: plan_lines, total_size, payload_bytes };

    if let Ok(mut cache_guard) = CACHE.lock() {
        let cache = cache_guard.get_or_insert_with(LayoutCache::new);
        cache.insert(cache_key, plan);
    }

    ThaiLayoutResult::Ready(ThaiCellLayout { lines, size: total_size })
}

/// Paints the ready Thai cell layout inside `rect` respecting alignment, padding, and clipping.
pub fn paint_thai_cell(
    p: &Painter,
    layout: &ThaiCellLayout,
    rect: Rect,
    text_left: f32,
    halign: HAlign,
    valign: VAlign,
    pad: f32,
    indent: f32,
    z: f32,
    color: Color32,
) {
    if layout.lines.is_empty() {
        return;
    }

    let clip = rect.intersect(p.clip_rect());
    let painter = p.with_clip_rect(clip);

    let pad_v = 1.5 * z;
    let y_top = rect.top() + pad_v;
    let y_bottom = rect.bottom() - layout.size.y - pad_v;
    let mut cur_y = match valign {
        VAlign::Top => y_top,
        VAlign::Bottom => y_bottom,
        _ => (y_top + y_bottom) / 2.0,
    };

    for line in &layout.lines {
        let line_w = line.galley.size().x;
        let line_h = line.galley.size().y;

        let x = match halign {
            HAlign::Center | HAlign::CenterAcross => rect.center().x - line_w / 2.0,
            HAlign::Right => rect.right() - pad - line_w,
            _ => text_left + pad + indent,
        };

        painter.galley(pos2(x, cur_y), line.galley.clone(), color);
        cur_y += line_h;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_thai_chars() {
        assert!(has_thai_chars("สวัสดี"));
        assert!(has_thai_chars("Hello สวัสดี"));
        assert!(has_thai_chars("123 บาท"));
        assert!(!has_thai_chars("Hello World"));
        assert!(!has_thai_chars("123.45"));
        assert!(!has_thai_chars(""));
    }

    #[test]
    fn test_hard_breaks_reconstruction() {
        let cases = ["สวัสดี", "สวัสดี\n", "สวัสดี\r\n", "สวัสดี\nชาวโลก", "สวัสดี\r\nชาวโลก\r\n", "\n\n\n", "\r\n\r\n", "", "บรรทัดหนึ่ง\nบรรทัดสอง\nบรรทัดสาม"];

        for text in cases {
            let parts = split_hard_breaks(text);
            let mut reconstructed = String::new();
            for (_, full, _) in &parts {
                reconstructed.push_str(&text[full.clone()]);
            }
            assert_eq!(reconstructed, text, "Full source reconstruction failed for {text:?}");
        }
    }

    #[test]
    fn test_thai_word_breaks_human_verified() {
        // "วันนี้อากาศดีมาก" consists of "วันนี้" (today) + "อากาศ" (weather) + "ดีมาก" (very good)
        let text = "วันนี้อากาศดีมาก";
        let breaks = get_break_opportunities(text);
        assert!(breaks.contains(&0));
        assert!(breaks.contains(&text.len()));
        // Must contain "วันนี้" boundary (18 bytes)
        assert!(breaks.contains(&18), "Missing expected break at วันนี้ boundary in {breaks:?}");
    }

    #[test]
    fn test_source_range_exact_concat_invariant() {
        let texts = [
            "วันนี้อากาศดีมาก",
            "เก้าอี้ น้ำ กำลัง ซื้อ เสื้อ เผื่อ",
            "ยอดรวม Total 1,234.50 บาท (VAT 7%) + ฿",
            "ข้อความไทย\nบรรทัดสอง\r\nบรรทัดสามและสี่",
            "ข้อความไทยยาวมากๆ ที่ต้องตัดคำหลายๆ บรรทัดติดต่อกันอย่างต่อเนื่อง",
            "   ช่องว่างนำหน้า และช่องว่างท้าย   ",
            "คำเดียวที่ยาวมากๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆๆ",
        ];

        for text in texts {
            for avail_w in [30.0, 50.0, 100.0, 150.0, 500.0] {
                let ranges = select_line_ranges(text, avail_w, |s| {
                    // Simple mock width proportional to character count
                    Ok(s.chars().count() as f32 * 10.0)
                })
                .expect("select_line_ranges must succeed");

                assert!(!ranges.is_empty(), "Ranges must not be empty for text {text:?}");

                let mut concat = String::new();
                let mut prev_end = 0;
                for r in &ranges {
                    assert_eq!(r.source_range.start, prev_end, "Discontinuity in source ranges");
                    concat.push_str(&text[r.source_range.clone()]);
                    prev_end = r.source_range.end;
                }
                assert_eq!(concat, text, "Concat of source ranges failed for w={avail_w}");
            }
        }
    }

    #[test]
    fn test_oversized_first_segment_clipped_safely() {
        // Single word wider than cell width
        let text = "น้ำ"; // single unbreakable word
        let avail_w = 2.0; // too narrow even for single word
        let ranges = select_line_ranges(text, avail_w, |_| Ok(60.0)).expect("must handle oversized safely");
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].source_range.clone()], text);

        // Multi-word where each word and space is wider than cell
        let multi = "วัน วัน วัน";
        let multi_ranges = select_line_ranges(multi, 2.0, |_| Ok(50.0)).expect("multi-word oversized");
        assert_eq!(multi_ranges.len(), 5);
        let mut concat = String::new();
        for r in &multi_ranges {
            concat.push_str(&multi[r.source_range.clone()]);
        }
        assert_eq!(concat, multi);
    }

    #[test]
    fn test_budget_exhaustion_returns_fallback() {
        // Text with over 512 break opportunities to trigger MAX_CELL_MEASUREMENTS budget exhaustion
        let text = "คำ ".repeat(600);
        let res = select_line_ranges(&text, 10000.0, |_| Ok(5.0));
        assert!(matches!(res, Err(FallbackReason::BudgetExhausted)));
    }

    #[test]
    fn test_invalid_widths_return_fallback() {
        assert_eq!(select_line_ranges("สวัสดี", -10.0, |_| Ok(10.0)).unwrap_err(), FallbackReason::InvalidWidth);
        assert_eq!(select_line_ranges("สวัสดี", f32::NAN, |_| Ok(10.0)).unwrap_err(), FallbackReason::InvalidWidth);
        assert_eq!(select_line_ranges("สวัสดี", 0.0, |_| Ok(10.0)).unwrap_err(), FallbackReason::InvalidWidth);
    }
}
