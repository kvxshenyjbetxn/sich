// @sich-managed-omp-hook
import { spawn } from "node:child_process";
import { BroadcastChannel } from "node:worker_threads";

// Only the main TUI publishes to Sich. Child bindings can live on other Bun threads.
let owner;
const clip = (text) => typeof text === "string" ? text.slice(0, 2048).toWellFormed() : undefined;
const label = (text) => typeof text === "string" ? text.trim().slice(0, 120).toWellFormed() || undefined : undefined;
const MAX_SUBAGENTS = 16; // Same roster limit as the daemon's Claude/Codex integrations.

export default function sichStatus(pi) {
    const { SICH_OMP_HOOK: exe, SICH_SOCKET: socket, SICH_TERM_ID: term } = process.env;
    if (!exe || !socket || !term) return;

    let pending, draining, prompt, reply, detail, context, relay, reportedWait;
    let settled = "idle", finished, lastFinished = 0;
    let working = false;
    const approvals = new Map();
    const plans = new Map();
    const questions = new Set();
    const roster = new Map();
    // Keep ownership for idle agents too: their detached children can still report progress.
    const known = new Map();

    function channel() {
        if (!relay) {
            relay = new BroadcastChannel(`sich-omp-status:${process.pid}:${term}`);
            relay.unref();
        }
        return relay;
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
        } else {
            if (run && run.parent !== sender?.id) return;
            const sameRun = run && (call && run.call ? call === run.call : !file || !run.file || file === run.file);
            if (status === "started") {
                if (sameRun) return;
                roster.delete(id);
                roster.set(id, {
                    parent: sender?.id, file, call, type: frame.agent, description: frame.description,
                    since: Date.now(), tool: undefined, waiting: undefined,
                });
                known.set(id, file);
            } else if (!sameRun) {
                return; // Progress and settle frames cannot resurrect an old run.
            } else if (frame.kind === "progress") {
                const description = frame.description ?? run.description;
                if (description === run.description && frame.detail === run.tool) return;
                run.description = description;
                run.tool = frame.detail;
            } else {
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
        if (!["started", "completed", "failed", "aborted"].includes(event?.status)) return;
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
            file: event.sessionFile, call: event.parentToolCallId,
        });
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

    // One process in flight, one latest snapshot waiting. No event waits on IPC except
    // shutdown, and intermediate updates can be dropped without losing session metadata.
    async function drain() {
        while (pending !== undefined) {
            const body = pending;
            pending = undefined;
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
            });
            if (subagents.length === MAX_SUBAGENTS) break;
        }
        pending = JSON.stringify({
            session_id: ctx.sessionManager.getSessionId(),
            state: state ?? (waiting ? "waiting" : working ? "working" : settled),
            model: clip(ctx.model?.id), title: clip(pi.getSessionName()), prompt,
            detail: waiting ?? detail, message: reply?.text,
            finished_unix_ms: finished, subagents: subagents.length ? subagents : undefined,
        });
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
    on("session_shutdown", (_event, ctx) => {
        if (ctx.agent.kind === "main") {
            roster.clear();
            known.clear();
            publish(ctx, "exited");
            owner = undefined;
        } else {
            clearWaits();
            reportWait(ctx);
        }
        context = undefined;
        relay?.close();
        relay = undefined;
        return draining;
    }, true);
}
