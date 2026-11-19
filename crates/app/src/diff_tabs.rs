//! Diff tab selection belongs to the UI and is kept per workspace.

use sich_git::compare::CompareKey;

pub(crate) struct DiffTab<T> {
    pub id: u64,
    pub key: CompareKey,
    pub view: T,
}

pub(crate) struct DiffTabs<T> {
    pub tabs: Vec<DiffTab<T>>,
    pub active: Option<u64>,
}

impl<T> Default for DiffTabs<T> {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: None,
        }
    }
}

impl<T> DiffTabs<T> {
    pub fn open(&mut self, id: u64, key: CompareKey, create: impl FnOnce() -> T) -> &DiffTab<T> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.key == key)
            .unwrap_or_else(|| {
                let index = self.tabs.len();
                self.tabs.push(DiffTab {
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

    pub fn active(&self) -> Option<&DiffTab<T>> {
        self.tabs.iter().find(|tab| Some(tab.id) == self.active)
    }

    pub fn activate(&mut self, id: u64) {
        if self.tabs.iter().any(|tab| tab.id == id) {
            self.active = Some(id);
        }
    }

    pub fn close(&mut self, id: u64) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        self.tabs.remove(index);
        if self.active == Some(id) {
            self.active = self
                .tabs
                .get(index)
                .or_else(|| self.tabs.last())
                .map(|tab| tab.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_git::compare::CompareSource;

    fn key(source: CompareSource) -> CompareKey {
        CompareKey {
            path: "src/file.rs".into(),
            source,
        }
    }

    #[test]
    fn reopening_selects_existing_file_and_source_with_its_view() {
        let mut tabs = DiffTabs::default();
        tabs.open(1, key(CompareSource::Working), || "working view");
        tabs.open(2, key(CompareSource::Staged), || "staged view");
        let working = tabs.open(3, key(CompareSource::Working), || panic!("duplicate view"));
        assert_eq!((working.id, working.view), (1, "working view"));
        assert_eq!(tabs.active, Some(1));
        assert_eq!(tabs.tabs.len(), 2);
    }

    #[test]
    fn closing_an_active_tab_selects_the_neighbor_or_returns_to_the_terminal() {
        let mut tabs = DiffTabs::default();
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
        let mut tabs = DiffTabs::default();
        tabs.open(1, key(CompareSource::Working), || ());
        tabs.open(2, key(CompareSource::Staged), || ());
        tabs.close(1);
        tabs.activate(1);
        assert_eq!(tabs.active, Some(2));
    }
}
