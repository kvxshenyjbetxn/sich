import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import extension from "../src/omp_status.js";

function bind() {
    const handlers = new Map();
    const bus = new Map();
    extension({
        on: (name, cb) => handlers.set(name, cb),
        events: { on: (name, cb) => bus.set(name, cb) },
        getSessionName: () => "Work session",
    });
    return { handlers, bus };
}

// Uses the real child-process bridge with a disposable receiver instead of the daemon.
test("omp extension tracks root and nested runs, preserves waits, and drains shutdown", async (t) => {
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
    assert.equal(bind().handlers.size, 0, "registers no handlers outside a Sich terminal");
    await writeFile("hook", String.raw`
        if (process.argv[2] !== "omp") process.exit(2);
        let body = "";
        process.stdin.setEncoding("utf8");
        process.stdin.on("data", (chunk) => { body += chunk; });
        process.stdin.on("end", () => require("node:fs").appendFileSync("events", body + "\n"));
    `);
    Object.assign(process.env, { SICH_TERM_ID: "1", SICH_SOCKET: "fixture", SICH_OMP_HOOK: process.execPath });
    const { handlers, bus } = bind();
    const childExtension = bind();
    let id = "first";
    const ctx = { mode: "tui", agent: { kind: "main" }, model: { id: "model-1" }, sessionManager: { getSessionId: () => id } };
    const child = {
        ...ctx, mode: "print", agent: { kind: "sub", id: "Scout", name: "scout" },
        sessionManager: { getSessionId: () => "child", getSessionFile: () => "first/Scout.jsonl" },
    };
    const emit = (name, event = {}, context = ctx) => handlers.get(name)(event, context);
    const emitChild = (name, event = {}) => childExtension.handlers.get(name)(event, child);
    const lifecycle = (source, payload) => source.get("task:subagent:lifecycle")(payload);
    const progress = (source, payload) => source.get("task:subagent:progress")(payload);
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

    emit("session_start", {}, { ...ctx, mode: "print" });
    emit("session_start");
    assert.deepEqual(await next("idle"), { session_id: "first", state: "idle", model: "model-1", title: "Work session" });
    emitChild("session_start");
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

    const scout = { id: "Scout", status: "started", agent: "scout", sessionFile: "first/Scout.jsonl", parentToolCallId: "call-1" };
    lifecycle(bus, scout);
    const withScout = await next("done");
    assert.equal(withScout.subagents[0].description, "Scout");
    assert.equal(withScout.subagents[0].state, "working");

    const toolProgress = { id: "Scout", description: "Check parser", currentTool: "read", currentToolArgs: "private" };
    progress(bus, { ...scout, progress: toolProgress });
    const withTool = await next("done");
    assert.equal(withTool.finished_unix_ms, withScout.finished_unix_ms);
    assert.equal(withTool.subagents[0].detail, "read");
    assert.equal(withTool.subagents[0].description, "Check parser");
    assert.equal(withTool.subagents[0].since_unix_ms, withScout.subagents[0].since_unix_ms);
    assert.ok(!JSON.stringify(withTool).includes("private"));

    emitChild("tool_approval_requested", { toolCallId: "permission", toolName: "write" });
    const withWaiting = await next("done");
    assert.equal(withWaiting.subagents[0].state, "waiting");
    assert.equal(withWaiting.subagents[0].detail, "write");

    const nested = { id: "Scout.Deep", status: "started", agent: "scout", parentToolCallId: "call-2" };
    lifecycle(childExtension.bus, nested);
    const withNested = await next("done");
    assert.deepEqual(withNested.subagents.map(({ id, state }) => [id, state]), [["Scout", "waiting"], ["Scout.Deep", "working"]]);
    emitChild("tool_approval_resolved", { toolCallId: "permission" });
    assert.equal((await next("done")).subagents[0].detail, "read");
    lifecycle(bus, { ...scout, status: "completed" });
    assert.deepEqual((await next("done")).subagents.map(({ id }) => id), ["Scout.Deep"]);
    lifecycle(childExtension.bus, { ...nested, status: "aborted" });
    assert.equal((await next("done")).subagents, undefined);
    lifecycle(bus, { ...scout, status: "started", parentToolCallId: "follow-up" });
    assert.equal((await next("done")).subagents[0].id, "Scout");

    emit("before_agent_start", { reason: "prompt", prompt: "Next" });
    await next("working");
    emit("message_end", { message: { role: "assistant", stopReason: "aborted", content: [] } });
    emit("agent_end");
    assert.equal((await next("idle")).prompt, "Next");
    id = "second";
    emit("session_switch");
    const switched = await next("idle");
    assert.equal(switched.prompt, undefined);
    assert.equal(switched.subagents, undefined);
    lifecycle(childExtension.bus, nested); // Old cascading event must not enter the new roster.
    await emitChild("session_shutdown");

    const batch = Array.from({ length: 17 }, (_, index) => ({ id: `Worker${index}`, status: "started", agent: "scout" }));
    for (const item of batch) lifecycle(bus, item);
    assert.deepEqual((await next("idle")).subagents.map(({ id }) => id), ["Worker0"]);
    assert.deepEqual((await next("idle")).subagents.map(({ id }) => id), batch.slice(0, 16).map(({ id }) => id));
    lifecycle(bus, { ...batch[0], status: "failed" });
    assert.deepEqual((await next("idle")).subagents.map(({ id }) => id), batch.slice(1).map(({ id }) => id));
    await emit("session_shutdown");
    assert.equal((await events()).at(-1).state, "exited", "shutdown waits for delivery");
    assert.equal((await next("exited")).session_id, "second");
});
