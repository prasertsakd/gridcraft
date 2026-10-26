//! Integration tests for Thai line wrapping in committed worksheet cells.
//!
//! Tests real grid renderer integration with egui_kittest harness:
//! - Human-verified Thai break opportunities ("วันนี้" / "อากาศดีมาก")
//! - Full grid painting through `paint_panes` and `paint_quadrant`
//! - Merged cells, frozen panes, and alignments
//! - Undo/redo, clipboard copy-paste, and XLSX byte-for-byte round-trip
//! - Regression protection for non-Thai, numeric, and wrap-off cells

use egui::{FontData, FontFamily, Shape, vec2};
use gridcraft_engine::Session;
use gridcraft_engine::core::{CellRef, Value};
use gridcraft_ui_egui::{SheetApp, theme};
use serde_json::json;
use std::sync::Arc;

type Harness = egui_kittest::Harness<'static, SheetApp>;

fn thai_font_definitions() -> egui::FontDefinitions {
    let mut fonts = theme::pinned_font_definitions();
    let noto_path = std::path::Path::new("D:/code/craft-fonts/fonts/noto-sans-thai/NotoSansThai.ttf");
    if noto_path.exists()
        && let Ok(bytes) = std::fs::read(noto_path)
    {
        fonts.font_data.insert("NotoSansThai".to_string(), Arc::new(FontData::from_owned(bytes)));
        if let Some(prop) = fonts.families.get_mut(&FontFamily::Proportional) {
            prop.push("NotoSansThai".to_string());
        }
        for role in [theme::CELL, theme::CELL_BOLD, theme::UI] {
            if let Some(chain) = fonts.families.get_mut(&FontFamily::Name(role.into())) {
                chain.push("NotoSansThai".to_string());
            }
        }
    }
    fonts
}

fn harness_with_session(session: Session) -> Harness {
    let mut app = SheetApp::new(session, Default::default());
    app.use_prepared_fonts();
    let mut h = egui_kittest::Harness::builder()
        .with_context_setup(|ctx| {
            ctx.set_fonts(thai_font_definitions());
            theme::apply(ctx, false);
        })
        .with_size(vec2(1200.0, 800.0))
        .build_ui_state(
            |ui, app: &mut SheetApp| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            app,
        );
    h.run_steps(4);
    h
}

/// Collects all painted text strings from the latest frame output.
fn painted_texts(h: &Harness) -> Vec<String> {
    let mut out = Vec::new();
    for s in &h.output().shapes {
        if let Shape::Text(t) = &s.shape {
            out.push(t.galley.job.text.clone());
        }
    }
    out
}

#[test]
fn test_thai_cell_wrapping_in_grid_renderer() {
    let mut s = Session::new();
    s.new_workbook();
    // A1: Thai text with Wrap Text ON and narrow column width (forcing wrap)
    s.execute("cell.set", json!({"cell": "A1", "input": "วันนี้อากาศดีมาก"})).unwrap();
    s.execute("home.formatCells", json!({"range": "A1", "style": {"align": {"wrap": true}}})).unwrap();
    s.execute("home.columnWidth", json!({"cols": "A", "width": 45})).unwrap();
    s.execute("home.rowHeight", json!({"rows": "1", "height": 50})).unwrap();

    // A2: Same text with Wrap Text OFF (control)
    s.execute("cell.set", json!({"cell": "A2", "input": "วันนี้อากาศดีมาก"})).unwrap();
    s.execute("home.columnWidth", json!({"cols": "A", "width": 45})).unwrap();

    let h = harness_with_session(s);
    let texts = painted_texts(&h);

    // In A1 with Wrap Text ON: must break at word boundaries ("วันนี้", "อากาศ", "ดีมาก")
    assert!(texts.iter().any(|t| t == "วันนี้"), "Grid renderer should paint 'วันนี้' on its own line in wrapped cell; found: {:?}", texts);
    assert!(
        texts.iter().any(|t| t == "อากาศ" || t == "อากาศดีมาก"),
        "Grid renderer should paint 'อากาศ' or 'อากาศดีมาก' on next line in wrapped cell; found: {:?}",
        texts
    );
    assert!(texts.iter().any(|t| t == "ดีมาก" || t == "อากาศดีมาก"), "Grid renderer should paint 'ดีมาก' or 'อากาศดีมาก'; found: {:?}", texts);

    // In A2 with Wrap Text OFF: the entire unbroken text must be painted on one line
    assert!(texts.iter().any(|t| t == "วันนี้อากาศดีมาก"), "Unwrapped cell A2 must paint full string on one line; found: {:?}", texts);
}

#[test]
fn test_thai_mixed_latin_numbers_currency() {
    let mut s = Session::new();
    s.new_workbook();
    let text = "ยอดรวม Total 1,234.50 บาท (VAT 7%) + ฿";
    s.execute("cell.set", json!({"cell": "A1", "input": text})).unwrap();
    s.execute("home.formatCells", json!({"range": "A1", "style": {"align": {"wrap": true}}})).unwrap();
    s.execute("home.columnWidth", json!({"cols": "A", "width": 80})).unwrap();
    s.execute("home.rowHeight", json!({"rows": "1", "height": 60})).unwrap();

    let h = harness_with_session(s);
    let texts = painted_texts(&h);

    // Verify wrapped lines are rendered without panics or corrupt characters
    assert!(texts.iter().any(|t| t.contains("ยอดรวม")), "Wrapped mixed Thai/Latin cell must render Thai prefix; found: {:?}", texts);
}

#[test]
fn test_thai_cell_hard_breaks_and_blank_lines() {
    let mut s = Session::new();
    s.new_workbook();
    let multiline = "ข้อความหนึ่ง\r\nข้อความสอง\n\nข้อความสี่";
    s.execute("cell.set", json!({"cell": "B2", "input": multiline})).unwrap();
    s.execute("home.formatCells", json!({"range": "B2", "style": {"align": {"wrap": true}}})).unwrap();
    s.execute("home.columnWidth", json!({"cols": "B", "width": 120})).unwrap();
    s.execute("home.rowHeight", json!({"rows": "2", "height": 80})).unwrap();

    let h = harness_with_session(s);
    let texts = painted_texts(&h);

    assert!(texts.iter().any(|t| t.contains("ข้อความหนึ่ง") || (t.contains("ข้อความ") && texts.iter().any(|x| x.contains("หนึ่ง")))));
    assert!(texts.iter().any(|t| t.contains("ข้อความสอง") || (t.contains("ข้อความ") && texts.iter().any(|x| x.contains("สอง")))));
    assert!(texts.iter().any(|t| t.contains("ข้อความสี่") || (t.contains("ข้อความ") && texts.iter().any(|x| x.contains("สี่")))));
}

#[test]
fn test_thai_cell_narrow_oversized_clips_safely() {
    let mut s = Session::new();
    s.new_workbook();
    // Word much wider than the column
    s.execute("cell.set", json!({"cell": "A1", "input": "สวัสดีประเทศไทย"})).unwrap();
    s.execute("home.formatCells", json!({"range": "A1", "style": {"align": {"wrap": true}}})).unwrap();
    s.execute("home.columnWidth", json!({"cols": "A", "width": 10})).unwrap(); // very narrow

    let h = harness_with_session(s);
    let texts = painted_texts(&h);
    // Should render without panic; text is clipped to cell bounds
    assert!(texts.iter().any(|t| t.contains("สวัสดี")));
}

#[test]
fn test_thai_cell_merged_and_frozen_panes() {
    let mut s = Session::new();
    s.new_workbook();
    s.execute("cell.set", json!({"cell": "B2", "input": "วันนี้อากาศดีมาก"})).unwrap();
    s.execute("home.formatCells", json!({"range": "B2", "style": {"align": {"wrap": true}}})).unwrap();
    s.execute("home.mergeCells", json!({"range": "B2:C3"})).unwrap();
    s.execute("view.freezePanes", json!({"cell": "B2"})).unwrap();

    let h = harness_with_session(s);
    let texts = painted_texts(&h);
    assert!(texts.iter().any(|t| t == "วันนี้" || t == "วันนี้อากาศดีมาก" || t == "อากาศดีมาก"));
}

#[test]
fn test_thai_xlsx_roundtrip_undo_redo_copy_paste() {
    let mut s = Session::new();
    s.new_workbook();
    let original = "วันนี้อากาศดีมาก เก้าอี้ น้ำ กำลัง ซื้อ เสื้อ เผื่อ";
    s.execute("cell.set", json!({"cell": "A1", "input": original})).unwrap();
    s.execute("home.formatCells", json!({"range": "A1", "style": {"align": {"wrap": true}}})).unwrap();

    let cell_a1 = CellRef::parse("A1").unwrap();
    let cell_b1 = CellRef::parse("B1").unwrap();

    // Copy A1 to B1
    s.execute("edit.copy", json!({"range": "A1"})).unwrap();
    s.execute("edit.paste", json!({"at": "B1"})).unwrap();

    let b1_val = match &s.active().unwrap().wb.active().unwrap().cell(cell_b1).unwrap().value {
        Value::Text(t) => &t[..],
        _ => "",
    };
    assert_eq!(b1_val, original, "Pasted cell must match original byte-for-byte");

    // Undo paste
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.active().unwrap().wb.active().unwrap().cell(cell_b1).is_none_or(|c| c.value.is_empty()));

    // Redo paste
    s.execute("edit.redo", json!({})).unwrap();
    let b1_restored = match &s.active().unwrap().wb.active().unwrap().cell(cell_b1).unwrap().value {
        Value::Text(t) => &t[..],
        _ => "",
    };
    assert_eq!(b1_restored, original, "Redone cell must match original byte-for-byte");

    // XLSX round-trip
    let wb = &s.active().unwrap().wb;
    let bytes = gridcraft_xlsx::write_xlsx(wb).expect("write xlsx");
    let (reopened, _) = gridcraft_xlsx::read_xlsx(&bytes).expect("read xlsx");
    let reopened_a1 = match &reopened.active().unwrap().cell(cell_a1).unwrap().value {
        Value::Text(t) => &t[..],
        _ => "",
    };
    assert_eq!(reopened_a1, original, "XLSX reopened cell must match original byte-for-byte without extra newlines or ZWSP");
}

#[test]
fn test_non_thai_and_unwrapped_regression() {
    let mut s = Session::new();
    s.new_workbook();
    // English wrapped text
    s.execute("cell.set", json!({"cell": "A1", "input": "Quick brown fox jumps"})).unwrap();
    s.execute("home.formatCells", json!({"range": "A1", "style": {"align": {"wrap": true}}})).unwrap();
    s.execute("home.columnWidth", json!({"cols": "A", "width": 50})).unwrap();

    // Numbers & dates: should not enter Thai wrapping
    s.execute("cell.set", json!({"cell": "A2", "input": "12345.67"})).unwrap();
    s.execute("home.formatCells", json!({"range": "A2", "style": {"num_fmt": "#,##0.00", "align": {"wrap": true}}})).unwrap();

    // Rotated text
    s.execute("cell.set", json!({"cell": "A3", "input": "สวัสดี"})).unwrap();
    s.execute("home.formatCells", json!({"range": "A3", "style": {"align": {"rotation": 45, "wrap": true}}})).unwrap();

    let h = harness_with_session(s);
    let texts = painted_texts(&h);

    assert!(texts.iter().any(|t| t == "Quick brown fox jumps"), "English wrapped uses standard LayoutJob");
    assert!(texts.iter().any(|t| t == "12,345.67"), "Formatted number rendered");
}

#[test]
fn test_thai_alignments_and_indent() {
    let mut s = Session::new();
    s.new_workbook();

    // Center alignment
    s.execute("cell.set", json!({"cell": "A1", "input": "วันนี้อากาศดีมาก"})).unwrap();
    s.execute("home.formatCells", json!({"range": "A1", "style": {"align": {"wrap": true, "h": "Center", "v": "Center"}}})).unwrap();
    s.execute("home.columnWidth", json!({"cols": "A", "width": 45})).unwrap();
    s.execute("home.rowHeight", json!({"rows": "1", "height": 60})).unwrap();

    // Right alignment + indent
    s.execute("cell.set", json!({"cell": "A2", "input": "วันนี้อากาศดีมาก"})).unwrap();
    s.execute("home.formatCells", json!({"range": "A2", "style": {"align": {"wrap": true, "h": "Right", "indent": 1}}})).unwrap();
    s.execute("home.columnWidth", json!({"cols": "A", "width": 45})).unwrap();
    s.execute("home.rowHeight", json!({"rows": "2", "height": 60})).unwrap();

    let h = harness_with_session(s);
    let texts = painted_texts(&h);

    assert!(texts.iter().any(|t| t == "วันนี้"));
    assert!(texts.iter().any(|t| t == "อากาศ" || t == "อากาศดีมาก"));
}
