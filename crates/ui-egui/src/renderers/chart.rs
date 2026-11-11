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

impl ArtifactRenderer for Chart {
    fn caps(&self) -> RendererCaps {
        RendererCaps::text("chart")
    }

    fn card(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name, id) = (cx.t, cx.artifact.filename.clone(), cx.id);
        if let Some(spec) = super::ready(ui, t, Place::Card, &name, spec(cx)) {
            ui.add_space(4.0);
            crate::chart::show(ui, t, spec, id);
        }
    }

    fn pane(&self, ui: &mut egui::Ui, cx: &mut ArtifactCx<'_>) {
        let (t, name, id) = (cx.t, cx.artifact.filename.clone(), cx.id);
        if let Some(spec) = super::ready(ui, t, Place::Pane, &name, spec(cx)) {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| crate::chart::show(ui, t, spec, id));
        }
    }
}
