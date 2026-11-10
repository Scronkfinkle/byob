//! A workspace's environment state, in its settings window: the remembered answer to "load
//! this workspace's environment?" (flake.nix, pixi.toml, …) and its container, with
//! buttons to change or forget the answer and to stop or remove the container. Where
//! commands run (Auto, this machine, Nix, container) is a setting of the workspace above it.
//! The engine owns all of it; this only shows
//! [`ServerMsg::WorkspaceEnv`](byob_protocol::ServerMsg::WorkspaceEnv) and sends
//! [`ClientMsg::WorkspaceEnv`].

use byob_protocol::{ClientMsg, EnvAction, WorkspaceEnv};
use egui::RichText;

use crate::theme::Tokens;

/// State between frames.
#[derive(Default)]
pub struct EnvPanel {
    env: Option<WorkspaceEnv>,
    /// A request is on its way.
    waiting: bool,
    asked: bool,
}

impl EnvPanel {
    pub fn received(&mut self, env: WorkspaceEnv) {
        self.env = Some(env);
        self.waiting = false;
    }

    /// Ask again (the workspace's settings changed).
    pub fn refresh(&mut self) {
        self.asked = false;
    }

    /// Draw the section for workspace `workspace`; returns a message to send.
    pub fn show(&mut self, ui: &mut egui::Ui, t: &Tokens, workspace: &str) -> Option<ClientMsg> {
        let mut action: Option<Option<EnvAction>> = None;
        if !self.asked {
            self.asked = true;
            action = Some(None);
        }
        match &self.env {
            None => {
                ui.label(RichText::new("loading…").color(t.text_dim));
            }
            Some(env) => {
                ui.add_enabled_ui(!self.waiting, |ui| {
                    let project = project_rows(ui, t, env);
                    if let Some(a) = project.or(container_rows(ui, t, env)) {
                        action = Some(Some(a));
                    }
                });
                if let Some(m) = &env.message {
                    ui.label(RichText::new(m).small().color(if env.failed { t.danger } else { t.text_dim }));
                }
            }
        }
        let action = action?;
        self.waiting = true;
        Some(ClientMsg::WorkspaceEnv { action, workspace: Some(workspace.to_owned()) })
    }
}

fn project_rows(ui: &mut egui::Ui, t: &Tokens, env: &WorkspaceEnv) -> Option<EnvAction> {
    let mut out = None;
    if env.project_files.is_empty() {
        ui.label(RichText::new("No project environment in the folder (flake.nix, shell.nix, pixi.toml, mise.toml).").small().color(t.text_dim));
    } else {
        let state = match (env.project_env_forced, env.project_env) {
            (Some(true), _) => "always loaded (Nix environment, or BYOB_WORKSPACE_ENV=on)",
            (Some(false), _) => "never loaded (BYOB_WORKSPACE_ENV=off)",
            (None, Some(true)) => "loaded for commands",
            (None, Some(false)) => "not loaded (you said no)",
            (None, None) => "byob asks before loading it",
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("Project environment ({}):", env.project_files.join(", "))).color(t.text));
            ui.label(RichText::new(state).color(t.text_strong));
        });
        if env.project_env_forced.is_none() {
            ui.horizontal(|ui| {
                if env.project_env != Some(true) && ui.button("Load it").on_hover_text("Load it for every command in this workspace").clicked() {
                    out = Some(EnvAction::ProjectEnv { load: Some(true) });
                }
                if env.project_env != Some(false) && ui.button("Don't load it").clicked() {
                    out = Some(EnvAction::ProjectEnv { load: Some(false) });
                }
                if env.project_env.is_some() && ui.button("Ask again").on_hover_text("Forget the answer").clicked() {
                    out = Some(EnvAction::ProjectEnv { load: None });
                }
            });
        }
    }
    if let Some(i) = &env.installer {
        ui.label(RichText::new(format!("install_packages uses {i}, kept per workspace in byob's data folder.")).small().color(t.text_dim));
    }
    out
}

fn container_rows(ui: &mut egui::Ui, t: &Tokens, env: &WorkspaceEnv) -> Option<EnvAction> {
    let c = &env.container;
    let mut out = None;
    let Some(runtime) = &c.runtime else {
        ui.label(RichText::new("Containers: neither Docker nor Podman is installed.").small().color(t.text_dim));
        return None;
    };
    let place = match c.use_container {
        Some(true) => format!("Commands run in a {runtime} container ({}).", c.image),
        Some(false) => "Commands run on this machine.".to_owned(),
        None if c.devcontainer.is_some() => "Not decided yet: byob asks on the first command whether to use the dev container.".to_owned(),
        None => "Commands run on this machine.".to_owned(),
    };
    ui.label(RichText::new(place).small().color(t.text_dim));
    ui.horizontal(|ui| {
        let state = c.state.as_deref().unwrap_or("none yet");
        ui.label(RichText::new(format!("Container: {state}")).small().color(t.text_dim));
        if c.state.as_deref() == Some("running") && ui.small_button("Stop").clicked() {
            out = Some(EnvAction::StopContainer);
        }
        if c.state.is_some() && ui.small_button("Remove").on_hover_text("Delete the container; packages installed in it are lost").clicked() {
            out = Some(EnvAction::RemoveContainer);
        }
    });
    out
}
