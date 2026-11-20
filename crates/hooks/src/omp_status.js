// @sich-managed-omp-hook
import { spawn } from "node:child_process";

// OMP also binds this factory in child sessions. Only the main TUI owns the terminal.
let owner;
const clip = (text) => typeof text === "string" ? text.slice(0, 2048).toWellFormed() : undefined;

export default function sichStatus(pi) {
    const { SICH_OMP_HOOK: exe, SICH_SOCKET: socket, SICH_TERM_ID: term } = process.env;
    if (!exe || !socket || !term) return;

    let pending, draining, prompt, reply, detail;
    let working = false;
    const approvals = new Map();
    const plans = new Map();
    const questions = new Set();

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
        const waiting = plans.values().next().value ??
            (questions.size ? "ask" : approvals.values().next().value);
        pending = JSON.stringify({
            session_id: ctx.sessionManager.getSessionId(),
            state: state ?? (waiting ? "waiting" : working ? "working" : "idle"),
            model: clip(ctx.model?.id), title: clip(pi.getSessionName()), prompt,
            detail: waiting ?? detail, message: reply?.text,
        });
        draining ??= drain();
    }

    function on(name, handler) {
        pi.on(name, (event, ctx) => {
            if (ctx.mode !== "tui") return;
            if (name === "session_start") owner ??= ctx.sessionManager;
            if (owner !== ctx.sessionManager) return;
            // Observers must never interrupt agent execution, especially approval handlers.
            try { return handler(event, ctx); } catch { /* fail soft */ }
        });
    }

    function reset(_event, ctx) {
        working = false;
        prompt = reply = detail = undefined;
        approvals.clear();
        plans.clear();
        questions.clear();
        publish(ctx, "idle");
    }

    on("session_start", reset);
    on("session_switch", reset);
    on("session_branch", reset);
    on("session_clear", reset);
    on("before_agent_start", (event, ctx) => {
        if (event.reason === "revive") return;
        working = true;
        prompt = clip(event.prompt);
        reply = detail = undefined;
        publish(ctx);
    });
    on("agent_start", (_event, ctx) => {
        if (working) return;
        working = true;
        reply = detail = undefined;
        publish(ctx);
    });
    on("tool_execution_start", (event, ctx) => {
        if (!working) return;
        detail = clip(event.toolName);
        if (event.toolName === "ask") questions.add(event.toolCallId);
        publish(ctx);
    });
    on("tool_execution_end", (event, ctx) => {
        if (!questions.delete(event.toolCallId)) return;
        if (detail === "ask") detail = undefined;
        publish(ctx);
    });
    on("tool_approval_requested", (event, ctx) => {
        approvals.set(event.toolCallId, clip(event.toolName));
        publish(ctx);
    });
    on("tool_approval_resolved", (event, ctx) => {
        if (approvals.delete(event.toolCallId)) publish(ctx);
    });
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
        if (!working || event.willContinue) return;
        working = false;
        detail = undefined;
        approvals.clear();
        plans.clear();
        questions.clear();
        publish(ctx, reply?.stop === "aborted" ? "idle" : reply?.stop === "error" ? "failed" : "done");
    });
    on("session_shutdown", (_event, ctx) => {
        publish(ctx, "exited");
        return draining;
    });
}
