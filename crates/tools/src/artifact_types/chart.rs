//! Charts (`.chart.json`): a chart spec, validated and normalised before it is saved, drawn
//! natively by clients.

use std::sync::Arc;

use byob_protocol::ArtifactKind;
use byob_protocol::artifact::TYPE_CHART;
use serde_json::Value;

use super::{ArtifactHooks, ArtifactType, ArtifactTypes};

pub fn register(types: &ArtifactTypes) {
    types.register(
        ArtifactType::new(TYPE_CHART, "chart", ArtifactKind::Chart)
            .suffixes(&[".chart.json"])
            .describe(".chart.json (a chart spec)")
            .pick("a chart of numbers → make_chart (or .html for a custom visualisation)")
            .hooks(Arc::new(Chart)),
    );
}

struct Chart;

impl ArtifactHooks for Chart {
    fn check(&self, content: &str) -> Result<(String, String), String> {
        let value: Value = serde_json::from_str(content).map_err(|e| format!("a .chart.json artifact must be a chart spec (JSON): {e}"))?;
        let spec = byob_types::chart::validate(&value)?;
        Ok((spec.to_json(), String::new()))
    }
}
