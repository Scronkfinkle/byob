//! Charts (`.chart.json` specs), drawn natively in the card and the pane.

use byob_protocol::artifact::TYPE_CHART;
use byob_protocol::media::ChartSpec;

use super::{ArtifactCx, ArtifactRenderer, ArtifactRenderers, Place, RendererCaps};

pub fn register(r: &mut ArtifactRenderers) {
    r.register(TYPE_CHART, Chart);
    r.native("chart", std::rc::Rc::new(Chart));
}

pub struct Chart;

fn spec<'a>(cx: &'a mut ArtifactCx<'_>) -> super::Fetch<&'a mut ChartSpec> {
    cx.prepared(|bytes| ChartSpec::from_json(&String::from_utf8_lossy(bytes)))
}

/// A chart drawn from a spreadsheet: where it comes from, and a button to open the sheet.
fn source_line(ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>, sheet: &str) {
    let t = cx.t;
    let name = cx.others.iter().find(|a| a.id == sheet).map(|a| a.title.clone());
    ui.horizontal_wrapped(|ui| {
        let what = match &name {
            Some(n) => format!("From the sheet \u{201c}{n}\u{201d} \u{b7} redraws when it changes"),
            None => "From a sheet that is no longer here; it keeps its last numbers".to_owned(),
        };
        ui.label(egui::RichText::new(what).small().color(t.text_dim));
        if name.is_some() && ui.small_button("Open sheet").on_hover_text("Show the spreadsheet this chart is drawn from").clicked() {
            cx.requests.push(super::Request::Show(sheet.to_owned()));
        }
    });
}

impl ArtifactRenderer for Chart {
    fn caps(&self) -> RendererCaps {
        RendererCaps::text("chart")
    }

    fn card(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name, id) = (cx.t, cx.artifact.filename.clone(), cx.id);
        let mut sheet = None;
        if let Some(spec) = super::ready(ui, t, Place::Card, &name, spec(cx)) {
            ui.add_space(4.0);
            crate::chart::show(ui, t, spec, id);
            sheet = spec.source.as_ref().map(|s| s.sheet.clone());
        }
        if let Some(sheet) = sheet {
            source_line(ui, cx, &sheet);
        }
    }

    fn pane(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name, id) = (cx.t, cx.artifact.filename.clone(), cx.id);
        let Some(spec) = super::ready(ui, t, Place::Pane, &name, spec(cx)).map(|s| s.clone()) else { return };
        if let Some(src) = &spec.source {
            source_line(ui, cx, &src.sheet);
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| crate::chart::show(ui, t, &spec, id));
    }
}
