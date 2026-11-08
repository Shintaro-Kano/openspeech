//! 窓の幅に収まる一覧の行。
//!
//! egui の `Grid` は列の幅が中身の最長の部品で決まり、文を折り返さない。窓は `max_size` を与えても
//! 中身の最小幅までは広がるので、長いキーワードや本文の行があると窓が最大幅を超え、中央揃えのまま
//! 画面の両側で切れる。ここでは 1 行ずつ、固定幅の列・右寄せの列・残りの幅の列に分け、文は列の幅で
//! 「…」で切る(全文はホバーで出す)。

use egui::{Align, Color32, Layout, Response, Sense, TextStyle, TextWrapMode, Ui, WidgetText};

/// 列と列の間に足す隙間(部品どうしの間隔 `item_spacing.x` に加えて)。
pub const GAP: f32 = 8.0;

/// 一覧の 1 行。`stripe` なら Grid の `striped` と同じ薄い地を敷く。中は上揃えで左から右へ置く。
/// 行どうしの間は地の内側の余白で取るので、呼ぶ側は縦の `item_spacing` を 0 にしておく(しまが途切れない)。
pub fn row<R>(ui: &mut Ui, stripe: bool, add: impl FnOnce(&mut Ui) -> R) -> R {
    let fill = if stripe {
        ui.visuals().faint_bg_color
    } else {
        Color32::TRANSPARENT
    };
    egui::Frame::new()
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(4, 3))
        .show(ui, |ui| {
            // 地を行の幅いっぱいに敷く。
            ui.set_width(ui.available_width());
            ui.horizontal_top(add).inner
        })
        .inner
}

/// 幅の決まった列(上揃え・左寄せ)。中の部品はこの幅を超えない物にする(文は `text`)。
pub fn column<R>(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let width = width.max(0.0);
    ui.allocate_ui_with_layout(egui::vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.set_width(width);
        // ボタンや選べるラベルの文も、折り返さずに列の幅で切る。
        ui.style_mut().wrap_mode = Some(TextWrapMode::Truncate);
        add(ui)
    })
    .inner
}

/// 行の右端から左へ置く部分。右寄せの列を置いたあと、最後に `rest` で残りの幅を取る。
pub fn from_right<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.with_layout(Layout::right_to_left(Align::Min), add).inner
}

/// 残りの幅をすべて取る列(`from_right` の中で、右寄せの列を置いたあとに呼ぶ)。
pub fn rest<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let width = ui.available_width();
    column(ui, width, add)
}

/// 列の幅に合わせて末尾を「…」で切る 1 行の文。`full` はこの文の元の全文で、隠れた部分があるとき
/// (幅で切れたか、`text` が文字数で切った物のとき)だけホバーで出す。
/// egui の `Label::truncate` は切れたときに渡した文をホバーで出し、別のホバーと二重になるので、自前で描く。
pub fn text(ui: &mut Ui, label: impl Into<WidgetText>, full: &str) -> Response {
    let galley = label.into().into_galley(
        ui,
        Some(TextWrapMode::Truncate),
        ui.available_width(),
        TextStyle::Body,
    );
    let shown = galley.text();
    let hidden = galley.elided || shown.strip_suffix('…').unwrap_or(shown) != full;
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().galley(rect.min, galley, ui.visuals().text_color());
    }
    if hidden && !full.is_empty() {
        response.on_hover_text(full)
    } else {
        response
    }
}
