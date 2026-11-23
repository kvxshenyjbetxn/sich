//! UI translations, checked at compile time.
//!
//! To add a string: add a variant to [`Tr`], then add its arm to every language in [`tr`]
//! (the exhaustive matches turn a missing translation into a compile error). Use it in the UI
//! as `tr(lang, Tr::Variant)` with `lang = settings::language(cx)`. Strings with parameters
//! get a small helper fn (see [`connected`]) instead of a variant.

use serde::{Deserialize, Serialize};
use sich_git::compare::LineEndings;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Uk,
    Ru,
    En,
}

impl Language {
    pub const ALL: [Language; 3] = [Language::Uk, Language::Ru, Language::En];

    /// Name of the language in that language.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::Uk => "Українська",
            Language::Ru => "Русский",
            Language::En => "English",
        }
    }

    /// Best guess from the system locale (e.g. "uk-UA"); English when unknown.
    pub fn from_locale(locale: Option<&str>) -> Self {
        let l = locale.unwrap_or("").to_ascii_lowercase();
        if l.starts_with("uk") {
            Language::Uk
        } else if l.starts_with("ru") {
            Language::Ru
        } else {
            Language::En
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tr {
    NewTerminal,
    Reconnecting,
    Settings,
    Language,
    Theme,
    ThemeGray,
    ThemeAmoled,
    CloseTerminal,
    SplitRight,
    SplitDown,
    DetachPane,
    DropPaneHere,
    TerminalSplitFailed,
    TerminalSplitDisconnected,
    QuickCommands,
    QuickCommandsEmpty,
    QuickCommandName,
    QuickCommandCommand,
    QuickCommandAdd,
    QuickCommandEdit,
    QuickCommandManage,
    QuickCommandSave,
    QuickCommandOpenFailed,
    QuickCommandRunning,
    QuickCommandFinished,
    NoTerminals,
    Exited,
    ProcessExited,
    CloseFromSidebar,
    Projects,
    Home,
    AddProject,
    Refresh,
    NewWorktree,
    ExternalWorktreeCount,
    ImportExternalWorktrees,
    HideExternalWorktrees,
    ShowExternalWorktrees,
    MoreActions,
    OpenInExplorer,
    CopyPath,
    RemoveProject,
    RemoveWorktree,
    MainTag,
    UncommittedChanges,
    NewTerminalHere,
    NoProjectsHint,
    NameLabel,
    BaseLabel,
    Cancel,
    Create,
    Remove,
    RemoveAnyway,
    DirtyWarning,
    RestartDaemon,
    RestartAllLegacyTerminals,
    BatchTerminalRestartFailed,
    DaemonUpdateRequired,
    DaemonRestarting,
    RestartLegacyTerminal,
    TerminalRestarting,
    TerminalRestartFailed,
    RestoredScrollback,
    AgentInterrupted,
    ResumeAgentsOnRestore,
    AgentWorking,
    AgentWaiting,
    AgentDone,
    AgentFailed,
    AgentsSection,
    TrackClaudeHooks,
    HooksNotInstalled,
    HooksWorking,
    TrackCodexHooks,
    CodexNotFound,
    CodexTooOld,
    CodexTrustPending,
    CodexTrustCleanupPending,
    TrackOmpHooks,
    OmpInstalled,
    OmpNotFound,
    OmpTooOld,
    UsageLimits,
    UsageTitle,
    UsageFiveHourDetail,
    UsageWeekDetail,
    UsageNoReading,
    ClaudeStatuslineWrap,
    UsageFiveHour,
    UsageWeek,
    UsageStale,
    UsageResetPassed,
    UsageClaudeNeedsWrap,
    UsageClaudeEdited,
    UsageClaudeOld,
    UsageClaudeFailed,
    UsageCodexOld,
    UsageCodexUnsupported,
    UsageCodexNoLimits,
    UsageCodexFailed,
    OrchestrationSkill,
    OrchUnattendedWorkers,
    OrchCoordinator,
    OrchWorker,
    OrchAutoCloseWorkers,
    OrchCleanupWorktrees,
    OrchTemporary,
    OrchAutoCloseRunning,
    OrchAutoCloseFinished,
    ActivityAutoCloseRunning,
    ActivityAutoCloseFinished,
    OrchKeepOpen,
    OrchAllowAutoClose,
    NewGroup,
    GroupName,
    RenameGroup,
    Rename,
    DeleteGroup,
    DeleteGroupConfirm,
    AddProjectToGroup,
    MoveToGroup,
    RemoveFromGroup,
    CloseToTray,
    Notifications,
    NotificationsEnabled,
    NotifyAgentNeedsInput,
    NotifyAgentDone,
    NotifySound,
    TestNotification,
    TestNotificationBody,
    ToastNeedsInput,
    ToastDone,
    ToastFailed,
    ToastAgent,
    Close,
    Update,
    UpdateTitle,
    Later,
    Downloading,
    Verifying,
    Installing,
    CheckUpdates,
    CheckNow,
    CheckingUpdates,
    UpdatesDevBuild,
    NoWriteAccess,
    TrayShow,
    TrayQuit,
    TrayRestart,
    QuitConfirmTitle,
    QuitConfirmText,
    Quit,
    TrayWaiting,
    TrayWorking,
    Git,
    GitPanel,
    GitSelectProject,
    GitNotRepo,
    GitMissing,
    GitLoading,
    GitDetached,
    GitNewBranch,
    GitBranchName,
    GitSwitchBranch,
    GitRemoteBranches,
    GitFilterBranches,
    GitFetch,
    GitPull,
    GitPush,
    GitDismiss,
    GitCommitMessage,
    GitCommit,
    GitCommitAll,
    GitConflicts,
    GitStaged,
    GitChanges,
    GitStage,
    GitUnstage,
    GitStageAll,
    GitUnstageAll,
    GitDiscard,
    GitDiscardAll,
    GitDiscardWarning,
    GitDiscardAllWarning,
    GitNoChanges,
    GitTruncated,
    GitNoDiff,
    DiffWorking,
    DiffStaged,
    DiffConflict,
    DiffClose,
    ActivityOpen,
    ActivityTab,
    ActivityClose,
    ActivityLoading,
    ActivityLive,
    ActivityFinished,
    ActivityUnavailable,
    ActivityEmpty,
    ActivityGap,
    ActivityStarted,
    ActivityToolStarted,
    ActivityToolFinished,
    ActivityToolFailed,
    ActivityMessage,
    ActivityAborted,
    ActivityEnded,
    ActivityTimeline,
    ActivityTerminal,
    DiffIndex,
    DiffHead,
    DiffBefore,
    DiffAfter,
    DiffOurs,
    DiffTheirs,
    DiffSideBySide,
    DiffInline,
    DiffWrapLines,
    DiffPrevious,
    DiffNext,
    DiffBinary,
    DiffTooLarge,
    DiffEolMixed,
    DiffEolNone,
    GitCommits,
    GitNoCommits,
    GitLoadMore,
    GitPullRebase,
    GitForcePush,
    GitForcePushConfirm,
    GitStashAll,
    GitStashChanges,
    GitRefreshPr,
    GitRebaseInProgress,
    GitMergeInProgress,
    GitCherryPickInProgress,
    GitRevertInProgress,
    GitResolveHint,
    GitContinue,
    GitAbort,
    GitAbortConfirm,
    GitCommitPush,
    GitAmend,
    GitUndoCommit,
    GitAlreadyPushed,
    GitAlreadyPushedUndo,
    GitCommitMore,
    GitStashes,
    GitStashApply,
    GitStashPop,
    GitStashDrop,
    GitStashDropConfirm,
    PrChecking,
    PrInstallGh,
    PrNotSignedIn,
    PrCopyCommand,
    PrRetry,
    PrCreate,
    PrAheadOne,
    PrAhead,
    PrWillPush,
    PrNoCommits,
    PrFinishOperation,
    PrStateOpen,
    PrStateDraft,
    PrStateMerged,
    PrStateClosed,
    PrOpenInBrowser,
    PrChecksFailing,
    PrChecksRunning,
    PrChecksPassed,
    PrReviewApproved,
    PrReviewChanges,
    PrReviewRequired,
    PrMergeConflicts,
    PrMergeBehind,
    PrMergeBlocked,
    PrMergeReadyChecksFailing,
    PrMergeReady,
    PrRerunFailed,
    PrSendToAgent,
    PrSendFailuresToAgent,
    PrNoAgentTerminal,
    PrNotPushed,
    PrUnpushedOne,
    PrUnpushed,
    PrPushButton,
    PrMergeSquash,
    PrMergeMerge,
    PrMergeRebase,
    PrMergeConfirmTitle,
    PrMergeConfirm,
    PrReasonDraft,
    PrReasonConflicts,
    PrReasonBlocked,
    PrReasonPush,
    PrCopyLink,
    PrMarkReady,
    PrConvertDraft,
    PrClose,
    PrCloseConfirm,
    PrReopen,
    PrTitleLabel,
    PrDescriptionLabel,
    PrBaseLabel,
    PrDraftCheckbox,
    PrWillBePushed,
    PrUncommitted,
    PrTabLabel,
    PrTipChecksPassing,
    PrTipChecksRunning,
    PrTipCheckFailingOne,
    PrTipChecksFailing,
    GithubPrs,
    GithubPrsDesc,
    WorktreeLocation,
    WorktreeLocSibling,
    WorktreeLocInside,
    WorktreeLocShared,
    WorktreeLocSiblingDesc,
    WorktreeLocInsideDesc,
    WorktreeLocSharedDesc,
    // ---- comments keys: append below ----
    CommentsTitle,
    CommentsSendUnresolved,
    CommentsModeGrouped,
    CommentsModeTimeline,
    CommentsAdd,
    CommentsFilterAll,
    CommentsFilterHumans,
    CommentsFilterBots,
    CommentsNeedsReview,
    CommentsConversation,
    CommentsResolved,
    CommentsTimeline,
    CommentsOutdated,
    CommentsBot,
    CommentsReply,
    CommentsResolve,
    CommentsUnresolve,
    CommentsEdit,
    CommentsDelete,
    CommentsDeleteWarning,
    CommentsSend,
    CommentsSave,
    CommentsPlaceholder,
    CommentsEmpty,
    CommentsEmptyHumans,
    CommentsEmptyBots,
    CommentsTruncated,
    CommentsLoading,
    CommentsAddReaction,
    // ---- end comments ----

    // ---- checks keys: append below ----
    PrUpdated,
    PrMergeCommit,
    PrAutoMergeEnable,
    PrAutoMergeDisable,
    PrStatusAutoMergeOn,
    PrStatusChecksFailed,
    PrStatusChecksPending,
    PrStatusAble,
    PrStatusChecking,
    PrReadyForReview,
    PrCloseShort,
    PrCheckPassing,
    PrCheckFailing,
    PrCheckPendingN,
    PrCheckSkipped,
    PrChecksNone,
    PrRerunAll,
    PrCheckStarted,
    PrCheckFinished,
    PrCheckApproval,
    PrCheckFailedJobs,
    PrCheckFailedSteps,
    PrCheckOutput,
    PrCheckAnnotations,
    PrCheckNoDetails,
    PrCheckLoading,
    PrCheckCopyLog,
    PrCheckNoLog,
    PrConflictsTitle,
    PrConflictsBehind,
    PrConflictsBehindOne,
    PrConflictsNotReproduced,
    PrConflictsSend,
    // ---- end checks ----

    // ---- details keys: append below ----
    DetTitle,
    DetOpenedBy,
    DetNoDescription,
    DetShowMore,
    DetShowLess,
    DetEdit,
    DetReviewers,
    DetLabels,
    DetAssignees,
    DetAdd,
    DetRemove,
    DetNone,
    DetLoading,
    DetReviewApproved,
    DetReviewChanges,
    DetReviewCommented,
    DetReviewPending,
    DetEditTitle,
    DetSave,
    DetPickReviewer,
    DetPickLabel,
    DetPickAssignee,
    DetPickerFilter,
    DetPickerCustom,
    DetPickerNoMatches,
    FilesTitle,
    FilesViewed,
    FilesFilter,
    FilesFetchHint,
    FilesLoading,
    FilesNone,
    FilesMarkViewed,
    // ---- end details ----

    // ---- history keys: append below ----
    GitCopySha,
    GitCopyMessage,
    GitOpenCommitOnGitHub,
    GitCherryPick,
    GitCherryPickConfirm,
    GitRevert,
    GitRevertConfirm,
    GitNothingToApply,
    // ---- end history ----

    // ---- compare keys: append below ----
    CmpVs,
    CmpAheadOf,
    CmpBehindOf,
    CmpBaseTooltip,
    CmpChangeBase,
    CmpBaseMissing,
    CmpNoCommonHistory,
    CmpResetBase,
    CmpBranchChanges,
    CmpMergeConflicts,
    CmpRebaseConflicts,
    CmpCherryPickConflicts,
    CmpRevertConflicts,
    CmpUnresolved,
    CmpResolveWithAgent,
    CmpAbortMerge,
    CmpAbortRebase,
    CmpAbortCherryPick,
    CmpAbortRevert,
    CmpBothModified,
    CmpBothAdded,
    CmpBothDeleted,
    CmpAddedByUs,
    CmpAddedByThem,
    CmpDeletedByUs,
    CmpDeletedByThem,
    CmpMarkResolved,
    CmpKeepFile,
    CmpKeepDeletion,
    CmpCompleteMerge,
    CmpAlreadyApplied,
    // ---- end compare ----

    // ---- prwt keys: append below ----
    // ---- end prwt ----

    // ---- notes keys: append below ----
    // ---- end notes ----
}

impl Tr {
    /// Every key (used by the completeness test).
    #[cfg(test)]
    pub const ALL: &'static [Tr] = &[
        Tr::NewTerminal,
        Tr::Reconnecting,
        Tr::Settings,
        Tr::Language,
        Tr::Theme,
        Tr::ThemeGray,
        Tr::ThemeAmoled,
        Tr::CloseTerminal,
        Tr::SplitRight,
        Tr::SplitDown,
        Tr::DetachPane,
        Tr::DropPaneHere,
        Tr::TerminalSplitFailed,
        Tr::TerminalSplitDisconnected,
        Tr::QuickCommands,
        Tr::QuickCommandsEmpty,
        Tr::QuickCommandName,
        Tr::QuickCommandCommand,
        Tr::QuickCommandAdd,
        Tr::QuickCommandEdit,
        Tr::QuickCommandManage,
        Tr::QuickCommandSave,
        Tr::QuickCommandOpenFailed,
        Tr::QuickCommandRunning,
        Tr::QuickCommandFinished,
        Tr::NoTerminals,
        Tr::Exited,
        Tr::ProcessExited,
        Tr::CloseFromSidebar,
        Tr::Projects,
        Tr::Home,
        Tr::AddProject,
        Tr::Refresh,
        Tr::NewWorktree,
        Tr::ExternalWorktreeCount,
        Tr::ImportExternalWorktrees,
        Tr::HideExternalWorktrees,
        Tr::ShowExternalWorktrees,
        Tr::MoreActions,
        Tr::OpenInExplorer,
        Tr::CopyPath,
        Tr::RemoveProject,
        Tr::RemoveWorktree,
        Tr::MainTag,
        Tr::UncommittedChanges,
        Tr::NewTerminalHere,
        Tr::NoProjectsHint,
        Tr::NameLabel,
        Tr::BaseLabel,
        Tr::Cancel,
        Tr::Create,
        Tr::Remove,
        Tr::RemoveAnyway,
        Tr::DirtyWarning,
        Tr::RestartDaemon,
        Tr::RestartAllLegacyTerminals,
        Tr::BatchTerminalRestartFailed,
        Tr::DaemonUpdateRequired,
        Tr::DaemonRestarting,
        Tr::RestartLegacyTerminal,
        Tr::TerminalRestarting,
        Tr::TerminalRestartFailed,
        Tr::RestoredScrollback,
        Tr::AgentInterrupted,
        Tr::ResumeAgentsOnRestore,
        Tr::AgentWorking,
        Tr::AgentWaiting,
        Tr::AgentDone,
        Tr::AgentFailed,
        Tr::AgentsSection,
        Tr::TrackClaudeHooks,
        Tr::HooksNotInstalled,
        Tr::HooksWorking,
        Tr::TrackCodexHooks,
        Tr::CodexNotFound,
        Tr::CodexTooOld,
        Tr::CodexTrustPending,
        Tr::CodexTrustCleanupPending,
        Tr::TrackOmpHooks,
        Tr::OmpInstalled,
        Tr::OmpNotFound,
        Tr::OmpTooOld,
        Tr::UsageLimits,
        Tr::UsageTitle,
        Tr::UsageFiveHourDetail,
        Tr::UsageWeekDetail,
        Tr::UsageNoReading,
        Tr::ClaudeStatuslineWrap,
        Tr::UsageFiveHour,
        Tr::UsageWeek,
        Tr::UsageStale,
        Tr::UsageResetPassed,
        Tr::UsageClaudeNeedsWrap,
        Tr::UsageClaudeEdited,
        Tr::UsageClaudeOld,
        Tr::UsageClaudeFailed,
        Tr::UsageCodexOld,
        Tr::UsageCodexUnsupported,
        Tr::UsageCodexNoLimits,
        Tr::UsageCodexFailed,
        Tr::OrchestrationSkill,
        Tr::OrchUnattendedWorkers,
        Tr::OrchCoordinator,
        Tr::OrchWorker,
        Tr::OrchAutoCloseWorkers,
        Tr::OrchCleanupWorktrees,
        Tr::OrchTemporary,
        Tr::OrchAutoCloseRunning,
        Tr::OrchAutoCloseFinished,
        Tr::ActivityAutoCloseRunning,
        Tr::ActivityAutoCloseFinished,
        Tr::OrchKeepOpen,
        Tr::OrchAllowAutoClose,
        Tr::NewGroup,
        Tr::GroupName,
        Tr::RenameGroup,
        Tr::Rename,
        Tr::DeleteGroup,
        Tr::DeleteGroupConfirm,
        Tr::AddProjectToGroup,
        Tr::MoveToGroup,
        Tr::RemoveFromGroup,
        Tr::CloseToTray,
        Tr::Notifications,
        Tr::NotificationsEnabled,
        Tr::NotifyAgentNeedsInput,
        Tr::NotifyAgentDone,
        Tr::NotifySound,
        Tr::TestNotification,
        Tr::TestNotificationBody,
        Tr::ToastNeedsInput,
        Tr::ToastDone,
        Tr::ToastFailed,
        Tr::ToastAgent,
        Tr::Close,
        Tr::Update,
        Tr::UpdateTitle,
        Tr::Later,
        Tr::Downloading,
        Tr::Verifying,
        Tr::Installing,
        Tr::CheckUpdates,
        Tr::CheckNow,
        Tr::CheckingUpdates,
        Tr::UpdatesDevBuild,
        Tr::NoWriteAccess,
        Tr::TrayShow,
        Tr::TrayQuit,
        Tr::TrayRestart,
        Tr::QuitConfirmTitle,
        Tr::QuitConfirmText,
        Tr::Quit,
        Tr::TrayWaiting,
        Tr::TrayWorking,
        Tr::Git,
        Tr::GitPanel,
        Tr::GitSelectProject,
        Tr::GitNotRepo,
        Tr::GitMissing,
        Tr::GitLoading,
        Tr::GitDetached,
        Tr::GitNewBranch,
        Tr::GitBranchName,
        Tr::GitSwitchBranch,
        Tr::GitRemoteBranches,
        Tr::GitFilterBranches,
        Tr::GitFetch,
        Tr::GitPull,
        Tr::GitPush,
        Tr::GitDismiss,
        Tr::GitCommitMessage,
        Tr::GitCommit,
        Tr::GitCommitAll,
        Tr::GitConflicts,
        Tr::GitStaged,
        Tr::GitChanges,
        Tr::GitStage,
        Tr::GitUnstage,
        Tr::GitStageAll,
        Tr::GitUnstageAll,
        Tr::GitDiscard,
        Tr::GitDiscardAll,
        Tr::GitDiscardWarning,
        Tr::GitDiscardAllWarning,
        Tr::GitNoChanges,
        Tr::GitTruncated,
        Tr::GitNoDiff,
        Tr::DiffWorking,
        Tr::DiffStaged,
        Tr::DiffConflict,
        Tr::DiffClose,
        Tr::ActivityOpen,
        Tr::ActivityTab,
        Tr::ActivityClose,
        Tr::ActivityLoading,
        Tr::ActivityLive,
        Tr::ActivityFinished,
        Tr::ActivityUnavailable,
        Tr::ActivityEmpty,
        Tr::ActivityGap,
        Tr::ActivityStarted,
        Tr::ActivityToolStarted,
        Tr::ActivityToolFinished,
        Tr::ActivityToolFailed,
        Tr::ActivityMessage,
        Tr::ActivityAborted,
        Tr::ActivityEnded,
        Tr::ActivityTimeline,
        Tr::ActivityTerminal,
        Tr::DiffIndex,
        Tr::DiffHead,
        Tr::DiffBefore,
        Tr::DiffAfter,
        Tr::DiffOurs,
        Tr::DiffTheirs,
        Tr::DiffSideBySide,
        Tr::DiffInline,
        Tr::DiffWrapLines,
        Tr::DiffPrevious,
        Tr::DiffNext,
        Tr::DiffBinary,
        Tr::DiffTooLarge,
        Tr::DiffEolMixed,
        Tr::DiffEolNone,
        Tr::GitCommits,
        Tr::GitNoCommits,
        Tr::GitLoadMore,
        Tr::GitPullRebase,
        Tr::GitForcePush,
        Tr::GitForcePushConfirm,
        Tr::GitStashAll,
        Tr::GitStashChanges,
        Tr::GitRefreshPr,
        Tr::GitRebaseInProgress,
        Tr::GitMergeInProgress,
        Tr::GitCherryPickInProgress,
        Tr::GitRevertInProgress,
        Tr::GitResolveHint,
        Tr::GitContinue,
        Tr::GitAbort,
        Tr::GitAbortConfirm,
        Tr::GitCommitPush,
        Tr::GitAmend,
        Tr::GitUndoCommit,
        Tr::GitAlreadyPushed,
        Tr::GitAlreadyPushedUndo,
        Tr::GitCommitMore,
        Tr::GitStashes,
        Tr::GitStashApply,
        Tr::GitStashPop,
        Tr::GitStashDrop,
        Tr::GitStashDropConfirm,
        Tr::PrChecking,
        Tr::PrInstallGh,
        Tr::PrNotSignedIn,
        Tr::PrCopyCommand,
        Tr::PrRetry,
        Tr::PrCreate,
        Tr::PrAheadOne,
        Tr::PrAhead,
        Tr::PrWillPush,
        Tr::PrNoCommits,
        Tr::PrFinishOperation,
        Tr::PrStateOpen,
        Tr::PrStateDraft,
        Tr::PrStateMerged,
        Tr::PrStateClosed,
        Tr::PrOpenInBrowser,
        Tr::PrChecksFailing,
        Tr::PrChecksRunning,
        Tr::PrChecksPassed,
        Tr::PrReviewApproved,
        Tr::PrReviewChanges,
        Tr::PrReviewRequired,
        Tr::PrMergeConflicts,
        Tr::PrMergeBehind,
        Tr::PrMergeBlocked,
        Tr::PrMergeReadyChecksFailing,
        Tr::PrMergeReady,
        Tr::PrRerunFailed,
        Tr::PrSendToAgent,
        Tr::PrSendFailuresToAgent,
        Tr::PrNoAgentTerminal,
        Tr::PrNotPushed,
        Tr::PrUnpushedOne,
        Tr::PrUnpushed,
        Tr::PrPushButton,
        Tr::PrMergeSquash,
        Tr::PrMergeMerge,
        Tr::PrMergeRebase,
        Tr::PrMergeConfirmTitle,
        Tr::PrMergeConfirm,
        Tr::PrReasonDraft,
        Tr::PrReasonConflicts,
        Tr::PrReasonBlocked,
        Tr::PrReasonPush,
        Tr::PrCopyLink,
        Tr::PrMarkReady,
        Tr::PrConvertDraft,
        Tr::PrClose,
        Tr::PrCloseConfirm,
        Tr::PrReopen,
        Tr::PrTitleLabel,
        Tr::PrDescriptionLabel,
        Tr::PrBaseLabel,
        Tr::PrDraftCheckbox,
        Tr::PrWillBePushed,
        Tr::PrUncommitted,
        Tr::PrTabLabel,
        Tr::PrTipChecksPassing,
        Tr::PrTipChecksRunning,
        Tr::PrTipCheckFailingOne,
        Tr::PrTipChecksFailing,
        Tr::GithubPrs,
        Tr::GithubPrsDesc,
        Tr::WorktreeLocation,
        Tr::WorktreeLocSibling,
        Tr::WorktreeLocInside,
        Tr::WorktreeLocShared,
        Tr::WorktreeLocSiblingDesc,
        Tr::WorktreeLocInsideDesc,
        Tr::WorktreeLocSharedDesc,
        // ---- comments keys: append below ----
        Tr::CommentsTitle,
        Tr::CommentsSendUnresolved,
        Tr::CommentsModeGrouped,
        Tr::CommentsModeTimeline,
        Tr::CommentsAdd,
        Tr::CommentsFilterAll,
        Tr::CommentsFilterHumans,
        Tr::CommentsFilterBots,
        Tr::CommentsNeedsReview,
        Tr::CommentsConversation,
        Tr::CommentsResolved,
        Tr::CommentsTimeline,
        Tr::CommentsOutdated,
        Tr::CommentsBot,
        Tr::CommentsReply,
        Tr::CommentsResolve,
        Tr::CommentsUnresolve,
        Tr::CommentsEdit,
        Tr::CommentsDelete,
        Tr::CommentsDeleteWarning,
        Tr::CommentsSend,
        Tr::CommentsSave,
        Tr::CommentsPlaceholder,
        Tr::CommentsEmpty,
        Tr::CommentsEmptyHumans,
        Tr::CommentsEmptyBots,
        Tr::CommentsTruncated,
        Tr::CommentsLoading,
        Tr::CommentsAddReaction,
        // ---- end comments ----

        // ---- checks keys: append below ----
        Tr::PrUpdated,
        Tr::PrMergeCommit,
        Tr::PrAutoMergeEnable,
        Tr::PrAutoMergeDisable,
        Tr::PrStatusAutoMergeOn,
        Tr::PrStatusChecksFailed,
        Tr::PrStatusChecksPending,
        Tr::PrStatusAble,
        Tr::PrStatusChecking,
        Tr::PrReadyForReview,
        Tr::PrCloseShort,
        Tr::PrCheckPassing,
        Tr::PrCheckFailing,
        Tr::PrCheckPendingN,
        Tr::PrCheckSkipped,
        Tr::PrChecksNone,
        Tr::PrRerunAll,
        Tr::PrCheckStarted,
        Tr::PrCheckFinished,
        Tr::PrCheckApproval,
        Tr::PrCheckFailedJobs,
        Tr::PrCheckFailedSteps,
        Tr::PrCheckOutput,
        Tr::PrCheckAnnotations,
        Tr::PrCheckNoDetails,
        Tr::PrCheckLoading,
        Tr::PrCheckCopyLog,
        Tr::PrCheckNoLog,
        Tr::PrConflictsTitle,
        Tr::PrConflictsBehind,
        Tr::PrConflictsBehindOne,
        Tr::PrConflictsNotReproduced,
        Tr::PrConflictsSend,
        // ---- end checks ----

        // ---- details keys: append below ----
        Tr::DetTitle,
        Tr::DetOpenedBy,
        Tr::DetNoDescription,
        Tr::DetShowMore,
        Tr::DetShowLess,
        Tr::DetEdit,
        Tr::DetReviewers,
        Tr::DetLabels,
        Tr::DetAssignees,
        Tr::DetAdd,
        Tr::DetRemove,
        Tr::DetNone,
        Tr::DetLoading,
        Tr::DetReviewApproved,
        Tr::DetReviewChanges,
        Tr::DetReviewCommented,
        Tr::DetReviewPending,
        Tr::DetEditTitle,
        Tr::DetSave,
        Tr::DetPickReviewer,
        Tr::DetPickLabel,
        Tr::DetPickAssignee,
        Tr::DetPickerFilter,
        Tr::DetPickerCustom,
        Tr::DetPickerNoMatches,
        Tr::FilesTitle,
        Tr::FilesViewed,
        Tr::FilesFilter,
        Tr::FilesFetchHint,
        Tr::FilesLoading,
        Tr::FilesNone,
        Tr::FilesMarkViewed,
        // ---- end details ----

        // ---- history keys: append below ----
        Tr::GitCopySha,
        Tr::GitCopyMessage,
        Tr::GitOpenCommitOnGitHub,
        Tr::GitCherryPick,
        Tr::GitCherryPickConfirm,
        Tr::GitRevert,
        Tr::GitRevertConfirm,
        Tr::GitNothingToApply,
        // ---- end history ----

        // ---- compare keys: append below ----
        Tr::CmpVs,
        Tr::CmpAheadOf,
        Tr::CmpBehindOf,
        Tr::CmpBaseTooltip,
        Tr::CmpChangeBase,
        Tr::CmpBaseMissing,
        Tr::CmpNoCommonHistory,
        Tr::CmpResetBase,
        Tr::CmpBranchChanges,
        Tr::CmpMergeConflicts,
        Tr::CmpRebaseConflicts,
        Tr::CmpCherryPickConflicts,
        Tr::CmpRevertConflicts,
        Tr::CmpUnresolved,
        Tr::CmpResolveWithAgent,
        Tr::CmpAbortMerge,
        Tr::CmpAbortRebase,
        Tr::CmpAbortCherryPick,
        Tr::CmpAbortRevert,
        Tr::CmpBothModified,
        Tr::CmpBothAdded,
        Tr::CmpBothDeleted,
        Tr::CmpAddedByUs,
        Tr::CmpAddedByThem,
        Tr::CmpDeletedByUs,
        Tr::CmpDeletedByThem,
        Tr::CmpMarkResolved,
        Tr::CmpKeepFile,
        Tr::CmpKeepDeletion,
        Tr::CmpCompleteMerge,
        Tr::CmpAlreadyApplied,
        // ---- end compare ----

        // ---- prwt keys: append below ----
        // ---- end prwt ----

        // ---- notes keys: append below ----
        // ---- end notes ----
    ];
}

pub fn tr(lang: Language, key: Tr) -> &'static str {
    use Language::*;
    match key {
        Tr::ExternalWorktreeCount => match lang {
            Uk => "{n} зовнішніх worktree",
            Ru => "{n} внешних worktree",
            En => "{n} external worktrees",
        },
        Tr::ImportExternalWorktrees => match lang {
            Uk => "Імпортувати",
            Ru => "Импортировать",
            En => "Import",
        },
        Tr::HideExternalWorktrees => match lang {
            Uk => "Сховати",
            Ru => "Скрыть",
            En => "Hide",
        },
        Tr::ShowExternalWorktrees => match lang {
            Uk => "Показувати всі worktree (і створені поза Січчю)",
            Ru => "Показывать все worktree (и созданные вне Сичи)",
            En => "Show all worktrees (including ones created outside Sich)",
        },
        Tr::GitNewBranch => match lang {
            Uk => "Нова гілка...",
            Ru => "Новая ветка...",
            En => "New branch...",
        },
        Tr::GitBranchName => match lang {
            Uk => "Назва гілки",
            Ru => "Имя ветки",
            En => "Branch name",
        },
        Tr::GitSwitchBranch => match lang {
            Uk => "Перемкнути",
            Ru => "Переключить",
            En => "Switch",
        },
        Tr::GitRemoteBranches => match lang {
            Uk => "Віддалені",
            Ru => "Удалённые",
            En => "Remote",
        },
        Tr::GitFilterBranches => match lang {
            Uk => "Фільтр гілок",
            Ru => "Фильтр веток",
            En => "Filter branches",
        },
        Tr::Git => match lang {
            Uk => "Git",
            Ru => "Git",
            En => "Git",
        },
        Tr::GitPanel => match lang {
            Uk => "Панель Git",
            Ru => "Панель Git",
            En => "Git panel",
        },
        Tr::GitSelectProject => match lang {
            Uk => "Виберіть проєкт",
            Ru => "Выберите проект",
            En => "Select a project",
        },
        Tr::GitNotRepo => match lang {
            Uk => "Це не репозиторій Git",
            Ru => "Это не репозиторий Git",
            En => "Not a Git repository",
        },
        Tr::GitMissing => match lang {
            Uk => "Git не встановлено",
            Ru => "Git не установлен",
            En => "Git is not installed",
        },
        Tr::GitLoading => match lang {
            Uk => "Завантаження…",
            Ru => "Загрузка…",
            En => "Loading…",
        },
        Tr::GitDetached => match lang {
            Uk => "від’єднано",
            Ru => "отсоединён",
            En => "detached",
        },
        Tr::GitFetch => match lang {
            Uk => "Отримати зміни (fetch)",
            Ru => "Получить изменения (fetch)",
            En => "Fetch",
        },
        Tr::GitPull => match lang {
            Uk => "Завантажити зміни (pull)",
            Ru => "Загрузить изменения (pull)",
            En => "Pull",
        },
        Tr::GitPush => match lang {
            Uk => "Надіслати зміни (push)",
            Ru => "Отправить изменения (push)",
            En => "Push",
        },
        Tr::GitDismiss => match lang {
            Uk => "Закрити повідомлення",
            Ru => "Закрыть сообщение",
            En => "Dismiss",
        },
        Tr::GitCommitMessage => match lang {
            Uk => "Повідомлення коміту (Ctrl+Enter)",
            Ru => "Сообщение коммита (Ctrl+Enter)",
            En => "Commit message (Ctrl+Enter)",
        },
        Tr::GitCommit => match lang {
            Uk => "Створити коміт",
            Ru => "Создать коммит",
            En => "Commit",
        },
        Tr::GitCommitAll => match lang {
            Uk => "Закомітити все",
            Ru => "Закоммитить всё",
            En => "Commit all",
        },
        Tr::GitConflicts => match lang {
            Uk => "Конфлікти злиття",
            Ru => "Конфликты слияния",
            En => "Merge conflicts",
        },
        Tr::GitStaged => match lang {
            Uk => "Підготовлені",
            Ru => "Подготовленные",
            En => "Staged",
        },
        Tr::GitChanges => match lang {
            Uk => "Зміни",
            Ru => "Изменения",
            En => "Changes",
        },
        Tr::GitStage => match lang {
            Uk => "Додати до індексу",
            Ru => "Добавить в индекс",
            En => "Stage",
        },
        Tr::GitUnstage => match lang {
            Uk => "Прибрати з індексу",
            Ru => "Убрать из индекса",
            En => "Unstage",
        },
        Tr::GitStageAll => match lang {
            Uk => "Додати все до індексу",
            Ru => "Добавить всё в индекс",
            En => "Stage all",
        },
        Tr::GitUnstageAll => match lang {
            Uk => "Прибрати все з індексу",
            Ru => "Убрать всё из индекса",
            En => "Unstage all",
        },
        Tr::GitDiscard => match lang {
            Uk => "Відкинути зміни",
            Ru => "Отменить изменения",
            En => "Discard changes",
        },
        Tr::GitDiscardAll => match lang {
            Uk => "Відкинути всі зміни",
            Ru => "Отменить все изменения",
            En => "Discard all",
        },
        Tr::GitDiscardWarning => match lang {
            Uk => "Непідготовлені зміни у файлі буде втрачено. Файл поза індексом буде видалено.",
            Ru => "Неподготовленные изменения в файле будут потеряны. Неотслеживаемый файл будет удалён.",
            En => "Unstaged edits in this file will be lost. Untracked files will be deleted.",
        },
        Tr::GitDiscardAllWarning => match lang {
            Uk => "Усі непідготовлені зміни буде втрачено. Файли поза індексом буде видалено.",
            Ru => "Все неподготовленные изменения будут потеряны. Неотслеживаемые файлы будут удалены.",
            En => "All unstaged edits will be lost. Untracked files will be deleted.",
        },
        Tr::GitNoChanges => match lang {
            Uk => "Немає змін",
            Ru => "Нет изменений",
            En => "No changes",
        },
        Tr::GitTruncated => match lang {
            Uk => "Забагато змін, показано перші",
            Ru => "Слишком много изменений, показаны первые",
            En => "Too many changes, showing the first",
        },
        Tr::GitNoDiff => match lang {
            Uk => "Немає різниці",
            Ru => "Нет различий",
            En => "No diff",
        },
        Tr::DiffWorking => match lang {
            Uk => "Робочі зміни",
            Ru => "Рабочие изменения",
            En => "Working",
        },
        Tr::DiffStaged => match lang {
            Uk => "В індексі",
            Ru => "В индексе",
            En => "Staged",
        },
        Tr::DiffConflict => match lang {
            Uk => "Конфлікт",
            Ru => "Конфликт",
            En => "Conflict",
        },
        Tr::DiffClose => match lang {
            Uk => "Закрити порівняння",
            Ru => "Закрыть сравнение",
            En => "Close diff",
        },
        Tr::ActivityOpen => match lang {
            Uk => "Відкрити журнал субагента",
            Ru => "Открыть журнал субагента",
            En => "Open subagent activity",
        },
        Tr::ActivityTab => match lang {
            Uk => "Журнал",
            Ru => "Журнал",
            En => "Activity",
        },
        Tr::ActivityClose => match lang {
            Uk => "Закрити журнал субагента",
            Ru => "Закрыть журнал субагента",
            En => "Close subagent activity",
        },
        Tr::ActivityLoading => match lang {
            Uk => "Завантаження журналу субагента…",
            Ru => "Загрузка журнала субагента…",
            En => "Loading subagent activity…",
        },
        Tr::ActivityLive => match lang {
            Uk => "Оновлюється наживо · Лише читання",
            Ru => "Обновляется в реальном времени · Только чтение",
            En => "Live · Read-only",
        },
        Tr::ActivityFinished => match lang {
            Uk => "Завершено",
            Ru => "Завершено",
            En => "Finished",
        },
        Tr::ActivityUnavailable => match lang {
            Uk => "Журнал цього субагента недоступний.",
            Ru => "Журнал этого субагента недоступен.",
            En => "Activity for this subagent is unavailable.",
        },
        Tr::ActivityEmpty => match lang {
            Uk => "У журналі ще немає подій.",
            Ru => "В журнале пока нет событий.",
            En => "No activity has been recorded.",
        },
        Tr::ActivityGap => match lang {
            Uk => "Деякі попередні події більше недоступні.",
            Ru => "Некоторые предыдущие события больше недоступны.",
            En => "Some earlier events are no longer available.",
        },
        Tr::ActivityStarted => match lang {
            Uk => "Розпочато",
            Ru => "Запущено",
            En => "Started",
        },
        Tr::ActivityToolStarted => match lang {
            Uk => "Запуск інструмента",
            Ru => "Запуск инструмента",
            En => "Tool started",
        },
        Tr::ActivityToolFinished => match lang {
            Uk => "Інструмент завершив роботу",
            Ru => "Инструмент завершил работу",
            En => "Tool finished",
        },
        Tr::ActivityToolFailed => match lang {
            Uk => "Помилка інструмента",
            Ru => "Ошибка инструмента",
            En => "Tool failed",
        },
        Tr::ActivityMessage => match lang {
            Uk => "Повідомлення",
            Ru => "Сообщение",
            En => "Message",
        },
        Tr::ActivityAborted => match lang {
            Uk => "Скасовано",
            Ru => "Отменено",
            En => "Aborted",
        },
        Tr::ActivityEnded => match lang {
            Uk => "Роботу припинено",
            Ru => "Работа прекращена",
            En => "Ended",
        },
        Tr::ActivityTimeline => match lang {
            Uk => "Хронологія",
            Ru => "Хронология",
            En => "Timeline",
        },
        Tr::ActivityTerminal => match lang {
            Uk => "Термінал",
            Ru => "Терминал",
            En => "Terminal",
        },
        Tr::DiffIndex => match lang {
            Uk => "Індекс",
            Ru => "Индекс",
            En => "Index",
        },
        Tr::DiffHead => match lang {
            Uk => "HEAD",
            Ru => "HEAD",
            En => "HEAD",
        },
        Tr::DiffBefore => match lang {
            Uk => "До змін",
            Ru => "До изменений",
            En => "Before",
        },
        Tr::DiffAfter => match lang {
            Uk => "Після змін",
            Ru => "После изменений",
            En => "After",
        },
        Tr::DiffOurs => match lang {
            Uk => "Наша версія",
            Ru => "Наша версия",
            En => "Ours",
        },
        Tr::DiffTheirs => match lang {
            Uk => "Їхня версія",
            Ru => "Их версия",
            En => "Theirs",
        },
        Tr::DiffSideBySide => match lang {
            Uk => "Поруч",
            Ru => "Рядом",
            En => "Side-by-side",
        },
        Tr::DiffWrapLines => match lang {
            Uk => "Переносити рядки",
            Ru => "Переносить строки",
            En => "Wrap lines",
        },
        Tr::DiffInline => match lang {
            Uk => "В одному стовпці",
            Ru => "В одном столбце",
            En => "Inline",
        },
        Tr::DiffPrevious => match lang {
            Uk => "Попередня зміна (Shift+F7)",
            Ru => "Предыдущее изменение (Shift+F7)",
            En => "Previous change (Shift+F7)",
        },
        Tr::DiffNext => match lang {
            Uk => "Наступна зміна (F7)",
            Ru => "Следующее изменение (F7)",
            En => "Next change (F7)",
        },
        Tr::DiffBinary => match lang {
            Uk => "Двійковий файл приховано",
            Ru => "Двоичный файл скрыт",
            En => "Binary file — not shown",
        },
        Tr::DiffEolMixed => match lang {
            Uk => "змішані",
            Ru => "смешанные",
            En => "mixed",
        },
        Tr::DiffEolNone => match lang {
            Uk => "відсутні",
            Ru => "отсутствуют",
            En => "none",
        },
        Tr::DiffTooLarge => match lang {
            Uk => "Файл завеликий для порівняння",
            Ru => "Файл слишком большой для сравнения",
            En => "File too large to diff",
        },
        Tr::GitCommits => match lang {
            Uk => "Коміти",
            Ru => "Коммиты",
            En => "Commits",
        },
        Tr::GitNoCommits => match lang {
            Uk => "Немає комітів",
            Ru => "Нет коммитов",
            En => "No commits",
        },
        Tr::GitLoadMore => match lang {
            Uk => "Завантажити ще",
            Ru => "Загрузить ещё",
            En => "Load more",
        },
        Tr::GitPullRebase => match lang {
            Uk => "Завантажити з rebase (pull --rebase)",
            Ru => "Загрузить с rebase (pull --rebase)",
            En => "Pull with rebase",
        },
        Tr::GitForcePush => match lang {
            Uk => "Примусовий push (with lease)",
            Ru => "Принудительный push (with lease)",
            En => "Force push (with lease)",
        },
        Tr::GitForcePushConfirm => match lang {
            Uk => "Історію віддаленої гілки буде перезаписано (з перевіркою --force-with-lease). Продовжити?",
            Ru => "История удалённой ветки будет перезаписана (с проверкой --force-with-lease). Продолжить?",
            En => "This overwrites the remote branch history (using --force-with-lease). Continue?",
        },
        Tr::GitStashAll => match lang {
            Uk => "Відкласти всі зміни (stash)",
            Ru => "Отложить все изменения (stash)",
            En => "Stash all changes",
        },
        Tr::GitStashChanges => match lang {
            Uk => "Відкласти зміни (stash)",
            Ru => "Отложить изменения (stash)",
            En => "Stash changes",
        },
        Tr::GitRefreshPr => match lang {
            Uk => "Оновити pull request",
            Ru => "Обновить pull request",
            En => "Refresh pull request",
        },
        Tr::GitRebaseInProgress => match lang {
            Uk => "Триває rebase",
            Ru => "Идёт rebase",
            En => "Rebase in progress",
        },
        Tr::GitMergeInProgress => match lang {
            Uk => "Триває злиття (merge)",
            Ru => "Идёт слияние (merge)",
            En => "Merge in progress",
        },
        Tr::GitCherryPickInProgress => match lang {
            Uk => "Триває cherry-pick",
            Ru => "Идёт cherry-pick",
            En => "Cherry-pick in progress",
        },
        Tr::GitRevertInProgress => match lang {
            Uk => "Триває revert",
            Ru => "Идёт revert",
            En => "Revert in progress",
        },
        Tr::GitResolveHint => match lang {
            Uk => "Розв'яжіть конфлікти, додайте файли до індексу й продовжте.",
            Ru => "Разрешите конфликты, добавьте файлы в индекс и продолжите.",
            En => "Resolve conflicts, stage the files, then continue.",
        },
        Tr::GitContinue => match lang {
            Uk => "Продовжити",
            Ru => "Продолжить",
            En => "Continue",
        },
        Tr::GitAbort => match lang {
            Uk => "Перервати",
            Ru => "Прервать",
            En => "Abort",
        },
        Tr::GitAbortConfirm => match lang {
            Uk => "Незавершену операцію буде скасовано, її прогрес втратиться. Перервати?",
            Ru => "Незавершённая операция будет отменена, её прогресс потеряется. Прервать?",
            En => "The unfinished operation will be cancelled and its progress lost. Abort?",
        },
        Tr::GitCommitPush => match lang {
            Uk => "Коміт і push",
            Ru => "Коммит и push",
            En => "Commit & Push",
        },
        Tr::GitAmend => match lang {
            Uk => "Виправити останній коміт (amend)",
            Ru => "Исправить последний коммит (amend)",
            En => "Amend last commit",
        },
        Tr::GitUndoCommit => match lang {
            Uk => "Скасувати останній коміт",
            Ru => "Отменить последний коммит",
            En => "Undo last commit",
        },
        Tr::GitAlreadyPushed => match lang {
            Uk => "Цей коміт уже надіслано. Після виправлення знадобиться force push.",
            Ru => "Этот коммит уже отправлен. После исправления потребуется force push.",
            En => "This commit is already pushed. Amending will require a force push.",
        },
        Tr::GitAlreadyPushedUndo => match lang {
            Uk => "Цей коміт уже надіслано. Після скасування знадобиться force push.",
            Ru => "Этот коммит уже отправлен. После отмены потребуется force push.",
            En => "This commit is already pushed. Undoing it will require a force push.",
        },
        Tr::GitCommitMore => match lang {
            Uk => "Інші дії з комітом",
            Ru => "Другие действия с коммитом",
            En => "More commit actions",
        },
        Tr::GitStashes => match lang {
            Uk => "Stash-і",
            Ru => "Stash",
            En => "Stashes",
        },
        Tr::GitStashApply => match lang {
            Uk => "Застосувати",
            Ru => "Применить",
            En => "Apply",
        },
        Tr::GitStashPop => match lang {
            Uk => "Застосувати й видалити (pop)",
            Ru => "Применить и удалить (pop)",
            En => "Pop",
        },
        Tr::GitStashDrop => match lang {
            Uk => "Видалити stash",
            Ru => "Удалить stash",
            En => "Drop",
        },
        Tr::GitStashDropConfirm => match lang {
            Uk => "Видалити цей stash? Це не можна скасувати.",
            Ru => "Удалить этот stash? Это нельзя отменить.",
            En => "Drop this stash? This cannot be undone.",
        },
        Tr::PrChecking => match lang {
            Uk => "Перевірка pull request…",
            Ru => "Проверка pull request…",
            En => "Checking pull request…",
        },
        Tr::PrInstallGh => match lang {
            Uk => "Встановіть GitHub CLI, щоб створювати й відстежувати pull request.",
            Ru => "Установите GitHub CLI, чтобы создавать и отслеживать pull request.",
            En => "Install GitHub CLI to create and track pull requests.",
        },
        Tr::PrNotSignedIn => match lang {
            Uk => "GitHub CLI не авторизовано. Виконайте `gh auth login` у терміналі.",
            Ru => "GitHub CLI не авторизован. Выполните `gh auth login` в терминале.",
            En => "GitHub CLI is not signed in. Run `gh auth login` in a terminal.",
        },
        Tr::PrCopyCommand => match lang {
            Uk => "Копіювати команду",
            Ru => "Копировать команду",
            En => "Copy command",
        },
        Tr::PrRetry => match lang {
            Uk => "Повторити",
            Ru => "Повторить",
            En => "Retry",
        },
        Tr::PrCreate => match lang {
            Uk => "Створити pull request",
            Ru => "Создать pull request",
            En => "Create pull request",
        },
        Tr::PrAheadOne => match lang {
            Uk => "1 коміт попереду {base}",
            Ru => "1 коммит впереди {base}",
            En => "1 commit ahead of {base}",
        },
        Tr::PrAhead => match lang {
            Uk => "Комітів попереду {base}: {n}",
            Ru => "Коммитов впереди {base}: {n}",
            En => "{n} commits ahead of {base}",
        },
        Tr::PrWillPush => match lang {
            Uk => "буде надіслано (push)",
            Ru => "будет отправлено (push)",
            En => "will push",
        },
        Tr::PrNoCommits => match lang {
            Uk => "Немає комітів для pull request",
            Ru => "Нет коммитов для pull request",
            En => "No commits to propose",
        },
        Tr::PrFinishOperation => match lang {
            Uk => "Спершу завершіть поточний rebase/merge",
            Ru => "Сначала завершите текущий rebase/merge",
            En => "Finish the current rebase/merge first",
        },
        Tr::PrStateOpen => match lang {
            Uk => "Відкритий",
            Ru => "Открыт",
            En => "Open",
        },
        Tr::PrStateDraft => match lang {
            Uk => "Чернетка",
            Ru => "Черновик",
            En => "Draft",
        },
        Tr::PrStateMerged => match lang {
            Uk => "Злитий",
            Ru => "Слит",
            En => "Merged",
        },
        Tr::PrStateClosed => match lang {
            Uk => "Закритий",
            Ru => "Закрыт",
            En => "Closed",
        },
        Tr::PrOpenInBrowser => match lang {
            Uk => "Відкрити в браузері",
            Ru => "Открыть в браузере",
            En => "Open in browser",
        },
        Tr::PrChecksFailing => match lang {
            Uk => "✕ {n} впало",
            Ru => "✕ {n} упало",
            En => "✕ {n} failing",
        },
        Tr::PrChecksRunning => match lang {
            Uk => "● {n} виконується",
            Ru => "● {n} выполняется",
            En => "● {n} running",
        },
        Tr::PrChecksPassed => match lang {
            Uk => "✓ {n} пройдено",
            Ru => "✓ {n} пройдено",
            En => "✓ {n} passed",
        },
        Tr::PrReviewApproved => match lang {
            Uk => "Схвалено",
            Ru => "Одобрено",
            En => "Approved",
        },
        Tr::PrReviewChanges => match lang {
            Uk => "Потрібні зміни",
            Ru => "Требуются изменения",
            En => "Changes requested",
        },
        Tr::PrReviewRequired => match lang {
            Uk => "Потрібне рев'ю",
            Ru => "Требуется ревью",
            En => "Review required",
        },
        Tr::PrMergeConflicts => match lang {
            Uk => "Конфлікти",
            Ru => "Конфликты",
            En => "Conflicts",
        },
        Tr::PrMergeBehind => match lang {
            Uk => "Відстає від {base}",
            Ru => "Отстаёт от {base}",
            En => "Behind {base}",
        },
        Tr::PrMergeBlocked => match lang {
            Uk => "Заблоковано",
            Ru => "Заблокировано",
            En => "Blocked",
        },
        Tr::PrMergeReadyChecksFailing => match lang {
            Uk => "Можна злити з попередженнями",
            Ru => "Можно слить с предупреждениями",
            En => "Mergeable with warnings",
        },
        Tr::PrMergeReady => match lang {
            Uk => "Готово до злиття",
            Ru => "Готово к слиянию",
            En => "Ready to merge",
        },
        Tr::PrRerunFailed => match lang {
            Uk => "Перезапустити невдалі",
            Ru => "Перезапустить упавшие",
            En => "Rerun failed",
        },
        Tr::PrSendToAgent => match lang {
            Uk => "Надіслати агенту",
            Ru => "Отправить агенту",
            En => "Send to agent",
        },
        Tr::PrSendFailuresToAgent => match lang {
            Uk => "Надіслати помилки агенту",
            Ru => "Отправить ошибки агенту",
            En => "Send failures to agent",
        },
        Tr::PrNoAgentTerminal => match lang {
            Uk => "У цьому worktree немає термінала з агентом. Спершу запустіть агента.",
            Ru => "В этом worktree нет терминала с агентом. Сначала запустите агента.",
            En => "No agent terminal is open in this worktree. Start an agent first.",
        },
        Tr::PrNotPushed => match lang {
            Uk => "Гілку не надіслано",
            Ru => "Ветка не отправлена",
            En => "Branch not pushed",
        },
        Tr::PrUnpushedOne => match lang {
            Uk => "1 ненадісланий коміт",
            Ru => "1 неотправленный коммит",
            En => "1 unpushed commit",
        },
        Tr::PrUnpushed => match lang {
            Uk => "Ненадіслано комітів: {n}",
            Ru => "Неотправлено коммитов: {n}",
            En => "{n} unpushed commits",
        },
        Tr::PrPushButton => match lang {
            Uk => "Надіслати",
            Ru => "Отправить",
            En => "Push",
        },
        Tr::PrMergeSquash => match lang {
            Uk => "Squash і злиття",
            Ru => "Squash и слияние",
            En => "Squash and merge",
        },
        Tr::PrMergeMerge => match lang {
            Uk => "Злиття (merge)",
            Ru => "Слияние (merge)",
            En => "Merge",
        },
        Tr::PrMergeRebase => match lang {
            Uk => "Rebase і злиття",
            Ru => "Rebase и слияние",
            En => "Rebase and merge",
        },
        Tr::PrMergeConfirmTitle => match lang {
            Uk => "Злити pull request",
            Ru => "Слить pull request",
            En => "Merge pull request",
        },
        Tr::PrMergeConfirm => match lang {
            Uk => "Злити #{n} у {base}? ({method})",
            Ru => "Слить #{n} в {base}? ({method})",
            En => "Merge #{n} into {base}? ({method})",
        },
        Tr::PrReasonDraft => match lang {
            Uk => "Чернетки не можна зливати",
            Ru => "Черновики нельзя сливать",
            En => "Draft pull requests can't be merged",
        },
        Tr::PrReasonConflicts => match lang {
            Uk => "Спершу розв'яжіть конфлікти",
            Ru => "Сначала разрешите конфликты",
            En => "Resolve conflicts first",
        },
        Tr::PrReasonBlocked => match lang {
            Uk => "Злиття заблоковано правилами гілки",
            Ru => "Слияние заблокировано правилами ветки",
            En => "Merging is blocked by branch protection",
        },
        Tr::PrReasonPush => match lang {
            Uk => "Спершу надішліть свої коміти",
            Ru => "Сначала отправьте свои коммиты",
            En => "Push your commits first",
        },
        Tr::PrCopyLink => match lang {
            Uk => "Копіювати посилання",
            Ru => "Копировать ссылку",
            En => "Copy link",
        },
        Tr::PrMarkReady => match lang {
            Uk => "Позначити готовим до рев'ю",
            Ru => "Отметить готовым к ревью",
            En => "Mark ready for review",
        },
        Tr::PrConvertDraft => match lang {
            Uk => "Перетворити на чернетку",
            Ru => "Преобразовать в черновик",
            En => "Convert to draft",
        },
        Tr::PrClose => match lang {
            Uk => "Закрити pull request",
            Ru => "Закрыть pull request",
            En => "Close pull request",
        },
        Tr::PrCloseConfirm => match lang {
            Uk => "Закрити #{n} без злиття?",
            Ru => "Закрыть #{n} без слияния?",
            En => "Close #{n} without merging?",
        },
        Tr::PrReopen => match lang {
            Uk => "Відкрити знову",
            Ru => "Открыть снова",
            En => "Reopen",
        },
        Tr::PrTitleLabel => match lang {
            Uk => "Заголовок",
            Ru => "Заголовок",
            En => "Title",
        },
        Tr::PrDescriptionLabel => match lang {
            Uk => "Опис",
            Ru => "Описание",
            En => "Description",
        },
        Tr::PrBaseLabel => match lang {
            Uk => "Цільова гілка",
            Ru => "Целевая ветка",
            En => "Base branch",
        },
        Tr::PrDraftCheckbox => match lang {
            Uk => "Створити як чернетку",
            Ru => "Создать как черновик",
            En => "Create as draft",
        },
        Tr::PrWillBePushed => match lang {
            Uk => "Гілку {branch} буде надіслано до {remote}",
            Ru => "Ветка {branch} будет отправлена в {remote}",
            En => "Branch {branch} will be pushed to {remote}",
        },
        Tr::PrUncommitted => match lang {
            Uk => "Незакомічені зміни не буде включено: {n}",
            Ru => "Незакоммиченные изменения не будут включены: {n}",
            En => "Uncommitted changes won't be included: {n}",
        },
        Tr::PrTabLabel => match lang {
            Uk => "Pull request",
            Ru => "Pull request",
            En => "Pull request",
        },
        Tr::PrTipChecksPassing => match lang {
            Uk => "Перевірки пройдено",
            Ru => "Проверки пройдены",
            En => "Checks passing",
        },
        Tr::PrTipChecksRunning => match lang {
            Uk => "Перевірки виконуються",
            Ru => "Проверки выполняются",
            En => "Checks running",
        },
        Tr::PrTipCheckFailingOne => match lang {
            Uk => "1 перевірка не пройдена",
            Ru => "1 проверка не пройдена",
            En => "1 check failing",
        },
        Tr::PrTipChecksFailing => match lang {
            Uk => "Перевірок не пройдено: {n}",
            Ru => "Проверок не пройдено: {n}",
            En => "{n} checks failing",
        },
        Tr::GithubPrs => match lang {
            Uk => "Pull request через GitHub CLI",
            Ru => "Pull request через GitHub CLI",
            En => "Pull requests via GitHub CLI",
        },
        Tr::GithubPrsDesc => match lang {
            Uk => "Використовує встановлений gh, щоб показувати й створювати pull request.",
            Ru => "Использует установленный gh для показа и создания pull request.",
            En => "Uses your installed gh to show and create pull requests.",
        },
        Tr::WorktreeLocation => match lang {
            Uk => "Розташування worktree",
            Ru => "Расположение worktree",
            En => "Worktree location",
        },
        Tr::WorktreeLocSibling => match lang {
            Uk => "Поруч із проєктом",
            Ru => "Рядом с проектом",
            En => "Next to the project",
        },
        Tr::WorktreeLocInside => match lang {
            Uk => "Усередині проєкту (.worktrees)",
            Ru => "Внутри проекта (.worktrees)",
            En => "Inside the project (.worktrees)",
        },
        Tr::WorktreeLocShared => match lang {
            Uk => "Спільна тека (~/sich/worktrees)",
            Ru => "Общая папка (~/sich/worktrees)",
            En => "Shared folder (~/sich/worktrees)",
        },
        Tr::WorktreeLocSiblingDesc => match lang {
            Uk => "Застосовується до нових worktree. Приклад: E:\\dev\\game-feature-ui",
            Ru => "Применяется к новым worktree. Пример: E:\\dev\\game-feature-ui",
            En => "Applies to new worktrees. Example: E:\\dev\\game-feature-ui",
        },
        Tr::WorktreeLocInsideDesc => match lang {
            Uk => "Застосовується до нових worktree. Приклад: E:\\dev\\game\\.worktrees\\feature\\ui (додається в .git/info/exclude)",
            Ru => "Применяется к новым worktree. Пример: E:\\dev\\game\\.worktrees\\feature\\ui (добавляется в .git/info/exclude)",
            En => "Applies to new worktrees. Example: E:\\dev\\game\\.worktrees\\feature\\ui (added to .git/info/exclude)",
        },
        Tr::WorktreeLocSharedDesc => match lang {
            Uk => "Застосовується до нових worktree. Приклад: ~/sich/worktrees/game/feature/ui",
            Ru => "Применяется к новым worktree. Пример: ~/sich/worktrees/game/feature/ui",
            En => "Applies to new worktrees. Example: ~/sich/worktrees/game/feature/ui",
        },
        Tr::NewTerminal => match lang {
            Uk => "Новий термінал",
            Ru => "Новый терминал",
            En => "New terminal",
        },
        Tr::Reconnecting => match lang {
            Uk => "Перепідключення\u{2026}",
            Ru => "Переподключение\u{2026}",
            En => "Reconnecting\u{2026}",
        },
        Tr::Settings => match lang {
            Uk => "Налаштування",
            Ru => "Настройки",
            En => "Settings",
        },
        Tr::Language => match lang {
            Uk => "Мова",
            Ru => "Язык",
            En => "Language",
        },
        Tr::Theme => match lang {
            Uk => "Тема оформлення",
            Ru => "Тема оформления",
            En => "Theme",
        },
        Tr::ThemeGray => match lang {
            Uk => "Сіра",
            Ru => "Серая",
            En => "Gray",
        },
        Tr::ThemeAmoled => match lang {
            Uk => "Чорна (AMOLED)",
            Ru => "Чёрная (AMOLED)",
            En => "Black (AMOLED)",
        },
        Tr::CloseTerminal => match lang {
            Uk => "Закрити термінал",
            Ru => "Закрыть терминал",
            En => "Close terminal",
        },
        Tr::SplitRight => match lang {
            Uk => "Розділити праворуч",
            Ru => "Разделить вправо",
            En => "Split Right",
        },
        Tr::SplitDown => match lang {
            Uk => "Розділити вниз",
            Ru => "Разделить вниз",
            En => "Split Down",
        },
        Tr::DetachPane => match lang {
            Uk => "Від'єднати панель",
            Ru => "Отсоединить панель",
            En => "Detach Pane",
        },
        Tr::DropPaneHere => match lang {
            Uk => "Відпустіть тут, щоб від’єднати панель",
            Ru => "Отпустите здесь, чтобы отсоединить панель",
            En => "Drop here to detach pane",
        },
        Tr::TerminalSplitFailed => match lang {
            Uk => "Не вдалося відкрити термінал у новій панелі.",
            Ru => "Не удалось открыть терминал в новой панели.",
            En => "Could not open a terminal in the new pane.",
        },
        Tr::TerminalSplitDisconnected => match lang {
            Uk => "Не вдалося розділити термінал: немає з’єднання з демоном.",
            Ru => "Не удалось разделить терминал: нет соединения с демоном.",
            En => "Could not split the terminal: the daemon is disconnected.",
        },
        Tr::QuickCommands => match lang {
            Uk => "Швидкі команди",
            Ru => "Быстрые команды",
            En => "Quick commands",
        },
        Tr::QuickCommandsEmpty => match lang {
            Uk => "Команд ще немає. Додайте першу команду.",
            Ru => "Команд пока нет. Добавьте первую команду.",
            En => "No commands yet. Add the first command.",
        },
        Tr::QuickCommandName => match lang {
            Uk => "Назва (необов’язково)",
            Ru => "Название (необязательно)",
            En => "Name (optional)",
        },
        Tr::QuickCommandCommand => match lang {
            Uk => "Команда",
            Ru => "Команда",
            En => "Command",
        },
        Tr::QuickCommandAdd => match lang {
            Uk => "Додати команду",
            Ru => "Добавить команду",
            En => "Add command",
        },
        Tr::QuickCommandEdit => match lang {
            Uk => "Редагувати швидку команду",
            Ru => "Изменить быструю команду",
            En => "Edit quick command",
        },
        Tr::QuickCommandManage => match lang {
            Uk => "Керувати командами…",
            Ru => "Управлять командами…",
            En => "Manage commands…",
        },
        Tr::QuickCommandSave => match lang {
            Uk => "Зберегти",
            Ru => "Сохранить",
            En => "Save",
        },
        Tr::QuickCommandOpenFailed => match lang {
            Uk => "Не вдалося відкрити вкладку для швидкої команди.",
            Ru => "Не удалось открыть вкладку для быстрой команды.",
            En => "Could not open a tab for the quick command.",
        },
        Tr::QuickCommandRunning => match lang {
            Uk => "Закриється після команди",
            Ru => "Закроется после команды",
            En => "Closes after the command",
        },
        Tr::QuickCommandFinished => match lang {
            Uk => "Закриття через",
            Ru => "Закрытие через",
            En => "Closes in",
        },
        Tr::NoTerminals => match lang {
            Uk => "Немає терміналів",
            Ru => "Нет терминалов",
            En => "No terminals",
        },
        Tr::Exited => match lang {
            Uk => "завершено",
            Ru => "завершён",
            En => "exited",
        },
        Tr::ProcessExited => match lang {
            Uk => "Процес завершився",
            Ru => "Процесс завершился",
            En => "Process exited",
        },
        Tr::CloseFromSidebar => match lang {
            Uk => "Закрийте термінал на вкладці.",
            Ru => "Закройте терминал на вкладке.",
            En => "Close the terminal from the tab bar.",
        },
        Tr::Projects => match lang {
            Uk => "Проєкти",
            Ru => "Проекты",
            En => "Projects",
        },
        Tr::Home => match lang {
            Uk => "Домівка",
            Ru => "Домашняя",
            En => "Home",
        },
        Tr::AddProject => match lang {
            Uk => "Додати проєкт",
            Ru => "Добавить проект",
            En => "Add project",
        },
        Tr::Refresh => match lang {
            Uk => "Оновити",
            Ru => "Обновить",
            En => "Refresh",
        },
        Tr::NewWorktree => match lang {
            Uk => "Новий worktree",
            Ru => "Новый worktree",
            En => "New worktree",
        },
        Tr::MoreActions => match lang {
            Uk => "Ще дії",
            Ru => "Ещё действия",
            En => "More actions",
        },
        Tr::OpenInExplorer => match lang {
            Uk => "Відкрити в Провіднику",
            Ru => "Открыть в Проводнике",
            En => "Open in Explorer",
        },
        Tr::CopyPath => match lang {
            Uk => "Копіювати шлях",
            Ru => "Копировать путь",
            En => "Copy path",
        },
        Tr::RemoveProject => match lang {
            Uk => "Видалити проєкт",
            Ru => "Удалить проект",
            En => "Remove project",
        },
        Tr::RemoveWorktree => match lang {
            Uk => "Видалити worktree",
            Ru => "Удалить worktree",
            En => "Remove worktree",
        },
        Tr::MainTag => match lang {
            Uk => "основний",
            Ru => "основной",
            En => "main",
        },
        Tr::UncommittedChanges => match lang {
            Uk => "Незафіксовані зміни",
            Ru => "Незафиксированные изменения",
            En => "Uncommitted changes",
        },
        Tr::NewTerminalHere => match lang {
            Uk => "Новий термінал тут",
            Ru => "Новый терминал здесь",
            En => "New terminal here",
        },
        Tr::NoProjectsHint => match lang {
            Uk => "Проєктів ще немає. Перетягніть сюди папку з кодом.",
            Ru => "Проектов пока нет. Перетащите сюда папку с кодом.",
            En => "No projects yet. Drop a folder here to add it.",
        },
        Tr::NameLabel => match lang {
            Uk => "Назва",
            Ru => "Название",
            En => "Name",
        },
        Tr::BaseLabel => match lang {
            Uk => "Основа",
            Ru => "Основа",
            En => "Base",
        },
        Tr::Cancel => match lang {
            Uk => "Скасувати",
            Ru => "Отмена",
            En => "Cancel",
        },
        Tr::Create => match lang {
            Uk => "Створити",
            Ru => "Создать",
            En => "Create",
        },
        Tr::Remove => match lang {
            Uk => "Видалити",
            Ru => "Удалить",
            En => "Remove",
        },
        Tr::RemoveAnyway => match lang {
            Uk => "Все одно видалити",
            Ru => "Всё равно удалить",
            En => "Remove anyway",
        },
        Tr::DirtyWarning => match lang {
            Uk => "У цьому worktree є незафіксовані зміни — їх буде втрачено.",
            Ru => "В этом worktree есть незафиксированные изменения — они будут потеряны.",
            En => "This worktree has uncommitted changes — they will be lost.",
        },
        Tr::RestartDaemon => match lang {
            Uk => "Перезапустити демон",
            Ru => "Перезапустить демон",
            En => "Restart daemon",
        },
        Tr::RestartAllLegacyTerminals => match lang {
            Uk => "Натисніть, щоб перезапустити всі термінали зі старих демонів на поточному. Вивід збережеться, запущені процеси завершаться.",
            Ru => "Нажмите, чтобы перезапустить все терминалы со старых демонов на текущем. Вывод сохранится, запущенные процессы завершатся.",
            En => "Click to restart all terminals from older daemons on the current daemon. Output is kept; running processes stop.",
        },
        Tr::BatchTerminalRestartFailed => match lang {
            Uk => "Не вдалося перезапустити всі термінали зі старих демонів.",
            Ru => "Не удалось перезапустить все терминалы со старых демонов.",
            En => "Could not restart all terminals from older daemons.",
        },
        Tr::DaemonUpdateRequired => match lang {
            Uk => "Sich оновлено. Потрібно перезапустити демон терміналів. Термінали буде перезапущено, а сесії агентів — відновлено.",
            Ru => "Sich обновлён. Нужно перезапустить демон терминалов. Терминалы будут перезапущены, а сессии агентов восстановлены.",
            En => "Sich was updated and the terminal daemon must restart. Running terminals will be restarted; agents will be resumed.",
        },
        Tr::DaemonRestarting => match lang {
            Uk => "Демон перезапускається…",
            Ru => "Демон перезапускается…",
            En => "Restarting daemon…",
        },
        Tr::RestartLegacyTerminal => match lang {
            Uk => "Термінал працює на старому демоні. Натисніть, щоб перезапустити, коли буде зручно. Вивід збережеться, запущені процеси завершаться.",
            Ru => "Терминал работает на старом демоне. Нажмите, чтобы перезапустить, когда будет удобно. Вывод сохранится, запущенные процессы завершатся.",
            En => "This terminal is on the older daemon. Click to restart when convenient. Output is kept; running processes stop.",
        },
        Tr::TerminalRestarting => match lang {
            Uk => "Термінал перезапускається…",
            Ru => "Терминал перезапускается…",
            En => "Restarting terminal…",
        },
        Tr::TerminalRestartFailed => match lang {
            Uk => "Не вдалося завершити перезапуск термінала.",
            Ru => "Не удалось завершить перезапуск терминала.",
            En => "Could not complete the terminal restart.",
        },
        Tr::RestoredScrollback => match lang {
            Uk => "─── відновлено після перезапуску ───",
            Ru => "─── восстановлено после перезапуска ───",
            En => "─── restored after restart ───",
        },
        Tr::AgentInterrupted => match lang {
            Uk => "Роботу агента перервано перезапуском",
            Ru => "Работа агента прервана перезапуском",
            En => "Agent was interrupted by the restart",
        },
        Tr::ResumeAgentsOnRestore => match lang {
            Uk => "Відновлювати сесії агентів після перезапуску терміналів",
            Ru => "Возобновлять сессии агентов после перезапуска терминалов",
            En => "Resume agents when restoring terminals",
        },
        Tr::AgentWorking => match lang {
            Uk => "Працює",
            Ru => "Работает",
            En => "Working",
        },
        Tr::AgentWaiting => match lang {
            Uk => "Чекає на вас",
            Ru => "Ждёт вас",
            En => "Waiting for you",
        },
        Tr::AgentDone => match lang {
            Uk => "Завершив",
            Ru => "Завершил",
            En => "Finished",
        },
        Tr::AgentFailed => match lang {
            Uk => "Помилка",
            Ru => "Ошибка",
            En => "Failed",
        },
        Tr::AgentsSection => match lang {
            Uk => "Агенти",
            Ru => "Агенты",
            En => "Agents",
        },
        Tr::TrackClaudeHooks => match lang {
            Uk => "Відстежувати стан Claude Code (додає хуки в ~/.claude/settings.json)",
            Ru => "Отслеживать состояние Claude Code (добавляет хуки в ~/.claude/settings.json)",
            En => "Track Claude Code status (installs hooks into ~/.claude/settings.json)",
        },
        Tr::HooksNotInstalled => match lang {
            Uk => "Не встановлено",
            Ru => "Не установлено",
            En => "Not installed",
        },
        Tr::HooksWorking => match lang {
            Uk => "Оновлення\u{2026}",
            Ru => "Обновление\u{2026}",
            En => "Updating\u{2026}",
        },
        Tr::TrackCodexHooks => match lang {
            Uk => "Відстежувати стан Codex (додає хуки в ~/.codex/hooks.json)",
            Ru => "Отслеживать состояние Codex (добавляет хуки в ~/.codex/hooks.json)",
            En => "Track Codex status (installs hooks into ~/.codex/hooks.json)",
        },
        Tr::TrackOmpHooks => match lang {
            Uk => "Відстежувати стан omp (встановлює розширення)",
            Ru => "Отслеживать состояние omp (устанавливает расширение)",
            En => "Track omp status (installs an extension)",
        },
        Tr::OmpInstalled => match lang {
            Uk => "Розширення встановлено. Зміни діють після повторного запуску omp.",
            Ru => "Расширение установлено. Изменения действуют после повторного запуска omp.",
            En => "Extension installed. Changes take effect the next time you start omp.",
        },
        Tr::OmpNotFound => match lang {
            Uk => "omp не знайдено",
            Ru => "omp не найден",
            En => "omp not found",
        },
        Tr::OmpTooOld => match lang {
            Uk => "Потрібен omp 19.0.1 або новіший",
            Ru => "Требуется omp 19.0.1 или новее",
            En => "Requires omp 19.0.1 or later",
        },
        Tr::UsageLimits => match lang {
            Language::Uk => "Показувати ліміти Codex і Claude Code у рядку стану",
            Language::Ru => "Показывать лимиты Codex и Claude Code в строке состояния",
            Language::En => "Show Codex and Claude Code limits in the status bar",
        },
        Tr::UsageTitle => match lang {
            Uk => "Ліміти використання",
            Ru => "Лимиты использования",
            En => "Usage limits",
        },
        Tr::UsageFiveHourDetail => match lang {
            Uk => "5-годинний ліміт",
            Ru => "5-часовой лимит",
            En => "5-hour limit",
        },
        Tr::UsageWeekDetail => match lang {
            Uk => "7-денний ліміт",
            Ru => "7-дневный лимит",
            En => "7-day limit",
        },
        Tr::UsageNoReading => match lang {
            Uk => "Немає даних",
            Ru => "Нет данных",
            En => "No usage data",
        },
        Tr::ClaudeStatuslineWrap => match lang {
            Language::Uk => "Обгортати мій statusLine Claude Code, щоб бачити ліміти (повертається при вимкненні)",
            Language::Ru => "Оборачивать мой statusLine Claude Code, чтобы видеть лимиты (возвращается при выключении)",
            Language::En => "Wrap my Claude Code statusLine to read limits (restored when turned off)",
        },
        Tr::UsageFiveHour => match lang {
            Language::Uk => "5г",
            Language::Ru => "5ч",
            Language::En => "5h",
        },
        Tr::UsageWeek => match lang {
            Language::Uk | Language::Ru => "7д",
            Language::En => "7d",
        },
        Tr::UsageStale => match lang {
            Language::Uk => " (застаріло)",
            Language::Ru => " (устарело)",
            Language::En => " (stale)",
        },
        Tr::UsageResetPassed => match lang {
            Language::Uk => "Очікування нового показника після скидання",
            Language::Ru => "Ожидание новых данных после сброса",
            Language::En => "Waiting for a new reading after reset",
        },
        Tr::UsageClaudeNeedsWrap => match lang {
            Language::Uk => "Увімкніть обгортання statusLine Claude Code у налаштуваннях Sich, щоб бачити ліміти.",
            Language::Ru => "Включите обёртку statusLine Claude Code в настройках Sich, чтобы видеть лимиты.",
            Language::En => "Enable Claude Code statusLine wrapping in Sich settings to see limits.",
        },
        Tr::UsageClaudeEdited => match lang {
            Language::Uk => "statusLine Claude Code змінено. Щоб відновити читання лімітів, вимкніть і знову ввімкніть показ лімітів.",
            Language::Ru => "statusLine Claude Code изменена. Чтобы возобновить чтение лимитов, выключите показ лимитов и включите снова.",
            Language::En => "Claude Code statusLine changed. Turn usage limits off and on to reconnect.",
        },
        Tr::UsageClaudeOld => match lang {
            Language::Uk => "Ліміти потребують Claude Code 2.1.80 або новішої версії.",
            Language::Ru => "Для лимитов нужна Claude Code версии 2.1.80 или новее.",
            Language::En => "Limits require Claude Code 2.1.80 or later.",
        },
        Tr::UsageClaudeFailed => match lang {
            Language::Uk => "Не вдалося налаштувати statusLine Claude Code.",
            Language::Ru => "Не удалось настроить statusLine Claude Code.",
            Language::En => "Couldn't configure the Claude Code statusLine.",
        },
        Tr::UsageCodexOld => match lang {
            Language::Uk => "Ліміти потребують Codex 0.48 або новішої версії.",
            Language::Ru => "Для лимитов нужен Codex версии 0.48 или новее.",
            Language::En => "Limits require Codex 0.48 or later.",
        },
        Tr::UsageCodexUnsupported => match lang {
            Language::Uk => "Встановлений Codex не підтримує запит лімітів.",
            Language::Ru => "Установленный Codex не поддерживает запрос лимитов.",
            Language::En => "The installed Codex doesn't support reading limits.",
        },
        Tr::UsageCodexNoLimits => match lang {
            Language::Uk => "Codex не надав дані про ліміти.",
            Language::Ru => "Codex не предоставил данные о лимитах.",
            Language::En => "Codex returned no usage limits.",
        },
        Tr::UsageCodexFailed => match lang {
            Language::Uk => "Не вдалося прочитати ліміти. Перевірте, чи ви ввійшли в Codex.",
            Language::Ru => "Не удалось прочитать лимиты. Проверьте, выполнен ли вход в Codex.",
            Language::En => "Couldn't read limits. Check your Codex sign-in.",
        },
        Tr::OrchestrationSkill => match lang {
            Uk => "Навчити Claude Code керувати воркерами (додає скіл у ~/.claude/skills)",
            Ru => "Научить Claude Code управлять воркерами (добавляет скилл в ~/.claude/skills)",
            En => "Teach Claude Code to orchestrate workers (installs a skill into ~/.claude/skills)",
        },
        Tr::OrchUnattendedWorkers => match lang {
            Uk => "Воркери оркестрації працюють без підтверджень (повна автономія, як в Orca)",
            Ru => "Воркеры оркестрации работают без подтверждений (полная автономия, как в Orca)",
            En => "Orchestration workers run without approval prompts (full autonomy, like Orca)",
        },
        Tr::OrchCoordinator => match lang {
            Uk => "коорд",
            Ru => "коорд",
            En => "coord",
        },
        Tr::OrchWorker => match lang {
            Uk => "воркер",
            Ru => "воркер",
            En => "worker",
        },
        Tr::OrchAutoCloseWorkers => match lang {
            Uk => "Автоматично закривати воркери, вкладки активності й термінали швидких команд (через 2 хв після завершення, якщо не натиснути «Залишити відкритим»)",
            Ru => "Автоматически закрывать воркеры, вкладки активности и терминалы быстрых команд (через 2 мин после завершения, если не нажать «Оставить открытым»)",
            En => "Auto-close workers, activity tabs and quick-command terminals (2 minutes after completion unless you choose Keep open)",
        },
        Tr::OrchCleanupWorktrees => match lang {
            Uk => "Видаляти worktree воркерів після злиття їхньої гілки",
            Ru => "Удалять worktree воркеров после слияния их ветки",
            En => "Remove worker worktrees after their branch is merged",
        },
        Tr::OrchTemporary => match lang {
            Uk => "Тимчасовий",
            Ru => "Временный",
            En => "Temporary",
        },
        Tr::OrchAutoCloseRunning => match lang {
            Uk => "Воркер оркестрації · закриється через 2 хв після завершення",
            Ru => "Воркер оркестрации · закроется через 2 мин после завершения",
            En => "Orchestration worker · Closes 2 minutes after it finishes",
        },
        Tr::OrchAutoCloseFinished => match lang {
            Uk => "Воркер завершив роботу · закриється через",
            Ru => "Воркер завершил работу · закроется через",
            En => "Orchestration worker finished · Closes in",
        },
        Tr::ActivityAutoCloseRunning => match lang {
            Uk => "Вкладка активності · закриється через 2 хв після завершення",
            Ru => "Вкладка активности · закроется через 2 мин после завершения",
            En => "Activity tab · Closes 2 minutes after the subagent finishes",
        },
        Tr::ActivityAutoCloseFinished => match lang {
            Uk => "Субагент завершив роботу · вкладка закриється через",
            Ru => "Субагент завершил работу · вкладка закроется через",
            En => "Subagent finished · Tab closes in",
        },
        Tr::OrchKeepOpen => match lang {
            Uk => "Залишити відкритим",
            Ru => "Оставить открытым",
            En => "Keep open",
        },
        Tr::OrchAllowAutoClose => match lang {
            Uk => "Дозволити автозакриття",
            Ru => "Разрешить автозакрытие",
            En => "Allow auto-close",
        },
        Tr::CodexNotFound => match lang {
            Uk => "Codex не знайдено",
            Ru => "Codex не найден",
            En => "Codex not found",
        },
        Tr::CodexTooOld => match lang {
            Uk => "Codex застарий (потрібна версія 0.129 або новіша)",
            Ru => "Codex устарел (нужна версия 0.129 или новее)",
            En => "Codex is too old (0.129 or newer required)",
        },
        Tr::CodexTrustPending => match lang {
            Uk => "Хуки встановлено. Codex ще попросить їх схвалити (/hooks)",
            Ru => "Хуки установлены. Codex ещё попросит их одобрить (/hooks)",
            En => "Hooks installed. Codex may still ask for approval (/hooks)",
        },
        Tr::CodexTrustCleanupPending => match lang {
            Uk => "Хуки видалено. Не вдалося прибрати їх схвалення з Codex",
            Ru => "Хуки удалены. Не удалось убрать их одобрение из Codex",
            En => "Hooks removed. Their Codex approvals could not be cleared",
        },
        Tr::NewGroup => match lang {
            Uk => "Нова група",
            Ru => "Новая группа",
            En => "New group",
        },
        Tr::GroupName => match lang {
            Uk => "Назва групи",
            Ru => "Название группы",
            En => "Group name",
        },
        Tr::RenameGroup => match lang {
            Uk => "Перейменувати групу",
            Ru => "Переименовать группу",
            En => "Rename group",
        },
        Tr::Rename => match lang {
            Uk => "Перейменувати",
            Ru => "Переименовать",
            En => "Rename",
        },
        Tr::DeleteGroup => match lang {
            Uk => "Видалити групу",
            Ru => "Удалить группу",
            En => "Delete group",
        },
        Tr::DeleteGroupConfirm => match lang {
            Uk => "Проєкти залишаться, вони лише вийдуть із групи.",
            Ru => "Проекты останутся, они лишь выйдут из группы.",
            En => "Projects stay, they just leave the group.",
        },
        Tr::AddProjectToGroup => match lang {
            Uk => "Додати проєкт до групи",
            Ru => "Добавить проект в группу",
            En => "Add project to group",
        },
        Tr::MoveToGroup => match lang {
            Uk => "Перемістити до групи",
            Ru => "Переместить в группу",
            En => "Move to group",
        },
        Tr::RemoveFromGroup => match lang {
            Uk => "Прибрати з групи",
            Ru => "Убрать из группы",
            En => "Remove from group",
        },
        Tr::Notifications => match lang {
            Uk => "Сповіщення",
            Ru => "Уведомления",
            En => "Notifications",
        },
        Tr::NotificationsEnabled => match lang {
            Uk => "Показувати сповіщення Windows",
            Ru => "Показывать уведомления Windows",
            En => "Show Windows notifications",
        },
        Tr::NotifyAgentNeedsInput => match lang {
            Uk => "Агент потребує уваги",
            Ru => "Агент требует внимания",
            En => "Agent needs input",
        },
        Tr::NotifyAgentDone => match lang {
            Uk => "Агент завершив роботу або сталася помилка",
            Ru => "Агент завершил работу или произошла ошибка",
            En => "Agent finished or failed",
        },
        Tr::NotifySound => match lang {
            Uk => "Звук сповіщень",
            Ru => "Звук уведомлений",
            En => "Notification sound",
        },
        Tr::TestNotification => match lang {
            Uk => "Тестове сповіщення",
            Ru => "Тестовое уведомление",
            En => "Test notification",
        },
        Tr::TestNotificationBody => match lang {
            Uk => "Натисніть, щоб відкрити Sich.",
            Ru => "Нажмите, чтобы открыть Sich.",
            En => "Click to open Sich.",
        },
        Tr::ToastNeedsInput => match lang {
            Uk => "потребує уваги",
            Ru => "требует внимания",
            En => "needs input",
        },
        Tr::ToastDone => match lang {
            Uk => "завершив",
            Ru => "завершил",
            En => "finished",
        },
        Tr::ToastFailed => match lang {
            Uk => "помилка",
            Ru => "ошибка",
            En => "failed",
        },
        Tr::ToastAgent => match lang {
            Uk => "Агент",
            Ru => "Агент",
            En => "Agent",
        },
        Tr::CloseToTray => match lang {
            Uk => "Згортати в трей при закритті",
            Ru => "Сворачивать в трей при закрытии",
            En => "Minimize to tray on close",
        },
        Tr::Update => match lang {
            Uk => "Оновити",
            Ru => "Обновить",
            En => "Update",
        },
        Tr::UpdateTitle => match lang {
            Uk => "Оновлення Sich",
            Ru => "Обновление Sich",
            En => "Sich update",
        },
        Tr::Later => match lang {
            Uk => "Пізніше",
            Ru => "Позже",
            En => "Later",
        },
        Tr::Downloading => match lang {
            Uk => "Завантаження…",
            Ru => "Загрузка…",
            En => "Downloading…",
        },
        Tr::Verifying => match lang {
            Uk => "Перевірка…",
            Ru => "Проверка…",
            En => "Verifying…",
        },
        Tr::Installing => match lang {
            Uk => "Встановлення…",
            Ru => "Установка…",
            En => "Installing…",
        },
        Tr::CheckUpdates => match lang {
            Uk => "Перевіряти оновлення",
            Ru => "Проверять обновления",
            En => "Check for updates",
        },
        Tr::CheckNow => match lang {
            Uk => "Перевірити зараз",
            Ru => "Проверить сейчас",
            En => "Check now",
        },
        Tr::CheckingUpdates => match lang {
            Uk => "Перевірка…",
            Ru => "Проверка…",
            En => "Checking…",
        },
        Tr::UpdatesDevBuild => match lang {
            Uk => "Dev-збірка: оновлення вимкнені",
            Ru => "Dev-сборка: обновления отключены",
            En => "Dev build: updates disabled",
        },
        Tr::NoWriteAccess => match lang {
            Uk => "Немає прав на запис у теку програми. Перенесіть Sich у теку користувача.",
            Ru => "Нет прав на запись в папку программы. Перенесите Sich в папку пользователя.",
            En => "No write access to the program folder. Move Sich to a user folder.",
        },
        Tr::Close => match lang {
            Uk => "Закрити",
            Ru => "Закрыть",
            En => "Close",
        },
        Tr::TrayShow => match lang {
            Uk => "Показати Sich",
            Ru => "Показать Sich",
            En => "Show Sich",
        },
        Tr::TrayRestart => match lang {
            Uk => "Перезапустити Sich",
            Ru => "Перезапустить Sich",
            En => "Restart Sich",
        },
        Tr::TrayQuit => match lang {
            Uk => "Вийти з Sich",
            Ru => "Выйти из Sich",
            En => "Quit Sich",
        },
        Tr::QuitConfirmTitle => match lang {
            Uk => "Вийти з Sich?",
            Ru => "Выйти из Sich?",
            En => "Quit Sich?",
        },
        Tr::QuitConfirmText => match lang {
            Uk => "Агенти ще працюють. Вийти й зупинити всі термінали?",
            Ru => "Агенты ещё работают. Выйти и остановить все терминалы?",
            En => "Agents are still running. Quit and stop all terminals?",
        },
        Tr::Quit => match lang {
            Uk => "Вийти",
            Ru => "Выйти",
            En => "Quit",
        },
        Tr::TrayWaiting => match lang {
            Uk => "чекає",
            Ru => "ждёт",
            En => "waiting",
        },
        Tr::TrayWorking => match lang {
            Uk => "працює",
            Ru => "работает",
            En => "working",
        },
        // ---- comments keys: append below ----
        Tr::CommentsTitle => match lang {
            Uk => "Коментарі",
            Ru => "Комментарии",
            En => "Comments",
        },
        Tr::CommentsSendUnresolved => match lang {
            Uk => "Надіслати невирішені агенту",
            Ru => "Отправить нерешённые агенту",
            En => "Send unresolved to agent",
        },
        Tr::CommentsModeGrouped => match lang {
            Uk => "Показати за потоками",
            Ru => "Показать по веткам",
            En => "Show grouped",
        },
        Tr::CommentsModeTimeline => match lang {
            Uk => "Показати хронологічно",
            Ru => "Показать хронологически",
            En => "Show as timeline",
        },
        Tr::CommentsAdd => match lang {
            Uk => "Додати коментар",
            Ru => "Добавить комментарий",
            En => "Add comment",
        },
        Tr::CommentsFilterAll => match lang {
            Uk => "Усі",
            Ru => "Все",
            En => "All",
        },
        Tr::CommentsFilterHumans => match lang {
            Uk => "Люди",
            Ru => "Люди",
            En => "Humans",
        },
        Tr::CommentsFilterBots => match lang {
            Uk => "Боти",
            Ru => "Боты",
            En => "Bots",
        },
        Tr::CommentsNeedsReview => match lang {
            Uk => "Потребують уваги",
            Ru => "Требуют внимания",
            En => "Needs review",
        },
        Tr::CommentsConversation => match lang {
            Uk => "Обговорення",
            Ru => "Обсуждение",
            En => "Conversation",
        },
        Tr::CommentsResolved => match lang {
            Uk => "Вирішені",
            Ru => "Решённые",
            En => "Resolved",
        },
        Tr::CommentsTimeline => match lang {
            Uk => "Хронологія",
            Ru => "Хронология",
            En => "Timeline",
        },
        Tr::CommentsOutdated => match lang {
            Uk => "застарілий",
            Ru => "устарел",
            En => "outdated",
        },
        Tr::CommentsBot => match lang {
            Uk => "бот",
            Ru => "бот",
            En => "bot",
        },
        Tr::CommentsReply => match lang {
            Uk => "Відповісти",
            Ru => "Ответить",
            En => "Reply",
        },
        Tr::CommentsResolve => match lang {
            Uk => "Вирішити",
            Ru => "Решить",
            En => "Resolve",
        },
        Tr::CommentsUnresolve => match lang {
            Uk => "Відновити",
            Ru => "Вернуть",
            En => "Unresolve",
        },
        Tr::CommentsEdit => match lang {
            Uk => "Редагувати",
            Ru => "Редактировать",
            En => "Edit",
        },
        Tr::CommentsDelete => match lang {
            Uk => "Видалити",
            Ru => "Удалить",
            En => "Delete",
        },
        Tr::CommentsDeleteWarning => match lang {
            Uk => "Видалити цей коментар з GitHub? Дію не можна скасувати.",
            Ru => "Удалить этот комментарий на GitHub? Действие нельзя отменить.",
            En => "Delete this comment from GitHub? This cannot be undone.",
        },
        Tr::CommentsSend => match lang {
            Uk => "Надіслати",
            Ru => "Отправить",
            En => "Send",
        },
        Tr::CommentsSave => match lang {
            Uk => "Зберегти",
            Ru => "Сохранить",
            En => "Save",
        },
        Tr::CommentsPlaceholder => match lang {
            Uk => "Напишіть коментар… (Ctrl+Enter — надіслати)",
            Ru => "Напишите комментарий… (Ctrl+Enter — отправить)",
            En => "Write a comment… (Ctrl+Enter to send)",
        },
        Tr::CommentsEmpty => match lang {
            Uk => "Коментарів ще немає.",
            Ru => "Комментариев пока нет.",
            En => "No comments yet.",
        },
        Tr::CommentsEmptyHumans => match lang {
            Uk => "Коментарів від людей немає.",
            Ru => "Комментариев от людей нет.",
            En => "No human comments.",
        },
        Tr::CommentsEmptyBots => match lang {
            Uk => "Коментарів від ботів немає.",
            Ru => "Комментариев от ботов нет.",
            En => "No bot comments.",
        },
        Tr::CommentsTruncated => match lang {
            Uk => "Показано перші {n} коментарів",
            Ru => "Показаны первые {n} комментариев",
            En => "Showing first {n} comments",
        },
        Tr::CommentsLoading => match lang {
            Uk => "Завантаження коментарів…",
            Ru => "Загрузка комментариев…",
            En => "Loading comments…",
        },
        Tr::CommentsAddReaction => match lang {
            Uk => "Додати реакцію",
            Ru => "Добавить реакцию",
            En => "Add reaction",
        },
        // ---- end comments ----

        // ---- checks keys: append below ----
        Tr::PrUpdated => match lang {
            Uk => "Оновлено {t}",
            Ru => "Обновлено {t}",
            En => "Updated {t}",
        },
        Tr::PrMergeCommit => match lang {
            Uk => "Створити merge-коміт",
            Ru => "Создать merge-коммит",
            En => "Create a merge commit",
        },
        Tr::PrAutoMergeEnable => match lang {
            Uk => "Увімкнути авто-злиття",
            Ru => "Включить авто-слияние",
            En => "Enable auto-merge",
        },
        Tr::PrAutoMergeDisable => match lang {
            Uk => "Вимкнути авто-злиття",
            Ru => "Выключить авто-слияние",
            En => "Disable auto-merge",
        },
        Tr::PrStatusAutoMergeOn => match lang {
            Uk => "Авто-злиття увімкнено",
            Ru => "Авто-слияние включено",
            En => "Auto-merge on",
        },
        Tr::PrStatusChecksFailed => match lang {
            Uk => "Перевірки не пройдені",
            Ru => "Проверки не пройдены",
            En => "Checks failed",
        },
        Tr::PrStatusChecksPending => match lang {
            Uk => "Перевірки тривають",
            Ru => "Проверки идут",
            En => "Checks pending",
        },
        Tr::PrStatusAble => match lang {
            Uk => "Можна зливати",
            Ru => "Можно сливать",
            En => "Able to merge",
        },
        Tr::PrStatusChecking => match lang {
            Uk => "Перевіряємо",
            Ru => "Проверяем",
            En => "Checking",
        },
        Tr::PrReadyForReview => match lang {
            Uk => "Готово до рев'ю",
            Ru => "Готово к ревью",
            En => "Ready for review",
        },
        Tr::PrCloseShort => match lang {
            Uk => "Закрити",
            Ru => "Закрыть",
            En => "Close",
        },
        Tr::PrCheckPassing => match lang {
            Uk => "{n} успішних",
            Ru => "{n} успешных",
            En => "{n} passing",
        },
        Tr::PrCheckFailing => match lang {
            Uk => "{n} невдалих",
            Ru => "{n} упавших",
            En => "{n} failing",
        },
        Tr::PrCheckPendingN => match lang {
            Uk => "{n} у процесі",
            Ru => "{n} в процессе",
            En => "{n} pending",
        },
        Tr::PrCheckSkipped => match lang {
            Uk => "{n} пропущених",
            Ru => "{n} пропущено",
            En => "{n} skipped",
        },
        Tr::PrChecksNone => match lang {
            Uk => "Перевірок немає",
            Ru => "Проверок нет",
            En => "No checks",
        },
        Tr::PrRerunAll => match lang {
            Uk => "Перезапустити всі",
            Ru => "Перезапустить все",
            En => "Rerun all",
        },
        Tr::PrCheckStarted => match lang {
            Uk => "Розпочато",
            Ru => "Начато",
            En => "Started",
        },
        Tr::PrCheckFinished => match lang {
            Uk => "Завершено",
            Ru => "Завершено",
            En => "Finished",
        },
        Tr::PrCheckApproval => match lang {
            Uk => "очікує схвалення",
            Ru => "ожидает одобрения",
            En => "waiting for approval",
        },
        Tr::PrCheckFailedJobs => match lang {
            Uk => "Невдалі завдання",
            Ru => "Упавшие задачи",
            En => "Failed jobs",
        },
        Tr::PrCheckFailedSteps => match lang {
            Uk => "Невдалі кроки",
            Ru => "Упавшие шаги",
            En => "Failed steps",
        },
        Tr::PrCheckOutput => match lang {
            Uk => "Вивід",
            Ru => "Вывод",
            En => "Output",
        },
        Tr::PrCheckAnnotations => match lang {
            Uk => "Анотації",
            Ru => "Аннотации",
            En => "Annotations",
        },
        Tr::PrCheckNoDetails => match lang {
            Uk => "Для цієї перевірки немає вбудованих деталей.",
            Ru => "Для этой проверки нет встроенных деталей.",
            En => "No inline details are available for this check.",
        },
        Tr::PrCheckLoading => match lang {
            Uk => "Завантаження деталей…",
            Ru => "Загрузка деталей…",
            En => "Loading details…",
        },
        Tr::PrCheckCopyLog => match lang {
            Uk => "Копіювати лог",
            Ru => "Копировать лог",
            En => "Copy log",
        },
        Tr::PrCheckNoLog => match lang {
            Uk => "Лог недоступний",
            Ru => "Лог недоступен",
            En => "Log not available",
        },
        Tr::PrConflictsTitle => match lang {
            Uk => "У цієї гілки є конфлікти, які треба розв'язати",
            Ru => "У этой ветки есть конфликты, которые нужно разрешить",
            En => "This branch has conflicts that must be resolved",
        },
        Tr::PrConflictsBehind => match lang {
            Uk => "{n} комітів позаду {base} (базовий коміт {sha})",
            Ru => "{n} коммитов позади {base} (базовый коммит {sha})",
            En => "{n} commits behind {base} (base commit {sha})",
        },
        Tr::PrConflictsBehindOne => match lang {
            Uk => "1 коміт позаду {base} (базовий коміт {sha})",
            Ru => "1 коммит позади {base} (базовый коммит {sha})",
            En => "1 commit behind {base} (base commit {sha})",
        },
        Tr::PrConflictsNotReproduced => match lang {
            Uk => "GitHub повідомляє про конфлікти, але локальний Git їх не відтворив. Отримайте базову гілку (fetch) і спробуйте ще раз.",
            Ru => "GitHub сообщает о конфликтах, но локальный Git их не воспроизвёл. Получите базовую ветку (fetch) и попробуйте снова.",
            En => "GitHub reports conflicts, but local Git did not reproduce them. Fetch the base branch and try again.",
        },
        Tr::PrConflictsSend => match lang {
            Uk => "Надіслати конфлікти агенту",
            Ru => "Отправить конфликты агенту",
            En => "Send conflicts to agent",
        },
        // ---- end checks ----

        // ---- details keys: append below ----
        Tr::DetTitle => match lang {
            Uk => "Деталі",
            Ru => "Детали",
            En => "Details",
        },
        Tr::DetOpenedBy => match lang {
            Uk => "відкрив @{user} · {when}",
            Ru => "открыл @{user} · {when}",
            En => "opened by @{user} · {when}",
        },
        Tr::DetNoDescription => match lang {
            Uk => "Без опису",
            Ru => "Без описания",
            En => "No description",
        },
        Tr::DetShowMore => match lang {
            Uk => "Показати більше",
            Ru => "Показать больше",
            En => "Show more",
        },
        Tr::DetShowLess => match lang {
            Uk => "Показати менше",
            Ru => "Показать меньше",
            En => "Show less",
        },
        Tr::DetEdit => match lang {
            Uk => "Редагувати",
            Ru => "Редактировать",
            En => "Edit",
        },
        Tr::DetReviewers => match lang {
            Uk => "Рецензенти",
            Ru => "Ревьюеры",
            En => "Reviewers",
        },
        Tr::DetLabels => match lang {
            Uk => "Мітки",
            Ru => "Метки",
            En => "Labels",
        },
        Tr::DetAssignees => match lang {
            Uk => "Виконавці",
            Ru => "Исполнители",
            En => "Assignees",
        },
        Tr::DetAdd => match lang {
            Uk => "Додати",
            Ru => "Добавить",
            En => "Add",
        },
        Tr::DetRemove => match lang {
            Uk => "Прибрати",
            Ru => "Убрать",
            En => "Remove",
        },
        Tr::DetNone => match lang {
            Uk => "Немає",
            Ru => "Нет",
            En => "None",
        },
        Tr::DetLoading => match lang {
            Uk => "Завантаження…",
            Ru => "Загрузка…",
            En => "Loading…",
        },
        Tr::DetReviewApproved => match lang {
            Uk => "Схвалено",
            Ru => "Одобрено",
            En => "Approved",
        },
        Tr::DetReviewChanges => match lang {
            Uk => "Запросив зміни",
            Ru => "Запросил изменения",
            En => "Changes requested",
        },
        Tr::DetReviewCommented => match lang {
            Uk => "Залишив коментар",
            Ru => "Оставил комментарий",
            En => "Commented",
        },
        Tr::DetReviewPending => match lang {
            Uk => "Очікує рев'ю",
            Ru => "Ожидает ревью",
            En => "Review pending",
        },
        Tr::DetEditTitle => match lang {
            Uk => "Редагувати pull request",
            Ru => "Редактировать pull request",
            En => "Edit pull request",
        },
        Tr::DetSave => match lang {
            Uk => "Зберегти",
            Ru => "Сохранить",
            En => "Save",
        },
        Tr::DetPickReviewer => match lang {
            Uk => "Додати рецензента",
            Ru => "Добавить ревьюера",
            En => "Add reviewer",
        },
        Tr::DetPickLabel => match lang {
            Uk => "Додати мітку",
            Ru => "Добавить метку",
            En => "Add label",
        },
        Tr::DetPickAssignee => match lang {
            Uk => "Додати виконавця",
            Ru => "Добавить исполнителя",
            En => "Add assignee",
        },
        Tr::DetPickerFilter => match lang {
            Uk => "Фільтр або ім'я",
            Ru => "Фильтр или имя",
            En => "Filter or type a name",
        },
        Tr::DetPickerCustom => match lang {
            Uk => "Додати «{name}»",
            Ru => "Добавить «{name}»",
            En => "Add «{name}»",
        },
        Tr::DetPickerNoMatches => match lang {
            Uk => "Нічого не знайдено",
            Ru => "Ничего не найдено",
            En => "No matches",
        },
        Tr::FilesTitle => match lang {
            Uk => "Змінені файли",
            Ru => "Изменённые файлы",
            En => "Files changed",
        },
        Tr::FilesViewed => match lang {
            Uk => "Переглянуто {k}/{n}",
            Ru => "Просмотрено {k}/{n}",
            En => "Viewed {k}/{n}",
        },
        Tr::FilesFilter => match lang {
            Uk => "Фільтр файлів",
            Ru => "Фильтр файлов",
            En => "Filter files",
        },
        Tr::FilesFetchHint => match lang {
            Uk => "Отримайте гілку (fetch), щоб бачити різницю",
            Ru => "Получите ветку (fetch), чтобы видеть diff",
            En => "Fetch the branch to view diffs",
        },
        Tr::FilesLoading => match lang {
            Uk => "Завантаження файлів…",
            Ru => "Загрузка файлов…",
            En => "Loading files…",
        },
        Tr::FilesNone => match lang {
            Uk => "Немає змін",
            Ru => "Нет изменений",
            En => "No changed files",
        },
        Tr::FilesMarkViewed => match lang {
            Uk => "Позначити переглянутим",
            Ru => "Отметить просмотренным",
            En => "Mark as viewed",
        },
        // ---- end details ----

        // ---- history keys: append below ----
        Tr::GitCopySha => match lang {
            Uk => "Копіювати SHA",
            Ru => "Копировать SHA",
            En => "Copy SHA",
        },
        Tr::GitCopyMessage => match lang {
            Uk => "Копіювати повідомлення",
            Ru => "Копировать сообщение",
            En => "Copy message",
        },
        Tr::GitOpenCommitOnGitHub => match lang {
            Uk => "Відкрити на GitHub",
            Ru => "Открыть на GitHub",
            En => "Open on GitHub",
        },
        Tr::GitCherryPick => match lang {
            Uk => "Cherry-pick на поточну гілку",
            Ru => "Cherry-pick на текущую ветку",
            En => "Cherry-pick onto current branch",
        },
        Tr::GitCherryPickConfirm => match lang {
            Uk => "Застосувати цей коміт поверх поточної гілки?",
            Ru => "Применить этот коммит поверх текущей ветки?",
            En => "Apply this commit on top of the current branch?",
        },
        Tr::GitRevert => match lang {
            Uk => "Скасувати коміт (revert)",
            Ru => "Отменить коммит (revert)",
            En => "Revert commit",
        },
        Tr::GitRevertConfirm => match lang {
            Uk => "Створити коміт, що скасовує цю зміну?",
            Ru => "Создать коммит, отменяющий это изменение?",
            En => "Create a commit that reverts this change?",
        },
        Tr::GitNothingToApply => match lang {
            Uk => "Нічого застосовувати: зміни вже є",
            Ru => "Нечего применять: изменения уже есть",
            En => "Nothing to apply: the changes are already there",
        },
        // ---- end history ----

        // ---- compare keys: append below ----
        Tr::CmpVs => match lang {
            Uk => "відносно",
            Ru => "относительно",
            En => "vs",
        },
        Tr::CmpAheadOf => match lang {
            Uk => "комітів попереду",
            Ru => "коммитов впереди",
            En => "commits ahead of",
        },
        Tr::CmpBehindOf => match lang {
            Uk => "комітів позаду",
            Ru => "коммитов позади",
            En => "commits behind",
        },
        Tr::CmpBaseTooltip => match lang {
            Uk => "Базова гілка порівняння. Натисніть, щоб змінити",
            Ru => "Базовая ветка сравнения. Нажмите, чтобы изменить",
            En => "Comparison base. Click to change",
        },
        Tr::CmpChangeBase => match lang {
            Uk => "Змінити базу",
            Ru => "Изменить базу",
            En => "Change base",
        },
        Tr::CmpBaseMissing => match lang {
            Uk => "Базу не знайдено",
            Ru => "База не найдена",
            En => "Base not found",
        },
        Tr::CmpNoCommonHistory => match lang {
            Uk => "Немає спільної історії з базою",
            Ru => "Нет общей истории с базой",
            En => "No common history with the base",
        },
        Tr::CmpResetBase => match lang {
            Uk => "Скинути до типової",
            Ru => "Сбросить на стандартную",
            En => "Reset to default",
        },
        Tr::CmpBranchChanges => match lang {
            Uk => "Зміни гілки",
            Ru => "Изменения ветки",
            En => "Branch changes",
        },
        Tr::CmpMergeConflicts => match lang {
            Uk => "Конфлікти злиття",
            Ru => "Конфликты слияния",
            En => "Merge conflicts",
        },
        Tr::CmpRebaseConflicts => match lang {
            Uk => "Конфлікти ребейзу",
            Ru => "Конфликты ребейза",
            En => "Rebase conflicts",
        },
        Tr::CmpCherryPickConflicts => match lang {
            Uk => "Конфлікти cherry-pick",
            Ru => "Конфликты cherry-pick",
            En => "Cherry-pick conflicts",
        },
        Tr::CmpRevertConflicts => match lang {
            Uk => "Конфлікти revert",
            Ru => "Конфликты revert",
            En => "Revert conflicts",
        },
        Tr::CmpUnresolved => match lang {
            Uk => "не розв'язано",
            Ru => "не разрешено",
            En => "unresolved",
        },
        Tr::CmpResolveWithAgent => match lang {
            Uk => "Розв'язати агентом",
            Ru => "Разрешить агентом",
            En => "Resolve with agent",
        },
        Tr::CmpAbortMerge => match lang {
            Uk => "Перервати злиття",
            Ru => "Прервать слияние",
            En => "Abort merge",
        },
        Tr::CmpAbortRebase => match lang {
            Uk => "Перервати ребейз",
            Ru => "Прервать ребейз",
            En => "Abort rebase",
        },
        Tr::CmpAbortCherryPick => match lang {
            Uk => "Перервати cherry-pick",
            Ru => "Прервать cherry-pick",
            En => "Abort cherry-pick",
        },
        Tr::CmpAbortRevert => match lang {
            Uk => "Перервати revert",
            Ru => "Прервать revert",
            En => "Abort revert",
        },
        Tr::CmpBothModified => match lang {
            Uk => "змінено обома",
            Ru => "изменено обеими",
            En => "both modified",
        },
        Tr::CmpBothAdded => match lang {
            Uk => "додано обома",
            Ru => "добавлено обеими",
            En => "both added",
        },
        Tr::CmpBothDeleted => match lang {
            Uk => "видалено обома",
            Ru => "удалено обеими",
            En => "both deleted",
        },
        Tr::CmpAddedByUs => match lang {
            Uk => "додано нами",
            Ru => "добавлено нами",
            En => "added by us",
        },
        Tr::CmpAddedByThem => match lang {
            Uk => "додано ними",
            Ru => "добавлено ими",
            En => "added by them",
        },
        Tr::CmpDeletedByUs => match lang {
            Uk => "видалено нами",
            Ru => "удалено нами",
            En => "deleted by us",
        },
        Tr::CmpDeletedByThem => match lang {
            Uk => "видалено ними",
            Ru => "удалено ими",
            En => "deleted by them",
        },
        Tr::CmpMarkResolved => match lang {
            Uk => "Розв'язано",
            Ru => "Разрешено",
            En => "Mark resolved",
        },
        Tr::CmpKeepFile => match lang {
            Uk => "Залишити файл",
            Ru => "Оставить файл",
            En => "Keep file",
        },
        Tr::CmpKeepDeletion => match lang {
            Uk => "Видалити",
            Ru => "Удалить",
            En => "Keep deletion",
        },
        Tr::CmpCompleteMerge => match lang {
            Uk => "Завершити злиття",
            Ru => "Завершить слияние",
            En => "Complete merge",
        },
        Tr::CmpAlreadyApplied => match lang {
            Uk => "Вже застосовано: крок нічого не змінив і був пропущений",
            Ru => "Уже применено: шаг ничего не изменил и был пропущен",
            En => "Already applied: the step changed nothing and was skipped",
        },
        // ---- end compare ----

        // ---- prwt keys: append below ----
        // ---- end prwt ----

        // ---- notes keys: append below ----
        // ---- end notes ----
    }
}

/// Tooltip of an agent badge: `label`, plus `: detail` when there is one.
pub fn agent_tooltip(label: &str, detail: Option<&str>) -> String {
    match detail.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => format!("{label}: {d}"),
        None => label.to_string(),
    }
}

pub fn diff_eol_text(lang: Language, old: LineEndings, new: LineEndings, only: bool) -> String {
    let label = |style| match style {
        LineEndings::Lf => "LF",
        LineEndings::Crlf => "CRLF",
        LineEndings::Cr => "CR",
        LineEndings::Mixed => tr(lang, Tr::DiffEolMixed),
        LineEndings::None => tr(lang, Tr::DiffEolNone),
    };
    let change = format!("{} → {}", label(old), label(new));
    match (lang, only) {
        (Language::Uk, false) => format!("Закінчення рядків: {change}"),
        (Language::Ru, false) => format!("Окончания строк: {change}"),
        (Language::En, false) => format!("Line endings: {change}"),
        (Language::Uk, true) => format!("Змінено лише закінчення рядків ({change})"),
        (Language::Ru, true) => format!("Изменились только окончания строк ({change})"),
        (Language::En, true) => format!("Only line endings changed ({change})"),
    }
}

pub fn diff_unchanged_lines(lang: Language, count: usize) -> String {
    match lang {
        Language::Uk => format!("⋯ Незмінені рядки ({count})"),
        Language::Ru => format!("⋯ Неизменённые строки ({count})"),
        Language::En if count == 1 => "⋯ 1 unchanged line".to_owned(),
        Language::En => format!("⋯ {count} unchanged lines"),
    }
}

/// Compact age of `since_ms` at `now_ms`: "now", "5m", "2h", "3d" (translated). A `since` in
/// the future (clock skew) counts as "now".
pub fn relative_time(lang: Language, now_ms: u64, since_ms: u64) -> String {
    let secs = now_ms.saturating_sub(since_ms) / 1000;
    let (n, unit) = if secs < 60 {
        return match lang {
            Language::Uk => "щойно",
            Language::Ru => "только что",
            Language::En => "now",
        }
        .to_string();
    } else if secs < 3600 {
        (secs / 60, 0)
    } else if secs < 86_400 {
        (secs / 3600, 1)
    } else {
        (secs / 86_400, 2)
    };
    let suffix = match (lang, unit) {
        (Language::Uk, 0) => "хв",
        (Language::Ru, 0) => "м",
        (Language::En, 0) => "m",
        (Language::Uk, 1) => "год",
        (Language::Ru, 1) => "ч",
        (Language::En, 1) => "h",
        (_, _) => match lang {
            Language::En => "d",
            _ => "д",
        },
    };
    format!("{n}{suffix}")
}

/// Status line under the hooks switch when hooks are installed.
pub fn hooks_installed(lang: Language, events: Option<usize>, claude_version: Option<&str>) -> String {
    let head = match lang {
        Language::Uk => "Хуки встановлено",
        Language::Ru => "Хуки установлены",
        Language::En => "Hooks installed",
    };
    let mut parts = Vec::new();
    if let Some(n) = events {
        parts.push(match lang {
            Language::Uk => format!("подій: {n}"),
            Language::Ru => format!("событий: {n}"),
            Language::En => format!("{n} events"),
        });
    }
    if let Some(v) = claude_version {
        parts.push(format!("Claude v{v}"));
    }
    if parts.is_empty() { head.to_string() } else { format!("{head} ({})", parts.join(", ")) }
}

/// Status line under the Codex hooks switch when hooks are installed.
pub fn codex_hooks_installed(lang: Language, events: Option<usize>, codex_version: Option<&str>) -> String {
    let head = match lang {
        Language::Uk => "Хуки встановлено",
        Language::Ru => "Хуки установлены",
        Language::En => "Hooks installed",
    };
    let mut parts = Vec::new();
    if let Some(n) = events {
        parts.push(match lang {
            Language::Uk => format!("подій: {n}"),
            Language::Ru => format!("событий: {n}"),
            Language::En => format!("{n} events"),
        });
    }
    if let Some(v) = codex_version {
        parts.push(format!("Codex v{v}"));
    }
    if parts.is_empty() { head.to_string() } else { format!("{head} ({})", parts.join(", ")) }
}

/// The "+N" row under an agent when more subagents run than are listed.
pub fn more_subagents(lang: Language, n: usize) -> String {
    match lang {
        Language::Uk => format!("ще {n} субагентів"),
        Language::Ru => format!("ещё {n} субагентов"),
        Language::En => format!("{n} more subagents"),
    }
}

pub fn connected(lang: Language, pid: u32) -> String {
    match lang {
        Language::Uk => format!("Підключено до демона (pid {pid})"),
        Language::Ru => format!("Подключено к демону (pid {pid})"),
        Language::En => format!("Connected to daemon (pid {pid})"),
    }
}

pub fn update_available(lang: Language, version: &str) -> String {
    match lang {
        Language::Uk => format!("Доступна версія {version}"),
        Language::Ru => format!("Доступна версия {version}"),
        Language::En => format!("Version {version} available"),
    }
}

pub fn up_to_date(lang: Language, current: &str) -> String {
    match lang {
        Language::Uk => format!("У вас остання версія ({current})"),
        Language::Ru => format!("У вас последняя версия ({current})"),
        Language::En => format!("You're up to date ({current})"),
    }
}

pub fn check_failed(lang: Language, err: &str) -> String {
    match lang {
        Language::Uk => format!("Не вдалося перевірити: {err}"),
        Language::Ru => format!("Не удалось проверить: {err}"),
        Language::En => format!("Check failed: {err}"),
    }
}

pub fn update_failed(lang: Language, err: &str) -> String {
    match lang {
        Language::Uk => format!("Не вдалося оновити: {err}"),
        Language::Ru => format!("Не удалось обновить: {err}"),
        Language::En => format!("Update failed: {err}"),
    }
}

/// "{done} / {total} МБ ({pct}%)"; sizes in MiB with one decimal.
pub fn download_progress(lang: Language, done: u64, total: u64) -> String {
    let mb = |n: u64| n as f64 / (1024.0 * 1024.0);
    let unit = match lang {
        Language::Uk | Language::Ru => "МБ",
        Language::En => "MB",
    };
    format!("{:.1} / {:.1} {unit} ({}%)", mb(done), mb(total), crate::updater::percent(done, total))
}

pub fn new_worktree_title(lang: Language, project: &str) -> String {
    format!("{} \u{2014} {project}", tr(lang, Tr::NewWorktree))
}

pub fn remove_worktree_text(lang: Language, name: &str) -> String {
    match lang {
        Language::Uk => format!("Видалити worktree \u{ab}{name}\u{bb}? Його теку буде видалено, а термінали закрито."),
        Language::Ru => format!("Удалить worktree \u{ab}{name}\u{bb}? Его папка будет удалена, а терминалы закрыты."),
        Language::En => format!("Remove worktree \u{ab}{name}\u{bb}? Its folder will be deleted and its terminals closed."),
    }
}

pub fn also_delete_branch(lang: Language, name: &str) -> String {
    match lang {
        Language::Uk => format!("Також видалити гілку \u{ab}{name}\u{bb}"),
        Language::Ru => format!("Также удалить ветку \u{ab}{name}\u{bb}"),
        Language::En => format!("Also delete branch \u{ab}{name}\u{bb}"),
    }
}

pub fn delete_group_text(lang: Language, name: &str) -> String {
    let note = tr(lang, Tr::DeleteGroupConfirm);
    match lang {
        Language::Uk => format!("Видалити групу \u{ab}{name}\u{bb}? {note}"),
        Language::Ru => format!("Удалить группу \u{ab}{name}\u{bb}? {note}"),
        Language::En => format!("Delete group \u{ab}{name}\u{bb}? {note}"),
    }
}

pub fn remove_project_text(lang: Language, name: &str) -> String {
    match lang {
        Language::Uk => format!("Видалити проєкт \u{ab}{name}\u{bb} із Sich? Файли на диску не буде змінено."),
        Language::Ru => format!("Удалить проект \u{ab}{name}\u{bb} из Sich? Файлы на диске не будут затронуты."),
        Language::En => format!("Remove project \u{ab}{name}\u{bb} from Sich? Files on disk are not touched."),
    }
}

/// Status-bar text for a daemon with another protocol version (`None` = older than v2).
pub fn daemon_incompatible(lang: Language, version: Option<u32>, need: u32) -> String {
    match (lang, version) {
        (Language::Uk, Some(v)) => format!("Запущений демон несумісний (протокол v{v}, потрібен v{need})"),
        (Language::Uk, None) => format!("Запущений демон несумісний (старий протокол, потрібен v{need})"),
        (Language::Ru, Some(v)) => format!("Запущенный демон несовместим (протокол v{v}, нужен v{need})"),
        (Language::Ru, None) => format!("Запущенный демон несовместим (старый протокол, нужен v{need})"),
        (Language::En, Some(v)) => format!("Running daemon is incompatible (protocol v{v}, need v{need})"),
        (Language::En, None) => format!("Running daemon is incompatible (older protocol, need v{need})"),
    }
}

/// Tab text of an exited terminal.
pub fn exited_tab(lang: Language, title: &str) -> String {
    format!("{title} ({})", tr(lang, Tr::Exited))
}

/// Overlay text for a finished process, with the exit code when known.
pub fn process_exited(lang: Language, code: Option<i32>) -> String {
    let head = tr(lang, Tr::ProcessExited);
    let tail = tr(lang, Tr::CloseFromSidebar);
    match code {
        Some(c) => match lang {
            Language::Uk | Language::Ru => format!("{head} (код {c}). {tail}"),
            Language::En => format!("{head} (code {c}). {tail}"),
        },
        None => format!("{head}. {tail}"),
    }
}


pub fn usage_updated(lang: Language, minutes: i64) -> String {
    match lang {
        Language::Uk => format!("оновлено {minutes} хв тому"),
        Language::Ru => format!("обновлено {minutes} мин назад"),
        Language::En => format!("updated {minutes} min ago"),
    }
}

pub fn usage_window(lang: Language, used: u8) -> String {
    match lang {
        Language::Uk => format!("використано {used}%"),
        Language::Ru => format!("использовано {used}%"),
        Language::En => format!("{used}% used"),
    }
}

pub fn usage_weekday(lang: Language, day: chrono::Weekday) -> &'static str {
    let names = match lang {
        Language::Uk => ["пн", "вт", "ср", "чт", "пт", "сб", "нд"],
        Language::Ru => ["пн", "вт", "ср", "чт", "пт", "сб", "вс"],
        Language::En => ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"],
    };
    names[day.num_days_from_monday() as usize]
}

pub fn usage_countdown(lang: Language, minutes: i64) -> String {
    format!("[{} {}]", usage_until(lang), usage_remaining(lang, minutes, true))
}

pub fn usage_reset(lang: Language, minutes: i64, clock: &str) -> String {
    let reset = match lang {
        Language::Uk => "Скидання",
        Language::Ru => "Сброс",
        Language::En => "Resets",
    };
    format!("{reset}: {clock} · {} {}", usage_until(lang), usage_remaining(lang, minutes, false))
}

fn usage_until(lang: Language) -> &'static str {
    match lang {
        Language::Uk | Language::Ru => "через",
        Language::En => "in",
    }
}

fn usage_remaining(lang: Language, minutes: i64, compact: bool) -> impl std::fmt::Display {
    let days = minutes / (24 * 60);
    let hours = minutes / 60 % 24;
    let minutes = minutes % 60;
    let (d, h, m) = match (lang, compact) {
        (Language::Uk, true) => ("д", "г", "хв"),
        (Language::Uk, false) => ("д", "год", "хв"),
        (Language::Ru, _) => ("д", "ч", "мин"),
        (Language::En, true) => ("d", "h", "m"),
        (Language::En, false) => ("d", "h", "min"),
    };
    let gap = if compact { "" } else { " " };
    std::fmt::from_fn(move |f| {
        if days > 0 {
            write!(f, "{days}{gap}{d}")?;
            if !compact || hours > 0 {
                write!(f, " {hours}{gap}{h}")?;
            }
            Ok(())
        } else if hours > 0 {
            write!(f, "{hours}{gap}{h}")?;
            if !compact || minutes > 0 {
                write!(f, " {minutes}{gap}{m}")?;
            }
            Ok(())
        } else if compact && minutes == 0 {
            write!(f, "<1{m}")
        } else {
            write!(f, "{minutes}{gap}{m}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_has_text_in_every_language() {
        for &key in Tr::ALL {
            for lang in Language::ALL {
                assert!(!tr(lang, key).trim().is_empty(), "{key:?} is empty for {lang:?}");
            }
        }
    }

    #[test]
    fn languages_differ_for_user_facing_words() {
        for key in [Tr::Home, Tr::Projects, Tr::Cancel, Tr::MainTag] {
            assert_ne!(tr(Language::Uk, key), tr(Language::En, key), "{key:?}");
            assert_ne!(tr(Language::Ru, key), tr(Language::En, key), "{key:?}");
        }
    }

    #[test]
    fn parameterized_strings_are_filled_in_all_languages() {
        for lang in Language::ALL {
            assert!(remove_worktree_text(lang, "feat").contains("feat"));
            assert!(also_delete_branch(lang, "feat").contains("feat"));
            assert!(remove_project_text(lang, "proj").contains("proj"));
            assert!(delete_group_text(lang, "grp").contains("grp"));
            assert!(new_worktree_title(lang, "proj").contains("proj"));
            assert!(daemon_incompatible(lang, Some(1), 2).contains("v1"));
            assert!(daemon_incompatible(lang, None, 2).contains("v2"));
            assert!(exited_tab(lang, "pwsh").starts_with("pwsh ("));
            assert!(more_subagents(lang, 7).contains('7'));
            assert!(update_available(lang, "1.2.3").contains("1.2.3"));
            assert!(update_failed(lang, "boom").contains("boom"));
            assert!(up_to_date(lang, "1.2.3").contains("1.2.3"));
            assert!(check_failed(lang, "boom").contains("boom"));
            assert!(download_progress(lang, 1024 * 1024, 4 * 1024 * 1024).contains("(25%)"));
            assert!(hooks_installed(lang, Some(9), Some("2.1.3")).contains('9'));
            assert!(hooks_installed(lang, Some(9), Some("2.1.3")).contains("v2.1.3"));
            assert!(!hooks_installed(lang, None, None).contains('('));
            assert!(codex_hooks_installed(lang, Some(8), Some("0.170.0")).contains("Codex v0.170.0"));
            assert!(!codex_hooks_installed(lang, None, None).contains('('));
        }
    }

    #[test]
    fn diff_eol_messages_include_the_ordered_styles_in_all_languages() {
        for lang in Language::ALL {
            for only in [false, true] {
                assert!(diff_eol_text(lang, LineEndings::Lf, LineEndings::Crlf, only)
                    .contains("LF → CRLF"));
            }
        }
        assert_eq!(diff_eol_text(Language::En, LineEndings::Mixed, LineEndings::Lf, false),
                   "Line endings: mixed → LF");
        assert_eq!(diff_eol_text(Language::En, LineEndings::Lf, LineEndings::Crlf, true),
                   "Only line endings changed (LF → CRLF)");
    }

    #[test]
    fn diff_fold_text_includes_count_in_all_languages() {
        assert_eq!(diff_unchanged_lines(Language::En, 1), "⋯ 1 unchanged line");
        assert_eq!(diff_unchanged_lines(Language::En, 8), "⋯ 8 unchanged lines");
        for lang in Language::ALL {
            assert!(diff_unchanged_lines(lang, 42).contains("42"));
        }
    }

    #[test]
    fn relative_time_all_languages_and_boundaries() {
        let now = 10_000_000_000u64;
        let rt = |lang, ago_s: u64| relative_time(lang, now, now - ago_s * 1000);
        let cases: [(u64, [&str; 3]); 11] = [
            (0, ["щойно", "только что", "now"]),
            (59, ["щойно", "только что", "now"]),
            (60, ["1хв", "1м", "1m"]),
            (300, ["5хв", "5м", "5m"]),
            (3599, ["59хв", "59м", "59m"]),
            (3600, ["1год", "1ч", "1h"]),
            (7200, ["2год", "2ч", "2h"]),
            (86_399, ["23год", "23ч", "23h"]),
            (86_400, ["1д", "1д", "1d"]),
            (3 * 86_400 + 5, ["3д", "3д", "3d"]),
            (40 * 86_400, ["40д", "40д", "40d"]),
        ];
        for (ago, [uk, ru, en]) in cases {
            assert_eq!(rt(Language::Uk, ago), uk, "{ago}");
            assert_eq!(rt(Language::Ru, ago), ru, "{ago}");
            assert_eq!(rt(Language::En, ago), en, "{ago}");
        }
    }

    #[test]
    fn relative_time_clock_skew_is_now() {
        assert_eq!(relative_time(Language::En, 1_000, 5_000), "now");
        assert_eq!(relative_time(Language::Uk, 0, u64::MAX), "щойно");
    }

    #[test]
    fn agent_tooltip_appends_detail() {
        assert_eq!(agent_tooltip("Working", Some("Bash")), "Working: Bash");
        assert_eq!(agent_tooltip("Working", None), "Working");
        assert_eq!(agent_tooltip("Working", Some("  ")), "Working");
    }
}
