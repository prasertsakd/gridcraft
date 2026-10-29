//! Visual verification and performance benchmarking for Thai line wrapping.
//!
//! Requires the pinned Noto Sans Thai font from `craft-fonts`. Set `CRAFT_FONTS_DIR` to that
//! checkout, or place it beside this repository. The example refuses to silently verify with an
//! unrelated system fallback.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, Shape, vec2};
use gridcraft_engine::Session;
use gridcraft_ui_egui::{SheetApp, monotonic_ms, thai_layout, theme};
use serde_json::json;
use sha2::{Digest, Sha256};

type Harness = egui_kittest::Harness<'static, SheetApp>;
const PINNED_NOTO_SANS_THAI_SHA256: &str = "5a1c559bb539583c8a1fd99d1c5b9491e5e14478c9cd2bd0970d5c3096cc9ef8";
const THAI_SENTENCE: &str = "วันนี้อากาศดีมาก";
const THAI_MARKS: &str = "เก้าอี้ น้ำ กำลัง ซื้อ เสื้อ เผื่อ";
const THAI_MIXED: &str = "ยอดรวม Total 1,234.50 บาท (VAT 7%) + ฿";

fn thai_font_path() -> Result<PathBuf, io::Error> {
    let fonts_root =
        std::env::var_os("CRAFT_FONTS_DIR").map(PathBuf::from).unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../craft-fonts"));
    let path = fonts_root.join("fonts/noto-sans-thai/NotoSansThai.ttf");
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("pinned Noto Sans Thai font not found at {}; set CRAFT_FONTS_DIR", path.display()),
        ));
    }
    Ok(path)
}

fn thai_font_definitions(bytes: Vec<u8>) -> FontDefinitions {
    let mut fonts = theme::pinned_font_definitions();
    fonts.font_data.insert("NotoSansThai".to_string(), Arc::new(FontData::from_owned(bytes)));
    if let Some(prop) = fonts.families.get_mut(&FontFamily::Proportional) {
        prop.push("NotoSansThai".to_string());
    }
    for role in [theme::CELL, theme::CELL_BOLD, theme::UI] {
        if let Some(chain) = fonts.families.get_mut(&FontFamily::Name(role.into())) {
            chain.push("NotoSansThai".to_string());
        }
    }
    fonts
}

fn build_session(zoom_pct: u16) -> Result<Session, Box<dyn Error>> {
    let mut session = Session::new();
    session.new_workbook();

    session.execute("home.columnWidth", json!({"cols": "A:B", "width": 80}))?;
    session.execute("home.rowHeight", json!({"rows": "1:3", "height": 65}))?;

    session.execute("cell.set", json!({"cell": "A1", "input": "รายการ"}))?;
    session.execute("cell.set", json!({"cell": "B1", "input": THAI_SENTENCE}))?;
    session.execute("home.formatCells", json!({"range": "B1", "style": {"align": {"wrap": true}}}))?;

    session.execute("cell.set", json!({"cell": "A2", "input": THAI_MARKS}))?;
    session.execute("cell.set", json!({"cell": "B2", "input": THAI_MARKS}))?;
    session.execute("home.formatCells", json!({"range": "B2", "style": {"align": {"wrap": true}}}))?;

    session.execute("cell.set", json!({"cell": "A3", "input": THAI_MIXED}))?;
    session.execute("cell.set", json!({"cell": "B3", "input": THAI_MIXED}))?;
    session.execute("home.formatCells", json!({"range": "B3", "style": {"align": {"wrap": true}}}))?;

    session.execute("data.filter", json!({"range": "A1:B3"}))?;

    session.execute("view.zoom", json!({"percent": zoom_pct}))?;
    session.execute("selection.set", json!({"range": "E10"}))?;
    Ok(session)
}

fn thai_galley_ids(harness: &Harness) -> BTreeMap<String, usize> {
    let mut galleys = BTreeMap::new();
    for shape in &harness.output().shapes {
        if let Shape::Text(text) = &shape.shape {
            let value = &text.galley.job.text;
            if thai_layout::has_thai_chars(value) {
                galleys.insert(value.clone(), Arc::as_ptr(&text.galley) as usize);
            }
        }
    }
    galleys
}

fn render_configuration(zoom_pct: u16, scale: f32, out_name: &str, thai_fonts: &FontDefinitions) -> Result<String, Box<dyn Error>> {
    let session = build_session(zoom_pct)?;
    let mut app = SheetApp::new(session, Default::default());
    app.use_prepared_fonts();
    let fonts = thai_fonts.clone();

    let mut harness = egui_kittest::Harness::builder()
        .with_context_setup(move |ctx| {
            theme::install_fonts(ctx, fonts.clone());
            theme::apply(ctx, false);
        })
        .with_size(vec2(1200.0, 700.0))
        .with_pixels_per_point(scale)
        .wgpu()
        .build_ui_state(
            |ui, app: &mut SheetApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            app,
        );

    // egui_kittest renders setup frames while building. Reinstalling the exact font set clears
    // both egui's galley cache and the Thai plan cache so the measured frame below is cold.
    theme::install_fonts(&harness.ctx, thai_fonts.clone());
    thai_layout::begin_frame_budget(&harness.ctx);

    let cold_start = monotonic_ms();
    harness.step();
    let cold_ms = monotonic_ms() - cold_start;
    let cold_usage = thai_layout::frame_budget_usage(&harness.ctx);
    if cold_usage.segmentations == 0 || cold_usage.measurements == 0 {
        return Err("cold frame did not exercise Thai segmentation and candidate measurement".into());
    }
    let cold_galleys = thai_galley_ids(&harness);

    let warm_start = monotonic_ms();
    harness.step();
    let warm_ms = monotonic_ms() - warm_start;
    let warm_usage = thai_layout::frame_budget_usage(&harness.ctx);
    let cache_usage = thai_layout::cache_usage();
    let warm_galleys = thai_galley_ids(&harness);
    let reused_galleys = warm_galleys.iter().filter(|(text, ptr)| cold_galleys.get(*text) == Some(*ptr)).count();
    let thai_galleys = warm_galleys.len();
    if warm_usage.segmentations != 0 || warm_usage.measurements != 0 {
        return Err("warm frame recomputed Thai segmentation or candidate measurements".into());
    }
    if reused_galleys != thai_galleys || thai_galleys == 0 {
        return Err(format!("warm frame reused {reused_galleys} of {thai_galleys} Thai final-line galleys").into());
    }
    if cache_usage.payload_bytes == 0 || cache_usage.payload_bytes > thai_layout::MAX_CACHE_BYTES {
        return Err(format!(
            "Thai plan cache payload {} exceeds its {} byte limit or is empty",
            cache_usage.payload_bytes,
            thai_layout::MAX_CACHE_BYTES
        )
        .into());
    }

    harness.run_steps(4);
    let image = harness.render()?;
    let out_dir = Path::new("plan/t1_verification");
    fs::create_dir_all(out_dir)?;
    let out_path = out_dir.join(out_name);
    image.save(&out_path)?;

    eprintln!(
        "Saved {} ({}x{}); cold {:.2} ms, warm {:.2} ms; segmentations {}/{}, candidates {}/{}, measured bytes {}/{}; galley reuse {}/{}; cache {}/{} bytes",
        out_path.display(),
        image.width(),
        image.height(),
        cold_ms,
        warm_ms,
        cold_usage.segmentations,
        warm_usage.segmentations,
        cold_usage.measurements,
        warm_usage.measurements,
        cold_usage.measured_bytes,
        warm_usage.measured_bytes,
        reused_galleys,
        thai_galleys,
        cache_usage.entries,
        cache_usage.payload_bytes,
    );

    Ok(format!(
        "| Zoom {zoom_pct}%, DPI {scale:.0}x | {cold_ms:.2} | {warm_ms:.2} | {} / {} | {} / {} | {} / {} | {reused_galleys} / {thai_galleys} | {} / {} / {} |",
        cold_usage.segmentations,
        warm_usage.segmentations,
        cold_usage.measurements,
        warm_usage.measurements,
        cold_usage.measured_bytes,
        warm_usage.measured_bytes,
        cache_usage.entries,
        cache_usage.payload_bytes,
        thai_layout::MAX_CACHE_BYTES,
    ))
}

fn main() -> Result<(), Box<dyn Error>> {
    let font_path = thai_font_path()?;
    let font_data = fs::read(&font_path)?;
    let font_bytes = font_data.len();
    let font_sha256 = Sha256::digest(&font_data).iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join("");
    if font_sha256 != PINNED_NOTO_SANS_THAI_SHA256 {
        return Err(format!(
            "Noto Sans Thai SHA-256 mismatch at {}: expected {}, found {}",
            font_path.display(),
            PINNED_NOTO_SANS_THAI_SHA256,
            font_sha256
        )
        .into());
    }
    eprintln!("Using pinned Thai font: {} ({font_bytes} bytes, SHA-256 {font_sha256})", font_path.display());
    let thai_fonts = thai_font_definitions(font_data);

    let rows = [
        render_configuration(100, 1.0, "thai_grid_zoom100_dpi1x.png", &thai_fonts)?,
        render_configuration(100, 2.0, "thai_grid_zoom100_dpi2x.png", &thai_fonts)?,
        render_configuration(50, 1.0, "thai_grid_zoom50_dpi1x.png", &thai_fonts)?,
        render_configuration(200, 1.0, "thai_grid_zoom200_dpi1x.png", &thai_fonts)?,
    ];
    let report = format!(
        "# T1 Thai Line Wrapping Verification Report\n\n\
        Date: 2026-10-26\n\
        OS: {} ({})\n\
        egui fork revision: 6d518fc799681571963842f95b55a6b68431b15a\n\
        Font: Noto Sans Thai Regular Version 2.002, {} bytes; SHA-256: {}\n\n\
        ## Frame Latency, Layout Work, and Warm Galley Reuse\n\n\
        Timings are single cold/warm frames from the offscreen GridCraft grid workload below; they are observations, not a statistical benchmark.\n\n\
        | Configuration | Cold (ms) | Warm (ms) | Cold / warm segmentations | Cold / warm candidate measurements | Cold / warm candidate bytes | Warm Thai galleys reusing the cold-frame Arc | Thai cache entries / payload bytes / 8 MiB limit |\n\
        |---|---:|---:|---:|---:|---:|---:|---:|\n\
        {row_0}\n\
        {row_1}\n\
        {row_2}\n\
        {row_3}\n\n\
        The Thai plan cache skips ICU segmentation and candidate measurements on hits. Final-line galleys are requested through egui's per-context layout cache; the Arc identity column records reuse observed in the rendered output.\n\n\
        ## Workload and snapshots\n\n\
        The workload has three wrapped targets in B1:B3 plus one filter header. Text lengths (Unicode scalars / UTF-8 bytes): B1 {sentence_chars} / {sentence_bytes}, B2 {marks_chars} / {marks_bytes}, B3 {mixed_chars} / {mixed_bytes}. The header's filter button reduces the available text width. Row heights are explicitly set, so this verifies T1 painting only; it does not verify AutoFit.\n\n\
        - `plan/t1_verification/thai_grid_zoom100_dpi1x.png`\n\
        - `plan/t1_verification/thai_grid_zoom100_dpi2x.png`\n\
        - `plan/t1_verification/thai_grid_zoom50_dpi1x.png`\n\
        - `plan/t1_verification/thai_grid_zoom200_dpi1x.png`\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        font_bytes,
        font_sha256,
        sentence_chars = THAI_SENTENCE.chars().count(),
        sentence_bytes = THAI_SENTENCE.len(),
        marks_chars = THAI_MARKS.chars().count(),
        marks_bytes = THAI_MARKS.len(),
        mixed_chars = THAI_MIXED.chars().count(),
        mixed_bytes = THAI_MIXED.len(),
        row_0 = rows[0],
        row_1 = rows[1],
        row_2 = rows[2],
        row_3 = rows[3],
    );
    fs::write("plan/t1_verification/REPORT.md", report)?;
    Ok(())
}
