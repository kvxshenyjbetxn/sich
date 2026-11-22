// @sich-managed-omp-hook
import { spawn } from "node:child_process";
import { BroadcastChannel } from "node:worker_threads";

// Only the main TUI publishes to Sich. Child bindings can live on other Bun threads.
let owner;
const clip = (text) => typeof text === "string" ? text.slice(0, 2048).toWellFormed() : undefined;
const label = (text) => typeof text === "string" ? text.trim().slice(0, 120).toWellFormed() || undefined : undefined;
const MAX_SUBAGENTS = 16; // Same roster limit as the daemon's Claude/Codex integrations.
const MAX_ACTIVITY_BATCH = 32;
const MAX_ACTIVITY_QUEUE = 128;
const MAX_ACTIVITY_TEXT = 2048;
const LIFECYCLE = new Set(["started", "completed", "failed", "aborted"]);

// The native task widget also reads only text blocks from finalized results. Never
// include args, tool details, provider metadata, reasoning, or streamed updates.
function activityText(content) {
    if (!Array.isArray(content)) return;
    let text = "";
    for (const block of content) {
        if (block?.type !== "text" || typeof block.text !== "string" || !block.text) continue;
        if (text) text += "\n";
        text += block.text.slice(0, MAX_ACTIVITY_TEXT - text.length);
        if (text.length >= MAX_ACTIVITY_TEXT) break;
    }
    return text.toWellFormed().replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g, "").trim() || undefined;
}

const THINKING_LEVELS = new Set(["off", "minimal", "low", "medium", "high", "xhigh", "max"]);

// omp 19.x reports the subagent's model on its progress item. Prefer the resolved
// identity (provider/model); fall back to the selector minus a thinking-level suffix,
// then keep only the model name after the last "/".
export function progressModel(item) {
    let model = typeof item?.resolvedModelIdentity === "string" ? item.resolvedModelIdentity.trim() : "";
    if (!model && typeof item?.resolvedModel === "string") {
        model = item.resolvedModel.trim();
        const colon = model.lastIndexOf(":");
        if (colon >= 0 && THINKING_LEVELS.has(model.slice(colon + 1))) model = model.slice(0, colon);
    }
    return label(model.slice(model.lastIndexOf("/") + 1));
}

export default function sichStatus(pi) {
    const { SICH_OMP_HOOK: exe, SICH_SOCKET: socket, SICH_TERM_ID: term } = process.env;
    if (!exe || !socket || !term) return;

    let pending, draining, prompt, reply, detail, context, relay, reportedWait;
    let settled = "idle", finished, lastFinished = 0;
    let rootSessionId, lastRunSince = 0;
    let working = false;
    const approvals = new Map();
    const plans = new Map();
    const questions = new Set();
    const roster = new Map();
    const activity = [];
    // Keep ownership for idle agents too: their detached children can still report progress.
    const known = new Map();

    function channel() {
        if (!relay) {
            relay = new BroadcastChannel(`sich-omp-status:${process.pid}:${term}`);
            relay.unref();
        }
        return relay;
    }

    function record(id, run, kind, title, text) {
        if (activity.length === MAX_ACTIVITY_QUEUE) {
            // Keep starts and outcomes even when ordinary activity floods the bridge.
            const ordinary = activity.findIndex((entry) => !LIFECYCLE.has(entry.kind));
            if (ordinary < 0 && !LIFECYCLE.has(kind)) return;
            activity.splice(Math.max(0, ordinary), 1);
        }
        activity.push({ id, since_unix_ms: run.since, kind, title, text });
    }

    function receive(frame, sender) {
        if (!context || !frame?.id) return;
        if (sender) {
            if (!known.has(sender.id)) return;
            const file = known.get(sender.id);
            if (file && sender.file && file !== sender.file) return;
        }
        const { id, status, file, call } = frame;
        const run = roster.get(id);
        if (frame.kind === "wait") {
            if (!sender || sender.id !== id || !run || run.waiting === frame.detail) return;
            run.waiting = frame.detail;
            if (frame.detail) record(id, run, "waiting", frame.detail);
        } else if (frame.kind === "activity") {
            // Native events have only an id, no call or file; the spawning bus
            // and its ordering with lifecycle frames identify the active run.
            if (!run || run.parent !== sender?.id) return;
            const modelChanged = frame.model !== undefined && frame.model !== run.model;
            if (modelChanged) run.model = frame.model;
            if (!frame.activityKind && !modelChanged) return;
            if (frame.activityKind) record(id, run, frame.activityKind, frame.title, frame.text);
            if (frame.activityKind === "tool_started" && frame.title === "wait") {
                record(id, run, "waiting", frame.title);
            }
        } else {
            if (run && run.parent !== sender?.id) return;
            const sameRun = run && (call && run.call ? call === run.call : !file || !run.file || file === run.file);
            if (status === "started") {
                if (sameRun) return;
                roster.delete(id);
                const started = {
                    parent: sender?.id, file, call, type: frame.agent, description: frame.description,
                    since: lastRunSince = Math.max(Date.now(), lastRunSince + 1), tool: undefined, waiting: undefined,
                };
                roster.set(id, started);
                record(id, started, "started", started.description ?? started.type);
                known.set(id, file);
            } else if (!sameRun) {
                return; // Progress and settle frames cannot resurrect an old run.
            } else if (frame.kind === "progress") {
                const description = frame.description ?? run.description;
                // Progress clears currentTool between calls; the last action is still useful.
                const tool = frame.detail ?? run.tool;
                const model = frame.model ?? run.model;
                if (description === run.description && tool === run.tool && model === run.model) return;
                run.description = description;
                run.tool = tool;
                run.model = model;
            } else {
                record(id, run, status);
                roster.delete(id);
            }
        }
        publish(context);
    }

    function observe(frame) {
        if (!context || typeof frame.id !== "string" || !frame.id) return;
        const { agent, sessionManager } = context;
        if (agent.kind === "main") {
            receive(frame);
        } else {
            const sender = { id: agent.id, file: sessionManager.getSessionFile() };
            channel().postMessage({ sender, frame });
        }
    }

    pi.events.on("task:subagent:lifecycle", (event) => {
        if (!LIFECYCLE.has(event?.status)) return;
        observe({
            kind: "lifecycle", id: event.id, status: event.status, agent: label(event.agent),
            description: label(event.description), file: event.sessionFile, call: event.parentToolCallId,
        });
    });
    pi.events.on("task:subagent:progress", (event) => {
        const item = event?.progress;
        if (!item) return;
        observe({
            kind: "progress", id: item.id, description: label(item.description), detail: label(item.currentTool),
            model: progressModel(item), file: event.sessionFile, call: event.parentToolCallId,
        });
    });
    pi.events.on("task:subagent:event", (payload) => {
        const { id, event } = payload ?? {};
        if (!context || typeof id !== "string" || !id) return;
        let activityKind, title, text, model;
        switch (event?.type) {
            case "tool_execution_start":
                activityKind = "tool_started";
                title = label(event.toolName);
                break;
            case "tool_execution_end":
                activityKind = event.isError === true ? "tool_failed" : "tool_finished";
                title = label(event.toolName);
                // Unlike ordinary widget output, ask's result can be direct user input.
                if (event.toolName !== "ask") text = activityText(event.result?.content);
                break;
            case "message_start":
            case "message_end": {
                const message = event.message;
                if (message?.role !== "assistant") return;
                if (message.stopReason !== "error" && message.stopReason !== "aborted") model = label(message.model);
                if (event.type === "message_end") text = activityText(message.content);
                if (text) activityKind = "message";
                if (!activityKind && !model) return;
                break;
            }
            case "agent_pause":
                if (event.awaiting !== true) return;
                activityKind = "waiting";
                break;
            default:
                // The task lifecycle status, not the model loop's end, settles a run.
                return;
        }
        observe({ kind: "activity", id, activityKind, title, text, model });
    });

    function waitingOn() {
        return plans.values().next().value ?? (questions.size ? "ask" : approvals.values().next().value);
    }

    function reportWait(ctx) {
        if (ctx.agent.kind === "main") {
            publish(ctx);
        } else {
            const detail = waitingOn();
            if (reportedWait === detail) return;
            reportedWait = detail;
            observe({ kind: "wait", id: ctx.agent.id, detail });
        }
    }

    function clearWaits() {
        approvals.clear();
        plans.clear();
        questions.clear();
    }

    // One process in flight; the roster coalesces independently of the activity FIFO.
    // No event waits on IPC except shutdown.
    async function drain() {
        while (pending !== undefined) {
            const batch = activity.length ? activity.splice(0, MAX_ACTIVITY_BATCH) : undefined;
            const body = JSON.stringify({ ...pending, activity: batch });
            if (!activity.length) pending = undefined;
            await new Promise((resolve) => {
                try {
                    const child = spawn(exe, ["hook", "omp"], {
                        stdio: ["pipe", "ignore", "ignore"], windowsHide: true, timeout: 900,
                    });
                    child.once("close", resolve);
                    child.on("error", () => {});
                    child.stdin.on("error", () => {});
                    child.stdin.end(body);
                } catch {
                    resolve();
                }
            });
        }
        draining = undefined;
    }

    function publish(ctx, state) {
        const waiting = waitingOn();
        const subagents = [];
        for (const [id, run] of roster) {
            subagents.push({
                id, agent_type: run.type, description: run.description ?? id,
                state: run.waiting ? "waiting" : "working", detail: run.waiting ?? run.tool,
                since_unix_ms: run.since,
                model: run.model,
            });
            if (subagents.length === MAX_SUBAGENTS) break;
        }
        pending = {
            session_id: rootSessionId,
            state: state ?? (waiting ? "waiting" : working ? "working" : settled),
            model: clip(ctx.model?.id), title: clip(pi.getSessionName()), prompt,
            detail: waiting ?? detail, message: reply?.text,
            finished_unix_ms: finished, subagents: subagents.length ? subagents : undefined,
        };
        draining ??= drain();
    }

    function on(name, handler, includeChildren = false) {
        pi.on(name, (event, ctx) => {
            if (ctx.agent.kind === "main") {
                if (ctx.mode !== "tui") return;
                if (name === "session_start") owner ??= ctx.sessionManager;
                if (owner !== ctx.sessionManager) return;
            } else if (!includeChildren) {
                return;
            }
            // Observers must never interrupt agent execution, especially approval handlers.
            try {
                context = ctx;
                return handler(event, ctx);
            } catch { /* fail soft */ }
        });
    }

    function reset(_event, ctx) {
        settled = "idle";
        working = false;
        finished = prompt = reply = detail = undefined;
        clearWaits();
        roster.clear();
        known.clear();
        // In-flight snapshots retain their captured session id; never carry old
        // pending activity into a snapshot of a replacement session.
        activity.length = 0;
        rootSessionId = ctx.sessionManager.getSessionId();
        publish(ctx);
    }

    on("session_start", (event, ctx) => {
        if (ctx.agent.kind !== "main") return;
        channel().onmessage = ({ data }) => {
            if (typeof data?.sender?.id !== "string") return;
            try { receive(data.frame, data.sender); } catch { /* fail soft */ }
        };
        reset(event, ctx);
    }, true);
    on("session_switch", reset);
    on("session_branch", reset);
    on("session_clear", reset);
    on("before_agent_start", (event, ctx) => {
        if (ctx.agent.kind !== "main" || event.reason === "revive") return;
        working = true;
        prompt = clip(event.prompt);
        finished = reply = detail = undefined;
        publish(ctx);
    }, true);
    on("agent_start", (_event, ctx) => {
        if (ctx.agent.kind !== "main" || working) return;
        working = true;
        finished = reply = detail = undefined;
        publish(ctx);
    }, true);
    on("tool_execution_start", (event, ctx) => {
        if (ctx.agent.kind === "main") {
            if (!working) return;
            detail = clip(event.toolName);
        } else if (event.toolName !== "ask") {
            return;
        }
        if (event.toolName === "ask") questions.add(event.toolCallId);
        reportWait(ctx);
    }, true);
    on("tool_execution_end", (event, ctx) => {
        if (!questions.delete(event.toolCallId)) return;
        if (detail === "ask") detail = undefined;
        reportWait(ctx);
    }, true);
    on("tool_approval_requested", (event, ctx) => {
        approvals.set(event.toolCallId, label(event.toolName) ?? "Approval");
        reportWait(ctx);
    }, true);
    on("tool_approval_resolved", (event, ctx) => {
        if (approvals.delete(event.toolCallId)) reportWait(ctx);
    }, true);
    on("plan_review_requested", (event, ctx) => {
        plans.set(event.reviewId, clip(event.title) || "Plan review");
        publish(ctx);
    });
    on("plan_review_resolved", (event, ctx) => {
        if (plans.delete(event.reviewId)) publish(ctx);
    });
    on("message_end", ({ message }, _ctx) => {
        if (!working || message.role !== "assistant") return;
        let text = "";
        if (message.stopReason === "error") {
            text = clip(message.errorMessage);
        } else {
            for (const block of message.content) {
                if (block.type !== "text" || !block.text) continue;
                if (text) text += " ";
                text += block.text.slice(0, 2048 - text.length);
                if (text.length >= 2048) break;
            }
        }
        reply = { stop: message.stopReason, text: clip(text) };
    });
    on("agent_end", (event, ctx) => {
        if (event.willContinue) return;
        if (ctx.agent.kind !== "main") {
            clearWaits();
            reportWait(ctx);
            return;
        }
        if (!working) return;
        working = false;
        detail = undefined;
        clearWaits();
        settled = reply?.stop === "aborted" ? "idle" : reply?.stop === "error" ? "failed" : "done";
        if (settled !== "idle") finished = lastFinished = Math.max(Date.now(), lastFinished + 1);
        publish(ctx);
    }, true);
    on("session_shutdown", async (_event, ctx) => {
        const main = ctx.agent.kind === "main";
        if (!main) {
            clearWaits();
            reportWait(ctx);
        }
        context = undefined;
        relay?.close();
        relay = undefined;
        if (main) {
            // Deliver queued activity while the old session is still live in
            // Sich, then close it with a single final snapshot.
            await draining;
            roster.clear();
            known.clear();
            publish(ctx, "exited");
            owner = undefined;
        }
        return draining;
    }, true);
}
