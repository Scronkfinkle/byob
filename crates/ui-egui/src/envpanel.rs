//! Settings → Workspace environment: the remembered answer to "load this workspace's
//! environment?" (flake.nix, pixi.toml, …) and the workspace's container, with buttons to
//! change or forget them. The engine owns all of it; this only shows
//! [`ServerMsg::WorkspaceEnv`](byob_protocol::ServerMsg::WorkspaceEnv) and sends
//! [`ClientMsg::WorkspaceEnv`].

use byob_protocol::{ClientMsg, EnvAction, WorkspaceEnv};
use egui::RichText;

use crate::theme::Tokens;

/// State between frames.
#[derive(Default)]
pub struct EnvPanel {
    env: Option<WorkspaceEnv>,
    /// A request is on its way (asked once per opening of the settings).
    waiting: bool,
    asked: bool,
}

impl EnvPanel {
    pub fn received(&mut self, env: WorkspaceEnv) {
        self.env = Some(env);
        self.waiting = false;
    }

    /// The settings closed: ask again next time they open (the chat may have changed).
    pub fn closed(&mut self) {
        self.asked = false;
    }

    /// Draw the section; returns a message to send.
    pub fn show(&mut self, ui: &mut egui::Ui, t: &Tokens) -> Option<ClientMsg> {
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
                ui.label(RichText::new(&env.workspace).small().monospace().color(t.text_dim));
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
        Some(ClientMsg::WorkspaceEnv { action })
    }
}

fn project_rows(ui: &mut egui::Ui, t: &Tokens, env: &WorkspaceEnv) -> Option<EnvAction> {
    let mut out = None;
    if env.project_files.is_empty() {
        ui.label(RichText::new("No project environment here (flake.nix, shell.nix, pixi.toml, mise.toml).").color(t.text_dim));
    } else {
        let state = match (env.project_env_forced, env.project_env) {
            (Some(true), _) => "always loaded (BYOB_WORKSPACE_ENV=on)",
            (Some(false), _) => "never loaded (BYOB_WORKSPACE_ENV=off)",
            (None, Some(true)) => "loaded for commands",
            (None, Some(false)) => "not loaded (you said no)",
            (None, None) => "byob asks before loading it",
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("Project environment ({}):", env.project_files.join(", "))).color(t.text));
            ui.label(RichText::new(state).color(t.text_strong));
        });
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
    if let Some(i) = &env.installer {
        ui.label(RichText::new(format!("install_packages uses {i}, kept per workspace in byob's data folder.")).small().color(t.text_dim));
    }
    out
}

fn container_rows(ui: &mut egui::Ui, t: &Tokens, env: &WorkspaceEnv) -> Option<EnvAction> {
    let c = &env.container;
    let mut out = None;
    ui.add_space(4.0);
    let Some(runtime) = &c.runtime else {
        ui.label(RichText::new("Containers: neither Docker nor Podman is installed.").color(t.text_dim));
        return None;
    };
    let on = c.use_container == Some(true);
    ui.horizontal_wrapped(|ui| {
        let what = match &c.devcontainer {
            Some(f) => format!("Run commands in the dev container ({f}, {runtime})"),
            None => format!("Run commands in a {runtime} container ({})", c.image),
        };
        let mut checked = on;
        if ui
            .checkbox(&mut checked, what)
            .on_hover_text("run_command and run_python run inside one container per workspace; the workspace is mounted at the same path")
            .changed()
        {
            out = Some(EnvAction::Container { on: Some(checked) });
        }
    });
    if c.devcontainer.is_some() && c.use_container.is_none() {
        ui.label(RichText::new("Not decided yet: byob asks on the first command.").small().color(t.text_dim));
    }
    ui.horizontal(|ui| {
        let state = c.state.as_deref().unwrap_or("none");
        ui.label(RichText::new(format!("Container: {state}")).small().color(t.text_dim));
        if c.state.as_deref() == Some("running") && ui.small_button("Stop").clicked() {
            out = Some(EnvAction::StopContainer);
        }
        if c.state.is_some() && ui.small_button("Remove").on_hover_text("Delete the container; packages installed in it are lost").clicked() {
            out = Some(EnvAction::RemoveContainer);
        }
        if c.use_container.is_some() && ui.small_button("Ask again").on_hover_text("Forget the choice").clicked() {
            out = Some(EnvAction::Container { on: None });
        }
    });
    out
}
