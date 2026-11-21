//! Tab selection for the non-terminal views of a workspace.

pub(crate) struct WorkspaceTab<K, T> {
    pub id: u64,
    pub key: K,
    pub view: T,
}

pub(crate) struct WorkspaceTabs<K, T> {
    pub tabs: Vec<WorkspaceTab<K, T>>,
    pub active: Option<u64>,
}

impl<K, T> Default for WorkspaceTabs<K, T> {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: None,
        }
    }
}

impl<K: Eq, T> WorkspaceTabs<K, T> {
    pub fn open(&mut self, id: u64, key: K, create: impl FnOnce() -> T) -> &WorkspaceTab<K, T> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.key == key)
            .unwrap_or_else(|| {
                let index = self.tabs.len();
                self.tabs.push(WorkspaceTab {
                    id,
                    key,
                    view: create(),
                });
                index
            });
        let tab = &self.tabs[index];
        self.active = Some(tab.id);
        tab
    }

    pub fn active(&self) -> Option<&WorkspaceTab<K, T>> {
        self.tabs.iter().find(|tab| Some(tab.id) == self.active)
    }

    pub fn activate(&mut self, id: u64) {
        if self.tabs.iter().any(|tab| tab.id == id) {
            self.active = Some(id);
        }
    }

    pub fn close(&mut self, id: u64) {
        self.close_where(|tab| tab.id == id);
    }

    /// Close a batch in one pass, selecting the next surviving neighbor if the active tab closed.
    pub fn close_where(&mut self, mut should_close: impl FnMut(&WorkspaceTab<K, T>) -> bool) -> bool {
        let old_len = self.tabs.len();
        let mut kept = 0;
        let mut fallback = None;
        self.tabs.retain(|tab| {
            if should_close(tab) {
                if self.active == Some(tab.id) {
                    fallback = Some(kept);
                }
                false
            } else {
                kept += 1;
                true
            }
        });
        if let Some(index) = fallback {
            self.active = self.tabs.get(index).or_else(|| self.tabs.last()).map(|tab| tab.id);
        }
        self.tabs.len() != old_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_git::compare::{CompareKey, CompareSource};

    fn key(source: CompareSource) -> CompareKey {
        CompareKey {
            path: "src/file.rs".into(),
            source,
        }
    }

    #[test]
    fn reopening_selects_existing_file_and_source_with_its_view() {
        let mut tabs = WorkspaceTabs::default();
        tabs.open(1, key(CompareSource::Working), || "working view");
        tabs.open(2, key(CompareSource::Staged), || "staged view");
        let working = tabs.open(3, key(CompareSource::Working), || panic!("duplicate view"));
        assert_eq!((working.id, working.view), (1, "working view"));
        assert_eq!(tabs.active, Some(1));
        assert_eq!(tabs.tabs.len(), 2);
    }

    #[test]
    fn closing_an_active_tab_selects_the_neighbor_or_returns_to_the_terminal() {
        let mut tabs = WorkspaceTabs::default();
        tabs.open(1, key(CompareSource::Working), || ());
        tabs.open(2, key(CompareSource::Staged), || ());
        tabs.open(3, key(CompareSource::Conflict), || ());
        tabs.activate(2);
        tabs.close(2);
        assert_eq!(tabs.active, Some(3));
        tabs.close(3);
        assert_eq!(tabs.active, Some(1));
        tabs.close(1);
        assert!(tabs.active().is_none());
    }

    #[test]
    fn closing_a_background_tab_keeps_the_current_selection() {
        let mut tabs = WorkspaceTabs::default();
        tabs.open(1, key(CompareSource::Working), || ());
        tabs.open(2, key(CompareSource::Staged), || ());
        tabs.close(1);
        tabs.activate(1);
        assert_eq!(tabs.active, Some(2));
    }
}
