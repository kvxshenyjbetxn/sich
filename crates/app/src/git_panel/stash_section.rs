//! The collapsible "Stashes" list under the working changes.

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

use super::render::{chevron, tiny, tool_button};
use super::*;
use crate::i18n::relative_time;
use crate::sidebar::now_unix_ms;

const ACTIONS_W: f32 = 60.;

/// "branch · 3 h ago", or just the time when the stash has no branch.
pub(super) fn stash_subtitle(lang: Language, stash: &Stash, now_ms: u64) -> String {
    let when = relative_time(lang, now_ms, (stash.timestamp.max(0) as u64).saturating_mul(1000));
    match &stash.branch {
        Some(branch) => format!("{branch} \u{b7} {when}"),
        None => when,
    }
}

pub(super) fn stash_title(stash: &Stash) -> String {
    if stash.message.trim().is_empty() {
        format!("stash@{{{}}}", stash.index)
    } else {
        stash.message.trim().to_owned()
    }
}

impl GitPanel {
    pub(super) fn stash_section(
        &self,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.stashes.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let collapsed = self.stashes_collapsed;
        let now = now_unix_ms();
        Some(
            v_flex()
                .id("git-stashes-section")
                .child(
                    h_flex()
                        .id("git-stashes")
                        .h(px(26.))
                        .gap_1()
                        .px_2()
                        .items_center()
                        .cursor_pointer()
                        .bg(theme.list_head)
                        .hover(|style| style.bg(theme.list_hover))
                        .on_click(cx.listener(|panel, _, _, cx| {
                            panel.stashes_collapsed = !panel.stashes_collapsed;
                            cx.notify();
                        }))
                        .child(chevron(!collapsed))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_size(px(12.))
                                .child(format!("{} {}", tr(lang, Tr::GitStashes), self.stashes.len())),
                        ),
                )
                .children(
                    self.stashes
                        .iter()
                        .filter(|_| !collapsed)
                        .map(|stash| self.stash_row(stash, busy, lang, now, cx)),
                )
                .into_any_element(),
        )
    }

    fn stash_row(
        &self,
        stash: &Stash,
        busy: Option<Operation>,
        lang: Language,
        now: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let group = SharedString::from(format!("git-stash-{}", stash.oid));
        let expansion = self.stash_open.get(&stash.oid);
        let (apply, pop, drop) = (stash.clone(), stash.clone(), stash.clone());
        let toggle = stash.clone();
        let actions = h_flex()
            .w(px(ACTIONS_W))
            .flex_shrink_0()
            .invisible()
            .group_hover(group.clone(), |style| style.visible())
            .child(
                tool_button("git-stash-apply", Ic::Copy, tr(lang, Tr::GitStashApply))
                    .disabled(busy.is_some())
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.mutate(Mutation::StashApply(apply.clone()), window, cx);
                        cx.stop_propagation();
                    })),
            )
            .child(
                tool_button("git-stash-pop", Ic::ArchiveRestore, tr(lang, Tr::GitStashPop))
                    .disabled(busy.is_some())
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.mutate(Mutation::StashPop(pop.clone()), window, cx);
                        cx.stop_propagation();
                    })),
            )
            .child(
                tool_button("git-stash-drop", Ic::Trash, tr(lang, Tr::GitStashDrop))
                    .disabled(busy.is_some())
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        let stash = drop.clone();
                        panel.confirm(
                            Tr::GitStashDrop,
                            format!("{}\n{}", tr(lang, Tr::GitStashDropConfirm), stash_title(&stash)),
                            true,
                            lang,
                            move |panel, window, cx| {
                                panel.mutate(Mutation::StashDrop(stash.clone()), window, cx)
                            },
                            window,
                            cx,
                        );
                        cx.stop_propagation();
                    })),
            );
        let title: SharedString = stash_title(stash).into();
        v_flex()
            .id(SharedString::from(format!("{group}-row")))
            .w_full()
            .child(
                h_flex()
                    .id(group.clone())
                    .group(group.clone())
                    .min_h(px(36.))
                    .min_w_0()
                    .gap_1()
                    .px_2()
                    .items_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.list_hover))
                    .on_click(cx.listener(move |panel, _, _, cx| panel.toggle_stash(toggle.clone(), cx)))
                    .child(chevron(expansion.is_some()))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child(stash_subtitle(lang, stash, now)),
                            ),
                    )
                    .child(actions),
            )
            .children(expansion.map(|files| {
                v_flex()
                    .w_full()
                    .pl_3()
                    .pb_1()
                    .child(match &files.result {
                        None => tiny(tr(lang, Tr::GitLoading), cx),
                        Some(Err(error)) => tiny(error.to_string(), cx),
                        Some(Ok(files)) => v_flex()
                            .w_full()
                            .children(files.iter().map(|file| {
                                self.file_row(
                                    file,
                                    CompareSource::Commit(stash.oid.clone()),
                                    format!("git-stash-file-{}-{}", stash.oid, file.path).into(),
                                    None,
                                    cx,
                                )
                            }))
                            .into_any_element(),
                    })
            }))
            .into_any_element()
    }

    /// Expands a stash (loading its files in the background) or collapses it.
    fn toggle_stash(&mut self, stash: Stash, cx: &mut Context<Self>) {
        if self.stash_open.remove(&stash.oid).is_some() {
            cx.notify();
            return;
        }
        let Some(repo) = self.repo.clone() else { return };
        self.next_request += 1;
        let request = self.next_request;
        let oid = stash.oid.clone();
        self.stash_open.insert(oid.clone(), Expansion::pending(request));
        let generation = self.generation;
        let load = cx.background_spawn(async move { repo.stash_files(&stash) });
        cx.spawn(async move |panel, cx| {
            let result = load.await;
            let _ = panel.update(cx, |panel, cx| {
                panel.generation.apply(generation, || {
                    if let Some(expansion) = panel.stash_open.get_mut(&oid) {
                        expansion.apply(request, result);
                        cx.notify();
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stash(branch: Option<&str>, message: &str, timestamp: i64) -> Stash {
        Stash {
            index: 2,
            oid: "a".repeat(40),
            message: message.into(),
            branch: branch.map(str::to_owned),
            timestamp,
        }
    }

    #[test]
    fn subtitle_joins_branch_and_relative_time() {
        let now = 1_000_000_000_000u64;
        let three_hours_ago = (now / 1000 - 3 * 3600) as i64;
        let subtitle = stash_subtitle(Language::En, &stash(Some("feat/x"), "wip", three_hours_ago), now);
        assert!(subtitle.starts_with("feat/x \u{b7} "), "{subtitle}");
        assert!(subtitle.len() > "feat/x \u{b7} ".len());
        let no_branch = stash_subtitle(Language::En, &stash(None, "wip", three_hours_ago), now);
        assert!(!no_branch.contains('\u{b7}'), "{no_branch}");
        assert!(!no_branch.is_empty());
    }

    #[test]
    fn subtitle_survives_bad_timestamps() {
        let negative = stash_subtitle(Language::En, &stash(Some("main"), "m", -5), 1_000);
        assert!(negative.starts_with("main \u{b7} "));
        let huge = stash_subtitle(Language::En, &stash(None, "m", i64::MAX), 1_000);
        assert!(!huge.is_empty());
    }

    #[test]
    fn title_falls_back_to_the_stash_selector() {
        assert_eq!(stash_title(&stash(None, "  my work ", 0)), "my work");
        assert_eq!(stash_title(&stash(None, "", 0)), "stash@{2}");
        assert_eq!(stash_title(&stash(None, "   ", 0)), "stash@{2}");
    }
}
