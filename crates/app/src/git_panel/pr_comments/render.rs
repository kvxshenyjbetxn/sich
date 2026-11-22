//! Rendering of the comments section. Nothing here decides anything: grouping, filters and
//! counts come from `sich_git::forge`, the rest from `state.rs`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Textarea;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use sich_git::forge::{
    CommentFilter, CommentGroup, CommentItem, CommentMode, GroupKind, PrComment, PrConversation, ReactionKind,
    ReviewThread, filter_counts, group_comments,
};

use super::state::{Selection, parse_iso_ms, strip_html_comments, thread_location};
use super::view::{PrComments, Target};
use crate::git_panel::render::tool_button;
use crate::i18n::{Language, Tr, relative_time, tr};
use crate::sidebar::now_unix_ms;

fn uid(text: &str) -> usize {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish() as usize
}

fn eid(name: &'static str, key: &str) -> ElementId {
    (name, uid(key)).into()
}

fn comment_count(conv: &PrConversation) -> usize {
    conv.threads.iter().map(|t| t.comments.len()).sum::<usize>() + conv.comments.len() + conv.reviews.len()
}

fn caption(kind: GroupKind) -> Tr {
    match kind {
        GroupKind::NeedsReview => Tr::CommentsNeedsReview,
        GroupKind::Conversation => Tr::CommentsConversation,
        GroupKind::Resolved => Tr::CommentsResolved,
        GroupKind::Timeline => Tr::CommentsTimeline,
    }
}

fn empty_text(filter: CommentFilter) -> Tr {
    match filter {
        CommentFilter::All => Tr::CommentsEmpty,
        CommentFilter::Humans => Tr::CommentsEmptyHumans,
        CommentFilter::Bots => Tr::CommentsEmptyBots,
    }
}

impl Render for PrComments {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.mark_seen();
        if self.ctx.is_none() {
            return div().into_any_element();
        }
        self.maybe_fetch(cx);
        let lang = self.lang;
        let theme = cx.theme().clone();
        let slot = self.slot();
        let conv = slot.and_then(|slot| slot.conv.as_ref());
        let loading = slot.is_some_and(|slot| slot.loading);
        let error = slot.and_then(|slot| slot.error.clone());
        let counts = conv.map(filter_counts).unwrap_or_default();

        let header = h_flex()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(format!("{} {}", tr(lang, Tr::CommentsTitle), counts.0)),
            )
            .child(
                tool_button("pc-send-unresolved", Ic::Send, tr(lang, Tr::CommentsSendUnresolved))
                    .on_click(cx.listener(|this, _, _, cx| this.send_to_agent(Selection::Unresolved, cx))),
            )
            .child(
                tool_button(
                    "pc-mode",
                    if self.mode == CommentMode::Grouped { Ic::Clock } else { Ic::ListTree },
                    if self.mode == CommentMode::Grouped {
                        tr(lang, Tr::CommentsModeTimeline)
                    } else {
                        tr(lang, Tr::CommentsModeGrouped)
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| this.toggle_mode(cx))),
            )
            .child(
                tool_button("pc-refresh", Ic::RefreshCw, tr(lang, Tr::Refresh))
                    .loading(loading)
                    .on_click(cx.listener(|this, _, _, cx| this.fetch(cx))),
            )
            .child(
                tool_button("pc-add", Ic::MessageSquarePlus, tr(lang, Tr::CommentsAdd)).on_click(cx.listener(
                    |this, _, window, cx| this.open_editor(Target::Add, "", window, cx),
                )),
            );

        let tabs = h_flex().gap_1().children(
            [
                (CommentFilter::All, Tr::CommentsFilterAll, counts.0),
                (CommentFilter::Humans, Tr::CommentsFilterHumans, counts.1),
                (CommentFilter::Bots, Tr::CommentsFilterBots, counts.2),
            ]
            .into_iter()
            .map(|(filter, label, count)| {
                Button::new(("pc-filter", filter as usize))
                    .ghost()
                    .xsmall()
                    .label(format!("{} {count}", tr(lang, label)))
                    .selected(self.filter == filter)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_filter(filter, cx)))
            }),
        );

        let notice = self.notice.clone().map(|message| {
            h_flex()
                .items_start()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(11.))
                        .text_color(theme.red)
                        .line_clamp(3)
                        .child(message.lines().take(3).collect::<Vec<_>>().join("\n")),
                )
                .child(
                    Button::new("pc-dismiss")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(Ic::Check))
                        .tooltip(tr(lang, Tr::GitDismiss))
                        .on_click(cx.listener(|this, _, _, cx| this.dismiss_notice(cx))),
                )
        });

        let composer = match &self.editor {
            Some(editor) if editor.target == Target::Add => Some(self.editor_box(editor, lang, cx)),
            _ => None,
        };

        let body: AnyElement = match conv {
            None => match error {
                Some(message) => h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.warning)
                            .child(message.lines().next().unwrap_or_default().to_owned()),
                    )
                    .child(
                        Button::new("pc-retry")
                            .ghost()
                            .xsmall()
                            .label(tr(lang, Tr::PrRetry))
                            .on_click(cx.listener(|this, _, _, cx| this.fetch(cx))),
                    )
                    .into_any_element(),
                None => muted(tr(lang, Tr::CommentsLoading), cx),
            },
            Some(conv) => {
                let groups = group_comments(conv, self.filter, self.mode);
                let now = now_unix_ms();
                let mut column = v_flex().gap_2();
                if let Some(message) = error {
                    column = column.child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(theme.warning)
                                    .child(message.lines().next().unwrap_or_default().to_owned()),
                            )
                            .child(
                                Button::new("pc-retry")
                                    .ghost()
                                    .xsmall()
                                    .label(tr(lang, Tr::PrRetry))
                                    .on_click(cx.listener(|this, _, _, cx| this.fetch(cx))),
                            ),
                    );
                }
                if groups.is_empty() {
                    column = column.child(muted(tr(lang, empty_text(self.filter)), cx));
                }
                for group in &groups {
                    column = column.child(self.group_el(group, lang, now, cx));
                }
                if conv.truncated {
                    column = column.child(muted(
                        &tr(lang, Tr::CommentsTruncated).replace("{n}", &comment_count(conv).to_string()),
                        cx,
                    ));
                }
                column.into_any_element()
            }
        };

        v_flex()
            .gap_2()
            .child(header)
            .child(tabs)
            .children(notice)
            .children(composer)
            .child(body)
            .into_any_element()
    }
}

fn muted(text: &str, cx: &Context<PrComments>) -> AnyElement {
    div()
        .py_1()
        .text_size(px(11.))
        .text_color(cx.theme().muted_foreground)
        .child(text.to_owned())
        .into_any_element()
}

impl PrComments {
    fn group_el(&self, group: &CommentGroup<'_>, lang: Language, now: u64, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let collapsible = group.kind == GroupKind::Resolved;
        let open = !collapsible || self.resolved_open;
        let title = format!("{} {}", tr(lang, caption(group.kind)).to_uppercase(), group.items.len());
        let head = h_flex()
            .id(eid("pc-group", &title))
            .gap_1()
            .items_center()
            .text_size(px(10.))
            .text_color(theme.muted_foreground)
            .when(collapsible, |row| {
                row.cursor_pointer()
                    .child(
                        Icon::new(if open { Ic::ChevronDown } else { Ic::ChevronRight })
                            .size(px(11.))
                            .flex_shrink_0(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_resolved_group(cx)))
            })
            .child(title);
        let items = open.then(|| {
            v_flex().gap_2().children(group.items.iter().map(|item| match item {
                CommentItem::Thread(thread) => self.thread_card(thread, lang, now, cx),
                CommentItem::Comment(comment) => self.standalone_comment(comment, lang, now, cx),
            }))
        });
        v_flex().gap_1().child(head).children(items).into_any_element()
    }

    fn card(&self, key: &str, cx: &Context<Self>) -> Stateful<gpui_kit::Div> {
        v_flex()
            .id(eid("pc-card", key))
            .gap_1()
            .p(px(6.))
            .rounded(px(6.))
            .border_1()
            .border_color(cx.theme().border)
    }

    fn thread_card(&self, thread: &ReviewThread, lang: Language, now: u64, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let location = thread_location(&thread.path, thread.line, thread.start_line);
        let badge = h_flex()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .font_family("Cascadia Mono")
                    .text_color(theme.muted_foreground)
                    .child(location),
            )
            .when(thread.outdated, |row| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.warning.opacity(0.15))
                        .text_color(theme.warning)
                        .text_size(px(10.))
                        .child(tr(lang, Tr::CommentsOutdated)),
                )
            });
        let thread_id = thread.id.clone();
        let comments: Vec<AnyElement> = thread
            .comments
            .iter()
            .enumerate()
            .filter_map(|(index, comment)| {
                self.comment_el(comment, false, lang, now, cx).map(|el| {
                    if index == 0 { el } else { div().pl_3().child(el).into_any_element() }
                })
            })
            .collect();
        let replying = matches!(&self.editor, Some(e) if e.target == Target::Reply(thread.id.clone()));
        let reply_box = replying.then(|| self.editor_box(self.editor.as_ref().expect("checked"), lang, cx));
        let resolved = thread.resolved;
        let footer = h_flex()
            .gap_1()
            .items_center()
            .child({
                let id = thread_id.clone();
                Button::new(eid("pc-t-reply", &thread_id))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::Reply))
                    .label(tr(lang, Tr::CommentsReply))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_editor(Target::Reply(id.clone()), "", window, cx)
                    }))
            })
            .child({
                let id = thread_id.clone();
                Button::new(eid("pc-t-resolve", &thread_id))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(if resolved { Ic::Undo2 } else { Ic::Check }))
                    .label(tr(lang, if resolved { Tr::CommentsUnresolve } else { Tr::CommentsResolve }))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_resolved(&id, !resolved, cx)))
            })
            .child({
                let id = thread_id.clone();
                Button::new(eid("pc-t-send", &thread_id))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::Send))
                    .label(tr(lang, Tr::PrSendToAgent))
                    .on_click(cx.listener(move |this, _, _, cx| this.send_to_agent(Selection::Thread(id.clone()), cx)))
            });
        self.card(&thread_id, cx)
            .child(badge)
            .children(comments)
            .children(reply_box)
            .child(footer)
            .into_any_element()
    }

    fn standalone_comment(&self, comment: &PrComment, lang: Language, now: u64, cx: &mut Context<Self>) -> AnyElement {
        let editable = comment.viewer_did_author && comment.review_state.is_none() && comment.database_id.is_some();
        let replying = matches!(&self.editor, Some(e) if e.target == Target::Reply(comment.node_id.clone()));
        let reply_box = replying.then(|| self.editor_box(self.editor.as_ref().expect("checked"), lang, cx));
        let id = comment.node_id.clone();
        let footer = h_flex().gap_1().items_center().child(
            Button::new(eid("pc-c-reply", &id))
                .ghost()
                .xsmall()
                .icon(Icon::new(Ic::Reply))
                .label(tr(lang, Tr::CommentsReply))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_editor(Target::Reply(id.clone()), "", window, cx)
                })),
        );
        self.card(&comment.node_id, cx)
            .children(self.comment_el(comment, editable, lang, now, cx))
            .children(reply_box)
            .child(footer)
            .into_any_element()
    }

    /// One comment: author line, body (markdown) or its editor, reactions, own-comment actions.
    fn comment_el(
        &self,
        comment: &PrComment,
        editable: bool,
        lang: Language,
        now: u64,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let id = comment.node_id.clone();
        let when = parse_iso_ms(&comment.created_at)
            .map(|at| relative_time(lang, now, at))
            .unwrap_or_default();
        let editing_here = matches!(&self.editor, Some(e) if e.target == Target::Edit(id.clone()));
        let header = h_flex()
            .gap_1()
            .items_center()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(comment.author.clone()),
            )
            .when(comment.author_is_bot, |row| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .px_1()
                        .rounded_sm()
                        .bg(theme.muted_foreground.opacity(0.15))
                        .text_color(theme.muted_foreground)
                        .text_size(px(10.))
                        .child(tr(lang, Tr::CommentsBot)),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(when),
            )
            .when(editable && !editing_here, |row| {
                let edit_id = id.clone();
                let body = comment.body.clone();
                let delete_id = id.clone();
                row.child(
                    Button::new(eid("pc-edit", &id))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(Ic::Pencil))
                        .tooltip(tr(lang, Tr::CommentsEdit))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_editor(Target::Edit(edit_id.clone()), &body, window, cx)
                        })),
                )
                .child(
                    Button::new(eid("pc-delete", &id))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(Ic::Trash))
                        .tooltip(tr(lang, Tr::CommentsDelete))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_delete(&delete_id, window, cx)
                        })),
                )
            });

        let body: AnyElement = if editing_here {
            self.editor_box(self.editor.as_ref().expect("checked"), lang, cx)
        } else {
            let text = strip_html_comments(&comment.body);
            div()
                .min_w_0()
                .text_size(px(12.))
                .child(TextView::markdown(eid("pc-md", &id), SharedString::from(text)).selectable(true))
                .into_any_element()
        };
        Some(
            v_flex()
                .gap_1()
                .min_w_0()
                .child(header)
                .child(body)
                .child(self.reactions_row(comment, cx))
                .into_any_element(),
        )
    }

    fn reactions_row(&self, comment: &PrComment, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let id = comment.node_id.clone();
        let picker_open = self.picker.as_deref() == Some(id.as_str());
        let chips = comment.reactions.iter().map(|reaction| {
            let kind = reaction.content;
            let node = id.clone();
            let active = reaction.viewer_has;
            div()
                .id(eid("pc-react", &format!("{id}{}", kind.graphql())))
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .border_1()
                .border_color(if active { theme.primary } else { theme.border })
                .bg(if active { theme.primary.opacity(0.15) } else { theme.border.opacity(0.0) })
                .text_size(px(11.))
                .child(format!("{} {}", kind.emoji(), reaction.count))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_reaction(&node, kind, cx)))
        });
        let toggle_id = id.clone();
        let add = Button::new(eid("pc-react-add", &id))
            .ghost()
            .xsmall()
            .icon(Icon::new(Ic::Plus))
            .selected(picker_open)
            .tooltip(tr(self.lang, Tr::CommentsAddReaction))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_picker(&toggle_id, cx)));
        let picker = picker_open.then(|| {
            h_flex().gap_1().flex_wrap().children(ReactionKind::ALL.into_iter().map(|kind| {
                let node = id.clone();
                Button::new(eid("pc-pick", &format!("{id}{}", kind.graphql())))
                    .ghost()
                    .xsmall()
                    .label(kind.emoji())
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_reaction(&node, kind, cx)))
            }))
        });
        v_flex()
            .gap_1()
            .child(h_flex().gap_1().flex_wrap().items_center().children(chips).child(add))
            .children(picker)
            .into_any_element()
    }

    /// Inline textarea with Send/Save and Cancel.
    fn editor_box(&self, editor: &super::view::Editor, lang: Language, cx: &mut Context<Self>) -> AnyElement {
        let editing = matches!(editor.target, Target::Edit(_));
        let sending = editor.sending;
        v_flex()
            .gap_1()
            .child(Textarea::new(&editor.state).small().aria_label(tr(lang, Tr::CommentsPlaceholder)))
            .child(
                h_flex()
                    .gap_1()
                    .justify_end()
                    .child(
                        Button::new("pc-editor-cancel")
                            .ghost()
                            .xsmall()
                            .label(tr(lang, Tr::Cancel))
                            .on_click(cx.listener(|this, _, _, cx| this.close_editor(cx))),
                    )
                    .child(
                        Button::new("pc-editor-send")
                            .primary()
                            .xsmall()
                            .loading(sending)
                            .label(tr(lang, if editing { Tr::CommentsSave } else { Tr::CommentsSend }))
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
            .into_any_element()
    }
}
