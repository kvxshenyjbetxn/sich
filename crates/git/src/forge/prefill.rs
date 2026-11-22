use super::repo_ext::CommitMessage;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrDraft {
    pub title: String,
    pub body: String,
}

/// Builds the initial title and body of a new pull request. `commits` are oldest first.
pub fn pr_prefill(branch: &str, commits: &[CommitMessage], template: Option<&str>) -> PrDraft {
    let (title, mut body) = match commits {
        [single] => (single.subject.trim().to_owned(), single.body.trim().to_owned()),
        _ => (
            humanize_branch(branch),
            commits
                .iter()
                .map(|commit| commit.subject.trim())
                .filter(|subject| !subject.is_empty())
                .map(|subject| format!("- {subject}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    };
    if let Some(template) = template.map(str::trim).filter(|t| !t.is_empty()) {
        body = if body.is_empty() {
            template.to_owned()
        } else {
            format!("{template}\n\n{body}")
        };
    }
    PrDraft { title, body }
}

fn humanize_branch(branch: &str) -> String {
    let tail = branch.rsplit('/').next().unwrap_or(branch);
    let spaced = tail.replace(['-', '_'], " ");
    let collapsed = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str, body: &str) -> CommitMessage {
        CommitMessage {
            subject: subject.into(),
            body: body.into(),
        }
    }

    #[test]
    fn single_commit_uses_subject_and_body() {
        let draft = pr_prefill("feature/x", &[commit("Fix crash", "\nDetails here.\n")], None);
        assert_eq!(draft.title, "Fix crash");
        assert_eq!(draft.body, "Details here.");
    }

    #[test]
    fn several_commits_use_branch_title_and_bullets() {
        let commits = [commit("One", "ignored body"), commit("Two", "")];
        let draft = pr_prefill("feature/add-pr_card", &commits, None);
        assert_eq!(draft.title, "Add pr card");
        assert_eq!(draft.body, "- One\n- Two");
    }

    #[test]
    fn zero_commits_use_branch_title_and_empty_body() {
        let draft = pr_prefill("fix-the--bug", &[], None);
        assert_eq!(draft.title, "Fix the bug");
        assert_eq!(draft.body, "");
    }

    #[test]
    fn template_is_prepended_to_commit_body() {
        let commits = [commit("A", "why")];
        let with = pr_prefill("b", &commits, Some("## Summary\n\n"));
        assert_eq!(with.body, "## Summary\n\nwhy");
        let without = pr_prefill("b", &[commit("A", "")], Some("## Summary"));
        assert_eq!(without.body, "## Summary");
        let empty = pr_prefill("b", &commits, Some("  \n"));
        assert_eq!(empty.body, "why");
    }

    #[test]
    fn odd_branch_names_do_not_panic() {
        assert_eq!(pr_prefill("a/", &[], None).title, "");
        assert_eq!(pr_prefill("---", &[], None).title, "");
        assert_eq!(pr_prefill("", &[], None).title, "");
        assert_eq!(pr_prefill("фіча/нова-річ", &[], None).title, "Нова річ");
        assert_eq!(pr_prefill("ßeta", &[], None).title, "SSeta");
    }
}
