//! Drag payloads shared across views. A view that accepts a drop names the
//! payload type, so the type lives here where every view crate can see it.

/// What a drag that started outside the pane tree carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneDragSource {
    /// A workspace tab from the title bar.
    WorkspaceTab(String),
    /// A session card from the sidebar.
    Session(String),
}

impl PaneDragSource {
    /// The id the hint compares with leaf ids, like `PaneDrop.fromId`.
    pub fn id(&self) -> &str {
        match self {
            PaneDragSource::WorkspaceTab(id) | PaneDragSource::Session(id) => id,
        }
    }
}
