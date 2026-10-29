//! Reproducible CPU/UI benchmark through the production committed-cell Grid renderer.
//!
//! Run with `--release --features thai-benchmark-diagnostics`, a pinned `CRAFT_FONTS_DIR`, and
//! separate `CARGO_TARGET_DIR`s for the base and head snapshots. The harness never enables the
//! GPU renderer and does not screenshot or encode images inside any measured interval.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

use egui::{FontData, FontDefinitions, FontFamily, FontId, Shape, vec2};
use gridcraft_engine::Session;
use gridcraft_engine::core::CellRef;
use gridcraft_ui_egui::{SheetApp, thai_benchmark, theme};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

type Harness = egui_kittest::Harness<'static, SheetApp>;
type FirstLineGalleys = BTreeMap<u32, Arc<egui::Galley>>;
type GeometryCoverage = (Vec<u32>, String, usize, FirstLineGalleys, [f32; 4], [f32; 4], [f32; 4], String);
type PassEvidence = (PassRecord, FirstLineGalleys);
type WarmSamples = (Vec<PassRecord>, Vec<SampleRecord>, bool);

#[allow(clippy::disallowed_methods)] // Native-only benchmark example; the wasm build does not execute it.
fn benchmark_clock_now() -> Instant {
    Instant::now()
}

#[allow(clippy::disallowed_methods)] // Timestamp is artifact metadata, outside every measured interval.
fn generated_at_unix_ms() -> Result<u128, SystemTimeError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}

const PINNED_FONT_SHA256: &str = "5a1c559bb539583c8a1fd99d1c5b9491e5e14478c9cd2bd0970d5c3096cc9ef8";
const PINNED_FONTS_REVISION: &str = "b076417b5880bd4154bd22f00695b1d28c6bea83";
const FONT_FAMILY: &str = "gridcraft-benchmark-required-thai-face";
const MAIN_SEED: u64 = 0x4752_4944_4352_4146;
const STRESS_SEED: u64 = 0x5448_4149_5f53_5452;
const MAIN_CELL_COUNT: usize = 500;
const MAIN_COLUMNS: usize = 20;
const MAIN_ROWS: usize = 25;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Config {
    zoom_pct: u16,
    dpi: u8,
}

impl Config {
    fn scale(self) -> f32 {
        f32::from(self.dpi)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CellInput {
    id: u32,
    row: u32,
    col: u32,
    prefix: String,
    text: String,
    horizontal: String,
    vertical: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkloadDescription {
    name: String,
    generator: String,
    seed: u64,
    rows: usize,
    columns: usize,
    expected_cells: usize,
    unique_texts: usize,
    min_unicode_scalars: usize,
    max_unicode_scalars: usize,
    average_unicode_scalars: f64,
    thai_unicode_scalars: usize,
    thai_cells: usize,
    text_and_style_sha256: String,
    horizontal_alignments: Vec<String>,
    vertical_alignments: Vec<String>,
    wrap_enabled: bool,
    rotation_degrees: i16,
    column_width_pixels: f64,
    row_height_pixels: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PassRecord {
    elapsed_ns: Option<u128>,
    unique_rendered_cell_count: usize,
    rendered_cell_ids: Vec<u32>,
    coverage_sha256: String,
    production_grid_rect: [f32; 4],
    production_cells_rect: [f32; 4],
    workload_cells_bounds: [f32; 4],
    workload_cell_geometry_sha256: String,
    matching_text_shape_count: usize,
    reused_first_line_galleys: usize,
    eligible_adapter: Option<thai_benchmark::AdapterCounters>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SampleRecord {
    sample_index: usize,
    first_pass_ns: u128,
    elapsed_through_last_pass_ns: u128,
    pass_count: usize,
    settled: bool,
    passes: Vec<PassRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ConfigResult {
    config: Config,
    viewport_points: [f32; 2],
    cold_definition: String,
    warmup_ready: bool,
    cold_samples: Vec<SampleRecord>,
    warmup_passes: Vec<PassRecord>,
    warm_samples: Vec<SampleRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkloadResult {
    workload: WorkloadDescription,
    configurations: Vec<ConfigResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FontProvenance {
    craft_fonts_dir: String,
    checkout_revision: String,
    relative_path: String,
    sha256: String,
    byte_length: usize,
    thai_glyphs_verified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RunArtifact {
    schema_version: u32,
    revision: String,
    source_patch_sha256: String,
    source_manifest_sha256: String,
    generated_at_unix_ms: u128,
    os: String,
    architecture: String,
    rustc_version: String,
    cargo_version: String,
    profile: String,
    build_features: Vec<String>,
    rustflags: String,
    diagnostic_feature_enabled: bool,
    thai_adapter_expected: bool,
    font: FontProvenance,
    workloads: Vec<WorkloadResult>,
}

#[derive(Clone, Debug)]
struct Options {
    output: PathBuf,
    revision: String,
    source_patch_sha256: String,
    source_manifest_sha256: String,
    cold_samples: usize,
    warm_samples: usize,
    configs: Vec<Config>,
    run_stress: bool,
    thai_adapter_expected: bool,
}

#[derive(Clone, Debug)]
struct Workload {
    description: WorkloadDescription,
    cells: Vec<CellInput>,
}

#[derive(Default)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn command_version(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|output| !output.is_empty())
        .unwrap_or_else(|| format!("{program} version unavailable"))
}

fn font_path() -> Result<(PathBuf, String, String), Box<dyn Error>> {
    let root = std::env::var_os("CRAFT_FONTS_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "CRAFT_FONTS_DIR is required for this Thai geometry benchmark"))?;
    let path = root.join("fonts/noto-sans-thai/NotoSansThai.ttf");
    let bytes = fs::read(&path).map_err(|error| {
        io::Error::new(error.kind(), format!("required pinned Noto Sans Thai font could not be read at {}: {error}", path.display()))
    })?;
    let hash = sha256(&bytes);
    if hash != PINNED_FONT_SHA256 {
        return Err(format!("Noto Sans Thai SHA-256 mismatch at {}: expected {PINNED_FONT_SHA256}, found {hash}", path.display()).into());
    }
    let root = fs::canonicalize(root)?;
    let git_head = Command::new("git").arg("-C").arg(&root).args(["rev-parse", "HEAD"]).output()?;
    if !git_head.status.success() {
        return Err(format!("could not verify craft-fonts checkout revision at {}", root.display()).into());
    }
    let revision = String::from_utf8(git_head.stdout)?.trim().to_string();
    if revision != PINNED_FONTS_REVISION {
        return Err(format!("craft-fonts checkout mismatch at {}: expected {PINNED_FONTS_REVISION}, found {revision}", root.display()).into());
    }
    Ok((path, root.display().to_string(), revision))
}

fn fonts_with_required_thai_face(bytes: Vec<u8>) -> FontDefinitions {
    let mut fonts = theme::pinned_font_definitions();
    let key = "NotoSansThai".to_string();
    fonts.font_data.insert(key.clone(), Arc::new(FontData::from_owned(bytes)));
    for family in [
        FontFamily::Proportional,
        FontFamily::Name(theme::CELL.into()),
        FontFamily::Name(theme::CELL_BOLD.into()),
        FontFamily::Name(theme::UI.into()),
    ] {
        let chain = fonts.families.entry(family).or_default();
        if !chain.contains(&key) {
            chain.insert(0, key.clone());
        }
    }
    fonts.families.insert(FontFamily::Name(FONT_FAMILY.into()), vec![key]);
    fonts
}

fn verify_font_coverage(ctx: &egui::Context, workload: &Workload) -> Result<(), Box<dyn Error>> {
    let font = FontId::new(16.0, FontFamily::Name(FONT_FAMILY.into()));
    let mut missing = BTreeSet::new();
    let chars = workload.cells.iter().flat_map(|cell| cell.text.chars()).collect::<BTreeSet<_>>();
    let _output = ctx.run_ui(egui::RawInput::default(), |ui| {
        ui.fonts_mut(|fonts| {
            for &ch in &chars {
                if !fonts.has_glyph(&font, ch) {
                    missing.insert(ch);
                }
            }
        });
    });
    if !missing.is_empty() {
        return Err(format!("the SHA-256-verified Thai-only face lacks workload glyphs: {missing:?}").into());
    }
    Ok(())
}

const THAI_WORDS: &[&str] = &[
    "รายการ",
    "ยอดขาย",
    "ข้อมูล",
    "สินค้า",
    "ราคา",
    "จำนวน",
    "ลูกค้า",
    "จังหวัด",
    "ใบสั่งซื้อ",
    "จัดส่ง",
    "ชำระเงิน",
    "ส่วนลด",
    "ภาษี",
    "รายงาน",
    "เดือน",
    "ผลรวม",
    "ค่าใช้จ่าย",
    "กำไร",
    "สต็อก",
    "คลังสินค้า",
    "พนักงาน",
    "บริษัท",
    "วันทำการ",
    "สถานะ",
    "ตรวจสอบ",
    "ปรับปรุง",
    "เปรียบเทียบ",
    "รายละเอียด",
    "คำสั่งซื้อ",
    "หมายเหตุ",
    "แผนก",
    "ประจำปี",
    "บริการ",
    "คุณภาพ",
    "วิเคราะห์",
    "กระบวนการ",
    "บันทึก",
    "สำเร็จ",
    "กำลังดำเนินการ",
    "ภาคเหนือ",
    "ภาคกลาง",
    "ภาคใต้",
    "เชียงใหม่",
    "กรุงเทพมหานคร",
    "ขอนแก่น",
    "นครราชสีมา",
    "ระยอง",
    "อุบลราชธานี",
    "ปริมาณ",
    "ต้นทุน",
    "ยอดคงเหลือ",
    "ประเมินผล",
    "แนวโน้ม",
    "ตรวจรับ",
    "เชื่อมโยง",
    "ปรับค่า",
    "สรุปผล",
    "ประสิทธิภาพ",
    "กำหนดการ",
    "ผู้รับผิดชอบ",
];

fn text_for(seed: u64, id: usize, scalar_target: usize) -> String {
    let prefix = format!("T{id:03} ");
    let mut chars: Vec<char> = prefix.chars().collect();
    let mut random = SplitMix64::new(seed ^ (id as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    while chars.len() < scalar_target {
        let index = (random.next() as usize) % THAI_WORDS.len();
        if let Some(word) = THAI_WORDS.get(index) {
            chars.push(' ');
            chars.extend(word.chars());
        }
    }
    chars.truncate(scalar_target);
    chars.into_iter().collect()
}

fn make_workload(name: &str, seed: u64, count: usize, shape: [usize; 2], target_scalars: usize, cell_size: [f64; 2]) -> Workload {
    let [rows, columns] = shape;
    let [width_px, height_px] = cell_size;
    let horizontal = ["General", "Left", "Center", "Right"];
    let vertical = ["Top", "Center", "Bottom"];
    let mut cells = Vec::with_capacity(count);
    let mut canonical = Vec::new();
    let mut scalar_total = 0usize;
    let mut thai_scalar_total = 0usize;
    let mut thai_cell_count = 0usize;
    let mut min_scalars = usize::MAX;
    let mut max_scalars = 0usize;
    for id in 0..count {
        let row = id / columns;
        let col = id % columns;
        let prefix = format!("T{id:03} ");
        let text = text_for(seed, id, target_scalars);
        let h = if name == "main" { horizontal[col % horizontal.len()] } else { "Left" };
        let v = if name == "main" { vertical[row % vertical.len()] } else { "Top" };
        let scalar_count = text.chars().count();
        let thai_scalar_count = text.chars().filter(|ch| ('\u{0e00}'..='\u{0e7f}').contains(ch)).count();
        if thai_scalar_count > 0 {
            thai_cell_count = thai_cell_count.saturating_add(1);
            thai_scalar_total = thai_scalar_total.saturating_add(thai_scalar_count);
        }
        scalar_total = scalar_total.saturating_add(scalar_count);
        min_scalars = min_scalars.min(scalar_count);
        max_scalars = max_scalars.max(scalar_count);
        canonical.extend_from_slice(&(id as u32).to_le_bytes());
        canonical.extend_from_slice(&(row as u32).to_le_bytes());
        canonical.extend_from_slice(&(col as u32).to_le_bytes());
        canonical.extend_from_slice(text.as_bytes());
        canonical.extend_from_slice(h.as_bytes());
        canonical.extend_from_slice(v.as_bytes());
        cells.push(CellInput { id: id as u32, row: row as u32, col: col as u32, prefix, text, horizontal: h.to_string(), vertical: v.to_string() });
    }
    canonical.extend_from_slice(&seed.to_le_bytes());
    canonical.extend_from_slice(&(width_px.to_bits()).to_le_bytes());
    canonical.extend_from_slice(&(height_px.to_bits()).to_le_bytes());
    canonical.extend_from_slice(&[1, 0]); // wrap enabled, rotation zero
    let unique_texts = cells.iter().map(|cell| &cell.text).collect::<BTreeSet<_>>().len();
    Workload {
        description: WorkloadDescription {
            name: name.to_string(),
            generator: "SplitMix64 + per-cell seed stream + THAI_WORDS v1 + unique T{id:03} prefix; Unicode-scalar truncation".to_string(),
            seed,
            rows,
            columns,
            expected_cells: count,
            unique_texts,
            min_unicode_scalars: if count == 0 { 0 } else { min_scalars },
            max_unicode_scalars: max_scalars,
            average_unicode_scalars: if count == 0 { 0.0 } else { scalar_total as f64 / count as f64 },
            thai_unicode_scalars: thai_scalar_total,
            thai_cells: thai_cell_count,
            text_and_style_sha256: sha256(&canonical),
            horizontal_alignments: if name == "main" {
                horizontal.iter().map(|value| (*value).to_string()).collect()
            } else {
                vec!["Left".to_string()]
            },
            vertical_alignments: if name == "main" { vertical.iter().map(|value| (*value).to_string()).collect() } else { vec!["Top".to_string()] },
            wrap_enabled: true,
            rotation_degrees: 0,
            column_width_pixels: width_px,
            row_height_pixels: height_px,
        },
        cells,
    }
}

fn column_name(col: usize) -> String {
    let mut n = col + 1;
    let mut output = String::new();
    while n > 0 {
        let digit = (n - 1) % 26;
        output.insert(0, (b'A' + digit as u8) as char);
        n = (n - 1) / 26;
    }
    output
}

fn build_session(workload: &Workload, config: Config) -> Result<Session, Box<dyn Error>> {
    let mut session = Session::new();
    session.new_workbook();
    let row_count = workload.description.rows.max(1);
    let col_count = workload.description.columns.max(1);
    let last_col = column_name(col_count - 1);
    session.execute("home.columnWidth", json!({"cols": format!("A:{last_col}"), "width": workload.description.column_width_pixels}))?;
    session.execute("home.rowHeight", json!({"rows": format!("1:{row_count}"), "height": workload.description.row_height_pixels}))?;
    for cell in &workload.cells {
        let address = format!("{}{}", column_name(cell.col as usize), cell.row + 1);
        session.execute("cell.set", json!({"cell": address, "input": cell.text}))?;
    }
    if workload.description.name == "main" {
        for col in 0..col_count {
            let col_name = column_name(col);
            let h = ["General", "Left", "Center", "Right"][col % 4];
            session.execute(
                "home.formatCells",
                json!({"range": format!("{col_name}1:{col_name}{row_count}"), "style": {"align": {"h": h, "wrap": true, "rotation": 0}}}),
            )?;
        }
    } else {
        let col_range = format!("A1:{last_col}{row_count}");
        session.execute("home.formatCells", json!({"range": col_range, "style": {"align": {"h": "Left", "wrap": true, "rotation": 0}}}))?;
    }
    for row in 0..row_count {
        let v = if workload.description.name == "main" { ["Top", "Center", "Bottom"][row % 3] } else { "Top" };
        session.execute(
            "home.formatCells",
            json!({"range": format!("A{}:{last_col}{}", row + 1, row + 1), "style": {"align": {"v": v, "wrap": true, "rotation": 0}}}),
        )?;
    }
    session.execute("view.zoom", json!({"percent": config.zoom_pct}))?;
    Ok(session)
}

fn viewport_points(config: Config, workload: &Workload) -> [f32; 2] {
    let z = (f32::from(config.zoom_pct) / 100.0) * 0.8;
    [
        workload.description.columns as f32 * workload.description.column_width_pixels as f32 * z + 800.0,
        workload.description.rows as f32 * workload.description.row_height_pixels as f32 * z + 700.0,
    ]
}

fn build_harness(app: SheetApp, config: Config, workload: &Workload, fonts: FontDefinitions) -> (Harness, Arc<AtomicBool>) {
    let draw_grid = Arc::new(AtomicBool::new(false));
    let visible = Arc::clone(&draw_grid);
    let size = viewport_points(config, workload);
    let mut app = app;
    app.use_prepared_fonts();
    let harness = egui_kittest::Harness::builder()
        .with_size(vec2(size[0], size[1]))
        .with_pixels_per_point(config.scale())
        .with_context_setup(move |ctx| {
            theme::install_fonts(ctx, fonts.clone());
            theme::apply(ctx, false);
        })
        .with_accessibility_check(false)
        .build_ui_state(
            move |ui, app: &mut SheetApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                if visible.load(Ordering::Acquire) {
                    // This is the same public production grid entry point called from
                    // `SheetApp::ui`'s CentralPanel. It keeps the test viewport dedicated to
                    // grid geometry instead of measuring ribbon/chrome layout as well.
                    gridcraft_ui_egui::grid::show(app, ui);
                }
            },
            app,
        );
    (harness, draw_grid)
}

fn verify_geometry_and_coverage(harness: &Harness, workload: &Workload) -> Result<GeometryCoverage, Box<dyn Error>> {
    let app = harness.state();
    let document = app.session.active().ok_or_else(|| io::Error::other("benchmark workbook missing after production renderer pass"))?;
    let sheet = document.wb.active().ok_or_else(|| io::Error::other("benchmark sheet missing after production renderer pass"))?;
    let geo = app.grid.geo.as_ref().ok_or_else(|| io::Error::other("production GridState.geo was not populated"))?;
    let mut geometry_bytes = Vec::with_capacity(workload.cells.len().saturating_mul(20));
    let mut workload_bounds: Option<egui::Rect> = None;
    for cell in &workload.cells {
        let cell_ref = CellRef::new(cell.row, cell.col);
        let rect = geo.screen_cell_rect(sheet, cell_ref);
        if rect.left() < geo.cells.left() || rect.top() < geo.cells.top() || rect.right() > geo.cells.right() || rect.bottom() > geo.cells.bottom() {
            return Err(
                format!("expected visible cell {} lies outside production viewport: cell={rect:?}, viewport={:?}", cell.prefix, geo.cells).into()
            );
        }
        geometry_bytes.extend_from_slice(&cell.id.to_le_bytes());
        for value in [rect.left(), rect.top(), rect.right(), rect.bottom()] {
            geometry_bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        workload_bounds = Some(workload_bounds.map_or(rect, |bounds| egui::Rect::from_min_max(bounds.min.min(rect.min), bounds.max.max(rect.max))));
    }

    let mut covered = BTreeSet::new();
    let mut first_lines = BTreeMap::new();
    let mut matching_shapes = 0usize;
    for clipped in &harness.output().shapes {
        if let Shape::Text(text) = &clipped.shape {
            let line = &text.galley.job.text;
            let Some(prefix) = line.get(..5) else {
                continue;
            };
            let prefix_bytes = prefix.as_bytes();
            if prefix_bytes.first() != Some(&b'T')
                || prefix_bytes.get(4) != Some(&b' ')
                || !prefix_bytes.get(1..4).is_some_and(|digits| digits.iter().all(u8::is_ascii_digit))
            {
                continue;
            }
            let cell_id = prefix.get(1..4).and_then(|digits| digits.parse::<usize>().ok());
            let Some(cell) = cell_id.and_then(|id| workload.cells.get(id)).filter(|cell| line.starts_with(&cell.prefix)) else {
                continue;
            };
            matching_shapes = matching_shapes.saturating_add(1);
            let cell_ref = CellRef::new(cell.row, cell.col);
            let rect = geo.screen_cell_rect(sheet, cell_ref);
            let clip = clipped.clip_rect.intersect(rect);
            if clip.width() <= 0.0 || clip.height() <= 0.0 {
                return Err(format!(
                    "rendered prefix {} has no overlap with its real Grid Geo cell clip: shape clip={:?}, cell={rect:?}",
                    cell.prefix, clipped.clip_rect
                )
                .into());
            }
            if workload.description.name == "main" {
                if !rect.expand(0.5).contains(text.pos) {
                    return Err(
                        format!("rendered prefix {} starts outside its real Grid Geo cell: pos={:?}, cell={rect:?}", cell.prefix, text.pos).into()
                    );
                }
                let hit = geo.cell_at(sheet, text.pos);
                if hit != cell_ref {
                    return Err(format!("production Geo::cell_at maps rendered prefix {} to {hit:?}, expected {cell_ref:?}", cell.prefix).into());
                }
            } else if clip.left() < rect.left() || clip.top() < rect.top() || clip.right() > rect.right() || clip.bottom() > rect.bottom() {
                return Err(
                    format!("stress prefix {} is not clipped to its actual production cell rect: clip={clip:?}, cell={rect:?}", cell.prefix).into()
                );
            }
            covered.insert(cell.id);
            first_lines.insert(cell.id, Arc::clone(&text.galley));
        }
    }
    let ids = covered.into_iter().collect::<Vec<_>>();
    if ids.len() != workload.cells.len() {
        let missing = workload.cells.iter().filter(|cell| !ids.contains(&cell.id)).map(|cell| cell.prefix.as_str()).collect::<Vec<_>>();
        return Err(
            format!("production renderer covered {} of {} expected committed cells; missing {missing:?}", ids.len(), workload.cells.len()).into()
        );
    }
    let expected_ids = (0..workload.cells.len()).map(|id| id as u32).collect::<Vec<_>>();
    if ids != expected_ids {
        return Err("production rendered-cell ids differ from the deterministic expected set".into());
    }
    let mut coverage_bytes = Vec::with_capacity(ids.len().saturating_mul(4));
    for id in &ids {
        coverage_bytes.extend_from_slice(&id.to_le_bytes());
    }
    let workload_bounds = workload_bounds.ok_or_else(|| io::Error::other("production Grid Geo returned no workload geometry"))?;
    let rect_values = |rect: egui::Rect| [rect.left(), rect.top(), rect.right(), rect.bottom()];
    Ok((
        ids,
        sha256(&coverage_bytes),
        matching_shapes,
        first_lines,
        rect_values(geo.rect),
        rect_values(geo.cells),
        rect_values(workload_bounds),
        sha256(&geometry_bytes),
    ))
}

fn run_pass(
    harness: &mut Harness,
    workload: &Workload,
    timed: bool,
    previous: Option<&BTreeMap<u32, Arc<egui::Galley>>>,
) -> Result<PassEvidence, Box<dyn Error>> {
    let start = benchmark_clock_now();
    harness.step();
    let elapsed_ns = timed.then(|| start.elapsed().as_nanos());
    let (
        rendered_cell_ids,
        coverage_sha256,
        matching_text_shape_count,
        lines,
        production_grid_rect,
        production_cells_rect,
        workload_cells_bounds,
        workload_cell_geometry_sha256,
    ) = verify_geometry_and_coverage(harness, workload)?;
    let reused_first_line_galleys =
        previous.map_or(0, |old| lines.iter().filter(|(id, galley)| old.get(id).is_some_and(|prior| Arc::ptr_eq(prior, galley))).count());
    let diagnostic = thai_benchmark::snapshot(&harness.ctx).adapter;
    let pass = PassRecord {
        elapsed_ns,
        unique_rendered_cell_count: rendered_cell_ids.len(),
        rendered_cell_ids,
        coverage_sha256,
        production_grid_rect,
        production_cells_rect,
        workload_cells_bounds,
        workload_cell_geometry_sha256,
        matching_text_shape_count,
        reused_first_line_galleys,
        eligible_adapter: diagnostic,
    };
    Ok((pass, lines))
}

fn assert_adapter_expectation(pass: &PassRecord, expected: usize, stage: &str, adapter_expected: bool) -> Result<(), Box<dyn Error>> {
    let diagnostics_expected = adapter_expected && cfg!(feature = "thai-benchmark-diagnostics");
    match (diagnostics_expected, pass.eligible_adapter) {
        (true, Some(adapter)) => {
            let actual = adapter.pass.map_or(0, |counters| counters.requests);
            if actual != expected {
                return Err(format!("Thai adapter recorded {actual} production requests in {stage}; expected {expected}").into());
            }
        }
        (true, None) => return Err(format!("Thai adapter diagnostics are missing during {stage}").into()),
        (false, Some(_)) => return Err(format!("unexpected Thai adapter diagnostics appeared during {stage}").into()),
        (false, None) => {}
    }
    Ok(())
}

fn run_cold_sample(
    index: usize,
    mut app: SheetApp,
    config: Config,
    workload: &Workload,
    fonts: FontDefinitions,
    adapter_expected: bool,
) -> Result<(SampleRecord, SheetApp), Box<dyn Error>> {
    app.session.text_width = None;
    let (mut harness, draw_grid) = build_harness(app, config, workload, fonts);
    if !harness.state().fonts_ready {
        return Err("SheetApp startup/font gate was not ready before cold timing".into());
    }
    verify_font_coverage(&harness.ctx, workload)?;
    thai_benchmark::clear_context(&harness.ctx);
    draw_grid.store(true, Ordering::Release);
    let mut passes = Vec::new();
    let mut previous = None;
    let mut total_ns = 0u128;
    let mut settled = false;
    for _ in 0..300 {
        let (record, lines) = run_pass(&mut harness, workload, true, previous.as_ref())?;
        let elapsed = record.elapsed_ns.unwrap_or(0);
        if passes.is_empty() {
            assert_adapter_expectation(&record, workload.cells.len(), "cold first pass", adapter_expected)?;
            if let Some(adapter) = record.eligible_adapter {
                let counters = adapter.pass.ok_or_else(|| io::Error::other("Thai cold counters missing"))?;
                if counters.cache_hits != 0
                    || counters.cache_misses != workload.cells.len()
                    || counters.segmentations == 0
                    || counters.measurements == 0
                {
                    return Err(format!("cold sample did not begin with an empty Thai plan cache and real layout work: {counters:?}").into());
                }
            }
        }
        total_ns = total_ns.saturating_add(elapsed);
        settled = record.eligible_adapter.is_none_or(|adapter| adapter.pass.is_none_or(|counters| !counters.repaint_retry));
        passes.push(record);
        previous = Some(lines);
        if settled {
            break;
        }
    }
    let first_pass_ns = passes.first().and_then(|pass| pass.elapsed_ns).unwrap_or(0);
    let sample =
        SampleRecord { sample_index: index, first_pass_ns, elapsed_through_last_pass_ns: total_ns, pass_count: passes.len(), settled, passes };
    let mut app = harness.into_state();
    app.session.text_width = None;
    Ok((sample, app))
}

fn run_warm_samples(
    mut app: SheetApp,
    config: Config,
    workload: &Workload,
    fonts: FontDefinitions,
    sample_count: usize,
    adapter_expected: bool,
) -> Result<WarmSamples, Box<dyn Error>> {
    app.session.text_width = None;
    let (mut harness, draw_grid) = build_harness(app, config, workload, fonts);
    if !harness.state().fonts_ready {
        return Err("SheetApp startup/font gate was not ready before warmup".into());
    }
    verify_font_coverage(&harness.ctx, workload)?;
    thai_benchmark::clear_context(&harness.ctx);
    draw_grid.store(true, Ordering::Release);

    let mut warmup_passes = Vec::new();
    let mut previous = None;
    let mut settled = false;
    for _ in 0..300 {
        let (record, lines) = run_pass(&mut harness, workload, false, previous.as_ref())?;
        assert_adapter_expectation(&record, workload.cells.len(), "warmup renderer pass", adapter_expected)?;
        settled = record.eligible_adapter.is_none_or(|adapter| adapter.pass.is_none_or(|counters| !counters.repaint_retry));
        previous = Some(lines);
        warmup_passes.push(record);
        if settled {
            break;
        }
    }
    let mut warmup_ready = settled;

    // Readiness is established from actual production counters and output, before samples start.
    let mut stable_frames = 0usize;
    for _ in 0..300 {
        let (record, lines) = run_pass(&mut harness, workload, false, previous.as_ref())?;
        assert_adapter_expectation(&record, workload.cells.len(), "steady warmup renderer pass", adapter_expected)?;
        let mut frame_ready = true;
        if let Some(adapter) = record.eligible_adapter {
            let counters = adapter.pass.ok_or_else(|| io::Error::other("warm readiness counters missing"))?;
            if counters.requests != workload.cells.len()
                || counters.cache_hits != workload.cells.len()
                || counters.segmentations != 0
                || counters.measurements != 0
                || counters.measured_bytes != 0
                || counters.exhausted
                || counters.repaint_retry
                || adapter.context_cache_entries != workload.cells.len()
            {
                frame_ready = false;
            }
        }
        if record.reused_first_line_galleys != workload.cells.len() {
            frame_ready = false;
        }
        stable_frames = if frame_ready { stable_frames.saturating_add(1) } else { 0 };
        previous = Some(lines);
        warmup_passes.push(record);
        if stable_frames == 3 {
            break;
        }
    }
    warmup_ready &= stable_frames == 3;

    let mut samples = Vec::with_capacity(sample_count);
    let timed_sample_count = if warmup_ready { sample_count } else { 0 };
    for index in 0..timed_sample_count {
        let (pass, lines) = run_pass(&mut harness, workload, true, previous.as_ref())?;
        assert_adapter_expectation(&pass, workload.cells.len(), "timed warm renderer pass", adapter_expected)?;
        if let Some(adapter) = pass.eligible_adapter {
            let counters = adapter.pass.ok_or_else(|| io::Error::other("timed warm counters missing"))?;
            if counters.requests != workload.cells.len() {
                return Err(
                    format!("timed warm renderer recorded {} actual adapter requests; expected {}", counters.requests, workload.cells.len()).into()
                );
            }
        }
        let elapsed = pass.elapsed_ns.unwrap_or(0);
        let settled = pass.eligible_adapter.is_none_or(|adapter| adapter.pass.is_none_or(|counters| !counters.repaint_retry));
        samples.push(SampleRecord {
            sample_index: index,
            first_pass_ns: elapsed,
            elapsed_through_last_pass_ns: elapsed,
            pass_count: 1,
            settled,
            passes: vec![pass],
        });
        previous = Some(lines);
    }
    Ok((warmup_passes, samples, warmup_ready))
}

fn configurations(only: Option<&str>) -> Result<Vec<Config>, Box<dyn Error>> {
    let all = [
        Config { zoom_pct: 50, dpi: 1 },
        Config { zoom_pct: 50, dpi: 2 },
        Config { zoom_pct: 100, dpi: 1 },
        Config { zoom_pct: 100, dpi: 2 },
        Config { zoom_pct: 200, dpi: 1 },
        Config { zoom_pct: 200, dpi: 2 },
    ];
    if let Some(key) = only.filter(|value| *value != "all") {
        let config = all.iter().copied().find(|config| format!("{}-{}", config.zoom_pct, config.dpi) == key);
        return config
            .map(|found| vec![found])
            .ok_or_else(|| format!("unknown configuration `{key}`; use all, 50-1, 50-2, 100-1, 100-2, 200-1, or 200-2").into());
    }
    Ok(all.to_vec())
}

fn parse_options() -> Result<Options, Box<dyn Error>> {
    let mut output = None;
    let mut revision = None;
    let mut patch_hash = None;
    let mut source_manifest_hash = None;
    let mut cold_samples = 30usize;
    let mut warm_samples = 100usize;
    let mut config_arg = "all".to_string();
    let mut run_stress = true;
    let mut thai_adapter_expected = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().ok_or_else(|| format!("missing value after {arg}"))?;
        match arg.as_str() {
            "--out" => output = Some(PathBuf::from(value)),
            "--revision" => revision = Some(value),
            "--patch-sha256" => patch_hash = Some(value),
            "--source-manifest-sha256" => source_manifest_hash = Some(value),
            "--cold" => cold_samples = value.parse()?,
            "--warm" => warm_samples = value.parse()?,
            "--config" => config_arg = value,
            "--stress" => run_stress = value.parse()?,
            "--thai-adapter" => thai_adapter_expected = Some(value.parse::<bool>()?),
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    let output = output.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "required argument --out"))?;
    let revision = revision.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "required argument --revision"))?;
    let source_patch_sha256 = patch_hash.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "required argument --patch-sha256"))?;
    let source_manifest_sha256 =
        source_manifest_hash.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "required argument --source-manifest-sha256"))?;
    let thai_adapter_expected =
        thai_adapter_expected.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "required argument --thai-adapter true|false"))?;
    let configs = configurations(Some(&config_arg))?;
    Ok(Options {
        output,
        revision,
        source_patch_sha256,
        source_manifest_sha256,
        cold_samples,
        warm_samples,
        configs,
        run_stress,
        thai_adapter_expected,
    })
}

fn benchmark_workload(
    workload: Workload,
    configs: &[Config],
    cold_count: usize,
    warm_count: usize,
    fonts: &FontDefinitions,
    adapter_expected: bool,
) -> Result<WorkloadResult, Box<dyn Error>> {
    let mut results = Vec::with_capacity(configs.len());
    for config in configs {
        let initial_app = SheetApp::new(build_session(&workload, *config)?, Default::default());
        let mut app = initial_app;
        let mut cold_samples = Vec::with_capacity(cold_count);
        for index in 0..cold_count {
            let (sample, next_app) = run_cold_sample(index, app, *config, &workload, fonts.clone(), adapter_expected)?;
            app = next_app;
            cold_samples.push(sample);
        }
        if cold_samples.is_empty() {
            app.session.text_width = None;
        }
        let (warmup_passes, warm_samples, warmup_ready) = run_warm_samples(app, *config, &workload, fonts.clone(), warm_count, adapter_expected)?;
        results.push(ConfigResult {
            config: *config,
            viewport_points: viewport_points(*config, &workload),
            cold_definition: "Fresh egui Context and empty Context-scoped Thai plan cache for each cold sample. The pinned font face and glyph coverage are verified before timing; no workload grid paint or workload galley layout occurs before the first timed pass. egui font/glyph activation is warm, while the Thai plan cache and workload galley cache are fresh. `harness.step()` calls SheetApp::logic and production `grid::show`; timing begins before the first layout call and ends after that renderer pass. Cold total sums timed renderer passes through the first pass without a Thai retry repaint, or through the 300-pass cap when unsettled. The dedicated viewport omits ribbon/chrome layout. GPU, screenshot, PNG encoding, and I/O are excluded.".to_string(),
            warmup_ready,
            cold_samples,
            warmup_passes,
            warm_samples,
        });
    }
    Ok(WorkloadResult { workload: workload.description, configurations: results })
}

fn run() -> Result<(), Box<dyn Error>> {
    let options = parse_options()?;
    if options.cold_samples == 0 && options.warm_samples == 0 {
        return Err("at least one cold or warm sample is required".into());
    }
    let (path, fonts_root, fonts_revision) = font_path()?;
    let bytes = fs::read(&path)?;
    let font_hash = sha256(&bytes);
    if font_hash != PINNED_FONT_SHA256 {
        return Err(format!("pinned Thai font changed before workload construction: expected {PINNED_FONT_SHA256}, found {font_hash}").into());
    }
    let font_len = bytes.len();
    let fonts = fonts_with_required_thai_face(bytes);
    let main = make_workload("main", MAIN_SEED, MAIN_CELL_COUNT, [MAIN_ROWS, MAIN_COLUMNS], 80, [160.0, 150.0]);
    if main.description.unique_texts != MAIN_CELL_COUNT
        || main.description.average_unicode_scalars != 80.0
        || main.description.thai_cells != MAIN_CELL_COUNT
        || main.description.thai_unicode_scalars == 0
    {
        return Err("deterministic main workload failed uniqueness or scalar-count readiness".into());
    }
    let mut workloads =
        vec![benchmark_workload(main, &options.configs, options.cold_samples, options.warm_samples, &fonts, options.thai_adapter_expected)?];
    if options.run_stress {
        let stress = make_workload("stress-long-text-narrow-columns", STRESS_SEED, 5, [5, 1], 8_000, [42.0, 60.0]);
        workloads.push(benchmark_workload(stress, &[Config { zoom_pct: 100, dpi: 1 }], 3, 10, &fonts, options.thai_adapter_expected)?);
    }
    let generated_at_unix_ms = generated_at_unix_ms()?;
    let artifact = RunArtifact {
        schema_version: 1,
        revision: options.revision.clone(),
        source_patch_sha256: options.source_patch_sha256,
        source_manifest_sha256: options.source_manifest_sha256,
        generated_at_unix_ms,
        os: std::env::consts::OS.to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        rustc_version: command_version("rustc", &["-Vv"]),
        cargo_version: command_version("cargo", &["-V"]),
        profile: "release".to_string(),
        build_features: if cfg!(feature = "thai-benchmark-diagnostics") { vec!["thai-benchmark-diagnostics".to_string()] } else { Vec::new() },
        rustflags: std::env::var("RUSTFLAGS").unwrap_or_default(),
        diagnostic_feature_enabled: cfg!(feature = "thai-benchmark-diagnostics"),
        thai_adapter_expected: options.thai_adapter_expected,
        font: FontProvenance {
            craft_fonts_dir: fonts_root,
            checkout_revision: fonts_revision,
            relative_path: "fonts/noto-sans-thai/NotoSansThai.ttf".to_string(),
            sha256: font_hash,
            byte_length: font_len,
            thai_glyphs_verified: true,
        },
        workloads,
    };
    let parent = options.output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    fs::write(&options.output, serde_json::to_vec_pretty(&artifact)?)?;
    println!("wrote {} ({})", options.output.display(), options.revision);
    Ok(())
}

fn percentile(values: &mut [u128], numerator: usize, denominator: usize) -> Option<u128> {
    if values.is_empty() || denominator == 0 {
        return None;
    }
    values.sort_unstable();
    let rank = values.len().saturating_mul(numerator).div_ceil(denominator).max(1);
    values.get(rank.saturating_sub(1)).copied()
}

fn cold_values(samples: &[SampleRecord], first_pass: bool) -> (Option<u128>, Option<u128>) {
    let mut values =
        samples.iter().map(|sample| if first_pass { sample.first_pass_ns } else { sample.elapsed_through_last_pass_ns }).collect::<Vec<_>>();
    (percentile(&mut values, 50, 100), percentile(&mut values, 95, 100))
}

fn warm_values(samples: &[SampleRecord]) -> (Option<u128>, Option<u128>) {
    let mut values = samples.iter().map(|sample| sample.first_pass_ns).collect::<Vec<_>>();
    (percentile(&mut values, 50, 100), percentile(&mut values, 95, 100))
}

fn fmt_ms(value: Option<u128>) -> String {
    value.map_or_else(|| "N/A".to_string(), |ns| format!("{:.3}", ns as f64 / 1_000_000.0))
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct CounterAggregate {
    measured_passes: usize,
    unique_rendered_cells_min_per_pass: usize,
    unique_rendered_cells_max_per_pass: usize,
    actual_adapter_requests: usize,
    cache_hits: usize,
    cache_misses: usize,
    segmentations: usize,
    candidate_measurements: usize,
    measured_utf8_bytes: usize,
    transient_fallbacks: usize,
    permanent_fallbacks: usize,
    exhausted_passes: usize,
    retry_repaint_passes: usize,
    retry_passes: usize,
    no_progress_passes: usize,
    context_cache_entries_min: usize,
    context_cache_entries_max: usize,
    context_cache_accounted_bytes_min: usize,
    context_cache_accounted_bytes_max: usize,
    global_cache_entries_min: usize,
    global_cache_entries_max: usize,
    global_cache_accounted_bytes_min: usize,
    global_cache_accounted_bytes_max: usize,
}

fn aggregate_counters(samples: &[SampleRecord]) -> Option<CounterAggregate> {
    let records = samples.iter().flat_map(|sample| &sample.passes).collect::<Vec<_>>();
    if records.is_empty() || records.iter().any(|pass| pass.eligible_adapter.is_none()) {
        return None;
    }
    let mut aggregate = CounterAggregate {
        unique_rendered_cells_min_per_pass: usize::MAX,
        context_cache_entries_min: usize::MAX,
        context_cache_accounted_bytes_min: usize::MAX,
        global_cache_entries_min: usize::MAX,
        global_cache_accounted_bytes_min: usize::MAX,
        ..CounterAggregate::default()
    };
    for pass in records {
        aggregate.measured_passes = aggregate.measured_passes.saturating_add(1);
        aggregate.unique_rendered_cells_min_per_pass = aggregate.unique_rendered_cells_min_per_pass.min(pass.unique_rendered_cell_count);
        aggregate.unique_rendered_cells_max_per_pass = aggregate.unique_rendered_cells_max_per_pass.max(pass.unique_rendered_cell_count);
        let Some(adapter) = pass.eligible_adapter else { continue };
        aggregate.context_cache_entries_min = aggregate.context_cache_entries_min.min(adapter.context_cache_entries);
        aggregate.context_cache_entries_max = aggregate.context_cache_entries_max.max(adapter.context_cache_entries);
        aggregate.context_cache_accounted_bytes_min = aggregate.context_cache_accounted_bytes_min.min(adapter.context_cache_accounted_bytes);
        aggregate.context_cache_accounted_bytes_max = aggregate.context_cache_accounted_bytes_max.max(adapter.context_cache_accounted_bytes);
        aggregate.global_cache_entries_min = aggregate.global_cache_entries_min.min(adapter.global_cache_entries);
        aggregate.global_cache_entries_max = aggregate.global_cache_entries_max.max(adapter.global_cache_entries);
        aggregate.global_cache_accounted_bytes_min = aggregate.global_cache_accounted_bytes_min.min(adapter.global_cache_accounted_bytes);
        aggregate.global_cache_accounted_bytes_max = aggregate.global_cache_accounted_bytes_max.max(adapter.global_cache_accounted_bytes);
        let Some(counters) = adapter.pass else { continue };
        aggregate.actual_adapter_requests = aggregate.actual_adapter_requests.saturating_add(counters.requests);
        aggregate.cache_hits = aggregate.cache_hits.saturating_add(counters.cache_hits);
        aggregate.cache_misses = aggregate.cache_misses.saturating_add(counters.cache_misses);
        aggregate.segmentations = aggregate.segmentations.saturating_add(counters.segmentations);
        aggregate.candidate_measurements = aggregate.candidate_measurements.saturating_add(counters.measurements);
        aggregate.measured_utf8_bytes = aggregate.measured_utf8_bytes.saturating_add(counters.measured_bytes);
        aggregate.transient_fallbacks = aggregate.transient_fallbacks.saturating_add(counters.transient_fallbacks);
        aggregate.permanent_fallbacks = aggregate.permanent_fallbacks.saturating_add(counters.permanent_fallbacks);
        aggregate.exhausted_passes = aggregate.exhausted_passes.saturating_add(usize::from(counters.exhausted));
        aggregate.retry_repaint_passes = aggregate.retry_repaint_passes.saturating_add(usize::from(counters.repaint_retry));
        aggregate.retry_passes = aggregate.retry_passes.saturating_add(counters.retry_passes);
        aggregate.no_progress_passes = aggregate.no_progress_passes.saturating_add(counters.no_progress_passes);
    }
    Some(aggregate)
}

fn aggregate_cell_bounds(samples: &[SampleRecord]) -> Option<[f32; 4]> {
    samples.iter().flat_map(|sample| &sample.passes).next().map(|pass| pass.workload_cells_bounds)
}

fn aggregate_geometry_hash(samples: &[SampleRecord]) -> Option<&str> {
    samples.iter().flat_map(|sample| &sample.passes).next().map(|pass| pass.workload_cell_geometry_sha256.as_str())
}

fn shape_count_range(samples: &[SampleRecord]) -> Option<(usize, usize)> {
    let mut counts = samples.iter().flat_map(|sample| &sample.passes).map(|pass| pass.matching_text_shape_count);
    let first = counts.next()?;
    Some(counts.fold((first, first), |(min, max), count| (min.min(count), max.max(count))))
}

fn format_counters(counters: Option<CounterAggregate>) -> String {
    counters.map_or_else(
        || "N/A (base has no Thai-specific counters)".to_string(),
        |c| format!(
            "passes={} visible={}..{} requests={} hits={} misses={} segmentations={} candidates={} bytes={} fallback(transient/permanent)={}/{} exhausted={} retry-repaint-passes={} retry-passes={} no-progress={} context-cache={}..{} entries/{}..{} B global-cache={}..{} entries/{}..{} B",
            c.measured_passes,
            c.unique_rendered_cells_min_per_pass,
            c.unique_rendered_cells_max_per_pass,
            c.actual_adapter_requests,
            c.cache_hits,
            c.cache_misses,
            c.segmentations,
            c.candidate_measurements,
            c.measured_utf8_bytes,
            c.transient_fallbacks,
            c.permanent_fallbacks,
            c.exhausted_passes,
            c.retry_repaint_passes,
            c.retry_passes,
            c.no_progress_passes,
            c.context_cache_entries_min,
            c.context_cache_entries_max,
            c.context_cache_accounted_bytes_min,
            c.context_cache_accounted_bytes_max,
            c.global_cache_entries_min,
            c.global_cache_entries_max,
            c.global_cache_accounted_bytes_min,
            c.global_cache_accounted_bytes_max,
        ),
    )
}

fn summary_report(base: &RunArtifact, head: &RunArtifact) -> Result<(String, serde_json::Value), Box<dyn Error>> {
    if base.thai_adapter_expected || !head.thai_adapter_expected {
        return Err("paired summary requires base f6b0 without Thai adapter and head with Thai adapter".into());
    }
    if base.font.sha256 != head.font.sha256 || base.font.checkout_revision != head.font.checkout_revision {
        return Err("base/head font inputs are not equivalent".into());
    }
    if base.rustc_version != head.rustc_version
        || base.cargo_version != head.cargo_version
        || base.profile != head.profile
        || base.build_features != head.build_features
        || base.rustflags != head.rustflags
        || base.os != head.os
        || base.architecture != head.architecture
    {
        return Err("base/head toolchain, profile/features/flags, or host settings differ".into());
    }
    if !base.diagnostic_feature_enabled || !head.diagnostic_feature_enabled {
        return Err("base/head measurement builds must use the same diagnostics feature setting".into());
    }
    if base.workloads.len() != head.workloads.len() {
        return Err("base/head workload counts differ".into());
    }
    let mut rows = String::new();
    let mut summary_workloads = Vec::new();
    for (base_workload, head_workload) in base.workloads.iter().zip(&head.workloads) {
        if base_workload.workload.text_and_style_sha256 != head_workload.workload.text_and_style_sha256
            || base_workload.workload.generator != head_workload.workload.generator
            || base_workload.workload.expected_cells != head_workload.workload.expected_cells
            || base_workload.workload.average_unicode_scalars != head_workload.workload.average_unicode_scalars
        {
            return Err(format!("base/head workload differs for {}", base_workload.workload.name).into());
        }
        if base_workload.configurations.len() != head_workload.configurations.len() {
            return Err(format!("base/head config count differs for {}", base_workload.workload.name).into());
        }
        rows.push_str(&format!(
            "\n### {} — {} cells, {:.1} Unicode scalars/cell\n\n",
            base_workload.workload.name, base_workload.workload.expected_cells, base_workload.workload.average_unicode_scalars
        ));
        rows.push_str(&format!("Generator `{}`; seed `0x{:016x}`; unique texts {}; Thai-bearing cells {}; Thai Unicode scalars {}; text/style/geometry SHA-256 `{}`.\n\n",
            base_workload.workload.generator, base_workload.workload.seed, base_workload.workload.unique_texts,
            base_workload.workload.thai_cells, base_workload.workload.thai_unicode_scalars, base_workload.workload.text_and_style_sha256));
        rows.push_str("| Zoom / DPI | cold first pass p50/p95 base (ms) | cold first pass p50/p95 head (ms) | cold total through settle/cap p50/p95 base (ms) | cold total through settle/cap p50/p95 head (ms) | warm p50/p95 base (ms) | warm p50/p95 head (ms) | warm p50 delta (ms / %) | warm p95 delta (ms / %) | samples cold/warm base/head; unsettled cold base/head; warmup ready base/head |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
        let mut configs_json = Vec::new();
        let mut geometry_rows = String::new();
        let mut diagnostic_rows = String::new();
        for (base_config, head_config) in base_workload.configurations.iter().zip(&head_workload.configurations) {
            if base_config.config.zoom_pct != head_config.config.zoom_pct || base_config.config.dpi != head_config.config.dpi {
                return Err("base/head configurations do not pair".into());
            }
            if base_config.viewport_points != head_config.viewport_points {
                return Err("base/head viewport sizes differ".into());
            }
            let base_first_geometry = base_config.cold_samples.iter().flat_map(|sample| &sample.passes).next();
            let head_first_geometry = head_config.cold_samples.iter().flat_map(|sample| &sample.passes).next();
            if let (Some(base_pass), Some(head_pass)) = (base_first_geometry, head_first_geometry)
                && (base_pass.rendered_cell_ids != head_pass.rendered_cell_ids
                    || base_pass.unique_rendered_cell_count != base_workload.workload.expected_cells
                    || head_pass.unique_rendered_cell_count != head_workload.workload.expected_cells
                    || base_pass.coverage_sha256 != head_pass.coverage_sha256
                    || base_pass.workload_cell_geometry_sha256 != head_pass.workload_cell_geometry_sha256
                    || base_pass.production_grid_rect != head_pass.production_grid_rect
                    || base_pass.production_cells_rect != head_pass.production_cells_rect
                    || base_pass.workload_cells_bounds != head_pass.workload_cells_bounds)
            {
                return Err(format!(
                    "base/head production coverage or geometry differs for {} at {}%/{}x",
                    base_workload.workload.name, base_config.config.zoom_pct, base_config.config.dpi
                )
                .into());
            }
            let base_cold_counters = aggregate_counters(&base_config.cold_samples);
            let head_cold_counters = aggregate_counters(&head_config.cold_samples);
            let base_warm_counters = aggregate_counters(&base_config.warm_samples);
            let head_warm_counters = aggregate_counters(&head_config.warm_samples);
            if let (Some(base_pass), Some(head_pass)) = (base_first_geometry, head_first_geometry) {
                geometry_rows.push_str(&format!(
                    "| {}% / {}x | {:?} | {:?} | {:?} | {:?} | {} | {} / {} | {:?} / {:?} |\n",
                    base_config.config.zoom_pct,
                    base_config.config.dpi,
                    base_config.viewport_points,
                    base_pass.production_grid_rect,
                    base_pass.production_cells_rect,
                    base_pass.workload_cells_bounds,
                    base_pass.workload_cell_geometry_sha256,
                    base_pass.unique_rendered_cell_count,
                    head_pass.unique_rendered_cell_count,
                    shape_count_range(&base_config.cold_samples),
                    shape_count_range(&head_config.cold_samples),
                ));
            } else {
                geometry_rows.push_str(&format!(
                    "| {}% / {}x | {:?} | N/A | N/A | N/A | N/A | N/A / N/A | N/A / N/A–N/A |\n",
                    base_config.config.zoom_pct, base_config.config.dpi, base_config.viewport_points
                ));
            }
            diagnostic_rows.push_str(&format!(
                "| {} | {}% / {}x | {} | {} | cold settled {}/{}; warmup ready {}; warm unsettled {} |\n",
                base.revision,
                base_config.config.zoom_pct,
                base_config.config.dpi,
                format_counters(base_cold_counters),
                format_counters(base_warm_counters),
                base_config.cold_samples.iter().filter(|s| s.settled).count(),
                base_config.cold_samples.len(),
                base_config.warmup_ready,
                base_config.warm_samples.iter().filter(|s| !s.settled).count()
            ));
            diagnostic_rows.push_str(&format!(
                "| {} | {}% / {}x | {} | {} | cold settled {}/{}; warmup ready {}; warm unsettled {} |\n",
                head.revision,
                head_config.config.zoom_pct,
                head_config.config.dpi,
                format_counters(head_cold_counters),
                format_counters(head_warm_counters),
                head_config.cold_samples.iter().filter(|s| s.settled).count(),
                head_config.cold_samples.len(),
                head_config.warmup_ready,
                head_config.warm_samples.iter().filter(|s| !s.settled).count()
            ));
            let (base_first_50, base_first_95) = cold_values(&base_config.cold_samples, true);
            let (head_first_50, head_first_95) = cold_values(&head_config.cold_samples, true);
            let (base_total_50, base_total_95) = cold_values(&base_config.cold_samples, false);
            let (head_total_50, head_total_95) = cold_values(&head_config.cold_samples, false);
            let (base_warm_50, base_warm_95) = warm_values(&base_config.warm_samples);
            let (head_warm_50, head_warm_95) = warm_values(&head_config.warm_samples);
            let p50_delta_ns = base_warm_50.zip(head_warm_50).map(|(b, h)| b as i128 - h as i128);
            let p50_delta_pct = base_warm_50.zip(head_warm_50).and_then(|(b, h)| (b > 0).then_some((b as f64 - h as f64) / b as f64 * 100.0));
            let p95_delta_ns = base_warm_95.zip(head_warm_95).map(|(b, h)| b as i128 - h as i128);
            let p95_delta_pct = base_warm_95.zip(head_warm_95).and_then(|(b, h)| (b > 0).then_some((b as f64 - h as f64) / b as f64 * 100.0));
            let sample_counts = format!(
                "{}/{} / {}/{}; {}/{}; {}/{}",
                base_config.cold_samples.len(),
                base_config.warm_samples.len(),
                head_config.cold_samples.len(),
                head_config.warm_samples.len(),
                base_config.cold_samples.iter().filter(|s| !s.settled).count(),
                head_config.cold_samples.iter().filter(|s| !s.settled).count(),
                base_config.warmup_ready,
                head_config.warmup_ready,
            );
            rows.push_str(&format!(
                "| {}% / {}x | {}/{} | {}/{} | {}/{} | {}/{} | {}/{} | {}/{} | {} / {} | {} / {} | {} |\n",
                base_config.config.zoom_pct,
                base_config.config.dpi,
                fmt_ms(base_first_50),
                fmt_ms(base_first_95),
                fmt_ms(head_first_50),
                fmt_ms(head_first_95),
                fmt_ms(base_total_50),
                fmt_ms(base_total_95),
                fmt_ms(head_total_50),
                fmt_ms(head_total_95),
                fmt_ms(base_warm_50),
                fmt_ms(base_warm_95),
                fmt_ms(head_warm_50),
                fmt_ms(head_warm_95),
                p50_delta_ns.map_or_else(|| "N/A".to_string(), |delta| format!("{:.3} ms", delta as f64 / 1_000_000.0)),
                p50_delta_pct.map_or_else(|| "N/A".to_string(), |delta| format!("{delta:.2}%")),
                p95_delta_ns.map_or_else(|| "N/A".to_string(), |delta| format!("{:.3} ms", delta as f64 / 1_000_000.0)),
                p95_delta_pct.map_or_else(|| "N/A".to_string(), |delta| format!("{delta:.2}%")),
                sample_counts,
            ));
            configs_json.push(json!({
                "config": base_config.config,
                "viewport_points": base_config.viewport_points,
                "base": {
                    "cold_first_pass_p50_ns": base_first_50,
                    "cold_first_pass_p95_ns": base_first_95,
                    "cold_elapsed_through_last_pass_p50_ns": base_total_50,
                    "cold_elapsed_through_last_pass_p95_ns": base_total_95,
                    "warm_p50_ns": base_warm_50,
                    "warm_p95_ns": base_warm_95,
                    "cold_measured_counters": base_cold_counters,
                    "warm_measured_counters": base_warm_counters,
                    "warmup_ready": base_config.warmup_ready,
                    "cold_samples": base_config.cold_samples.len(),
                    "warm_samples": base_config.warm_samples.len(),
                },
                "head": {
                    "cold_first_pass_p50_ns": head_first_50,
                    "cold_first_pass_p95_ns": head_first_95,
                    "cold_elapsed_through_last_pass_p50_ns": head_total_50,
                    "cold_elapsed_through_last_pass_p95_ns": head_total_95,
                    "warm_p50_ns": head_warm_50,
                    "warm_p95_ns": head_warm_95,
                    "cold_measured_counters": head_cold_counters,
                    "warm_measured_counters": head_warm_counters,
                    "warmup_ready": head_config.warmup_ready,
                    "cold_samples": head_config.cold_samples.len(),
                    "warm_samples": head_config.warm_samples.len(),
                },
                "warm_p50_delta_ns_base_minus_head": p50_delta_ns,
                "warm_p50_delta_percent_of_base": p50_delta_pct,
                "warm_p95_delta_ns_base_minus_head": p95_delta_ns,
                "warm_p95_delta_percent_of_base": p95_delta_pct,
                "base_unsettled_cold_samples": base_config.cold_samples.iter().filter(|s| !s.settled).count(),
                "head_unsettled_cold_samples": head_config.cold_samples.iter().filter(|s| !s.settled).count(),
                "base_unsettled_warm_samples": base_config.warm_samples.iter().filter(|s| !s.settled).count(),
                "head_unsettled_warm_samples": head_config.warm_samples.iter().filter(|s| !s.settled).count(),
                "base_geometry_sha256": aggregate_geometry_hash(&base_config.cold_samples),
                "head_geometry_sha256": aggregate_geometry_hash(&head_config.cold_samples),
                "base_cell_bounds": aggregate_cell_bounds(&base_config.cold_samples),
                "head_cell_bounds": aggregate_cell_bounds(&head_config.cold_samples),
                "base_shape_count_range": shape_count_range(&base_config.cold_samples),
                "head_shape_count_range": shape_count_range(&head_config.cold_samples),
            }));
        }
        rows.push_str("\n#### Production geometry and coverage\n\n");
        rows.push_str("| Zoom / DPI | viewport size (pt) | `Geo.rect` | `Geo.cells` | workload-cell bounds | hash of every production `screen_cell_rect` | unique cell IDs base/head | prefixed renderer text shapes base/head min–max |\n|---|---|---|---|---|---|---:|---:|\n");
        rows.push_str(&geometry_rows);
        rows.push_str("\n#### Measured diagnostics\n\n");
        rows.push_str("Each aggregate covers every raw cold pass or timed warm pass in that row. Cache ranges are Context-specific; process-global totals are shown separately. Base Thai-adapter measurements are N/A because f6b0 has no adapter.\n\n");
        rows.push_str("| Revision | Zoom / DPI | cold passes: actual counters | timed warm passes: actual counters | settle/readiness |\n|---|---:|---|---|---|\n");
        rows.push_str(&diagnostic_rows);
        summary_workloads.push(json!({"name": base_workload.workload.name, "workload_sha256": base_workload.workload.text_and_style_sha256, "configurations": configs_json}));
    }
    let report = format!(
        "# TASK 4 Thai wrapping benchmark\n\n\
         - Base: `{}`; tracked patch SHA-256 `{}`; complete measurement-source manifest SHA-256 `{}`.\n\
         - Head: `{}`; tracked patch SHA-256 `{}`; complete measurement-source manifest SHA-256 `{}`.\n\
         - Thai adapter counters: base N/A (baseline has no Thai adapter); head measured through `thai-benchmark-diagnostics`.\n\
         - Font: `{}` from craft-fonts `{}`, SHA-256 `{}` ({} bytes).\n\
         - Toolchain/profile: base rustc `{}`; head rustc `{}`; cargo `{}` / `{}`; release profile; diagnostic feature on for measured runs.\n\
         - Percentiles use nearest-rank `ceil(p*n)` (1-indexed), computed from every raw sample. Deltas are base minus head; positive means lower head latency.\n\
         - Cold uses a fresh egui Context and an empty Context-scoped Thai plan cache for each sample. Font/glyph activation is warm; workload galley layout and Thai plans are fresh. First-pass timing begins before grid layout. Total sums every measured pass through the first no-retry pass, or through the 300-pass cap when unsettled; capped samples remain in percentiles.\n\
         - Warm readiness requires three consecutive full-coverage frames with all expected Thai requests and cache hits, zero segmentation/measurement/retry, resident context cache entries and reused first-line egui Galleys. Fallback counters are reported even if nonzero; any timed retry/fallback remains in samples.\n\
         - Each recorded pass asserts the full expected set of unique renderer-produced cell prefixes, production `Geo` cell bounds, `Geo::cell_at` hit testing for the main workload, and nonempty intersection with the renderer text clip. Baseline Thai-adapter counters are N/A because base has no adapter.\n\
         - CPU/UI timing wraps `Harness::step()` running `SheetApp::logic` plus the production `grid::show` renderer entry point in a dedicated grid viewport; ribbon/chrome layout is omitted equally on both revisions. It excludes workbook construction, font activation, coverage/counter scans, output serialization, GPU work, screenshot/PNG encoding, and file I/O.\n\
         - Main workload: seed `{}`; {} unique committed text cells, {}×{}; {:.1} Unicode scalars/cell; workload text/style SHA-256 `{}`.\n{}\n\
         - The adjacent stress workload is reported separately; do not compare it with the main 500-cell table.\n",
        base.revision,
        base.source_patch_sha256,
        base.source_manifest_sha256,
        head.revision,
        head.source_patch_sha256,
        head.source_manifest_sha256,
        head.font.relative_path,
        head.font.checkout_revision,
        head.font.sha256,
        head.font.byte_length,
        base.rustc_version,
        head.rustc_version,
        base.cargo_version,
        head.cargo_version,
        MAIN_SEED,
        MAIN_CELL_COUNT,
        MAIN_ROWS,
        MAIN_COLUMNS,
        head.workloads.first().map_or(0.0, |w| w.workload.average_unicode_scalars),
        head.workloads.first().map_or("N/A", |w| w.workload.text_and_style_sha256.as_str()),
        rows,
    );
    let summary = json!({
        "base_revision": base.revision,
        "head_revision": head.revision,
        "base_source_patch_sha256": base.source_patch_sha256,
        "head_source_patch_sha256": head.source_patch_sha256,
        "base_source_manifest_sha256": base.source_manifest_sha256,
        "head_source_manifest_sha256": head.source_manifest_sha256,
        "base_thai_adapter_expected": base.thai_adapter_expected,
        "head_thai_adapter_expected": head.thai_adapter_expected,
        "font": head.font,
        "percentile_method": "nearest-rank ceil(p*n), 1-indexed",
        "workloads": summary_workloads,
    });
    Ok((report, summary))
}

fn summarize(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 4 {
        return Err("usage: benchmark_thai_renderer summarize <base.json> <head.json> <report.md>".into());
    }
    let base: RunArtifact = serde_json::from_slice(&fs::read(args.get(1).ok_or("missing base path")?)?)?;
    let head: RunArtifact = serde_json::from_slice(&fs::read(args.get(2).ok_or("missing head path")?)?)?;
    let (report, summary) = summary_report(&base, &head)?;
    let report_path = PathBuf::from(args.get(3).ok_or("missing report path")?);
    let parent = report_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    fs::write(&report_path, report)?;
    let summary_path = report_path.with_extension("summary.json");
    fs::write(&summary_path, serde_json::to_vec_pretty(&summary)?)?;
    println!("wrote {} and {}", report_path.display(), summary_path.display());
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|first| first == "summarize") { summarize(&args) } else { run() }
}
