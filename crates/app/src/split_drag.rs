//! GPUI drag payloads for terminal panes and split separators.
use gpui_kit::{Context, IntoElement, Render, Window, div};
use sich_proto::TermId;
use crate::workspace::WorkspaceKey;

#[derive(Clone)]
pub(crate) struct TerminalDrag {
    pub workspace: WorkspaceKey,
    pub id: TermId,
}

#[derive(Clone)]
pub(crate) struct SeparatorDrag {
    pub workspace: WorkspaceKey,
    pub path: Vec<bool>,
}

pub(crate) struct DragGhost;
impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
