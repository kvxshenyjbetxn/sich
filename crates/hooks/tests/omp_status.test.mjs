import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import extension from "../src/omp_status.js";

function bind() {
    const handlers = new Map();
    extension({ on: (name, cb) => handlers.set(name, cb), getSessionName: () => "Work session" });
    return handlers;
}

// Uses the real child-process bridge with a disposable receiver instead of the daemon.
test("omp extension observes the main TUI, tracks overlapping waits, and drains shutdown", async (t) => {
    const dir = await mkdtemp(join(tmpdir(), "sich-omp-hook-"));
    const cwd = process.cwd();
    const env = { ...process.env };
    t.after(async () => {
        process.chdir(cwd);
        process.env = env;
        await rm(dir, { recursive: true, force: true });
    });
    process.chdir(dir);
    delete process.env.SICH_TERM_ID;
    assert.equal(bind().size, 0, "registers no handlers outside a Sich terminal");
    await writeFile("hook", String.raw`
        if (process.argv[2] !== "omp") process.exit(2);
        let body = "";
        process.stdin.setEncoding("utf8");
        process.stdin.on("data", (chunk) => { body += chunk; });
        process.stdin.on("end", () => require("node:fs").appendFileSync("events", body + "\n"));
    `);
    Object.assign(process.env, { SICH_TERM_ID: "1", SICH_SOCKET: "fixture", SICH_OMP_HOOK: process.execPath });
    const handlers = bind();
    let id = "first";
    const ctx = { mode: "tui", model: { id: "model-1" }, sessionManager: { getSessionId: () => id } };
    const child = { ...ctx, sessionManager: { getSessionId: () => "child" } };
    const emit = (name, event = {}, context = ctx) => handlers.get(name)(event, context);
    let seen = 0;
    const events = async () => {
        let text;
        try { text = await readFile("events", "utf8"); } catch (e) {
            if (e.code !== "ENOENT") throw e;
            return [];
        }
        return text.split("\n").slice(0, -1).map(JSON.parse);
    };
    async function next(state) {
        const until = Date.now() + 5000;
        do {
            const rows = await events();
            if (rows.length > seen) {
                const row = rows[seen++];
                assert.equal(row.state, state);
                return row;
            }
            await delay(15);
        } while (Date.now() < until);
        assert.fail(`missing ${state} snapshot`);
    }

    emit("session_start", {}, { ...child, mode: "print" });
    emit("session_start");
    assert.deepEqual(await next("idle"), { session_id: "first", state: "idle", model: "model-1", title: "Work session" });
    emit("session_start", {}, child);
    await emit("session_shutdown", {}, child);
    emit("before_agent_start", { reason: "prompt", prompt: "User prompt" });
    assert.equal((await next("working")).prompt, "User prompt");
    emit("tool_approval_requested", { toolCallId: "1", toolName: "shell" });
    assert.equal((await next("waiting")).detail, "shell");
    emit("tool_approval_requested", { toolCallId: "2", toolName: "write" });
    assert.equal((await next("waiting")).detail, "shell");
    emit("tool_approval_resolved", { toolCallId: "1" });
    assert.equal((await next("waiting")).detail, "write");
    emit("tool_approval_resolved", { toolCallId: "2" });
    await next("working");
    emit("tool_execution_start", { toolCallId: "question", toolName: "ask" });
    assert.equal((await next("waiting")).detail, "ask");
    emit("tool_execution_end", { toolCallId: "other", toolName: "read" });
    emit("tool_execution_end", { toolCallId: "question", toolName: "ask" });
    assert.equal((await next("working")).detail, undefined);
    emit("message_end", { message: { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "Answer" }] } });
    emit("agent_end", { willContinue: true });
    emit("agent_end");
    assert.equal((await next("done")).message, "Answer");

    emit("before_agent_start", { reason: "prompt", prompt: "Next" });
    await next("working");
    emit("message_end", { message: { role: "assistant", stopReason: "aborted", content: [] } });
    emit("agent_end");
    assert.equal((await next("idle")).prompt, "Next");
    id = "second";
    emit("session_switch");
    assert.equal((await next("idle")).prompt, undefined);
    await emit("session_shutdown");
    assert.equal((await events()).at(-1).state, "exited", "shutdown waits for delivery");
    assert.equal((await next("exited")).session_id, "second");
});
