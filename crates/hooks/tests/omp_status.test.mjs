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
    const native = (source, id, event) => source.get("task:subagent:event")({ id, event });
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
                if (state) assert.equal(row.state, state);
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
    assert.equal(withScout.subagents[0].model, undefined, "never copies the parent's model");
    const since = withScout.subagents[0].since_unix_ms;
    const scoutEvent = (kind, fields = {}) => ({ id: "Scout", since_unix_ms: since, kind, ...fields });
    assert.deepEqual(withScout.activity, [scoutEvent("started", { title: "scout" })]);

    native(bus, "Scout", {
        type: "message_start", message: { role: "assistant", model: "scout-model", content: [] },
    });
    const withModel = await next("done");
    assert.equal(withModel.model, "model-1");
    assert.equal(withModel.subagents[0].model, "scout-model");
    assert.equal(withModel.activity, undefined, "a model-only update is not an activity entry");
    native(bus, "Scout", {
        type: "message_end", message: { role: "assistant", model: "fallback-model", content: [] },
    });
    assert.equal((await next("done")).subagents[0].model, "fallback-model");
    native(bus, "Scout", {
        type: "message_end", message: { role: "assistant", model: "PRIVATE_FAILED_MODEL", stopReason: "error", content: [] },
    });

    const toolProgress = { id: "Scout", description: "Check parser", currentTool: "read", currentToolArgs: "private" };
    progress(bus, { ...scout, progress: toolProgress });
    const withTool = await next("done");
    assert.equal(withTool.finished_unix_ms, withScout.finished_unix_ms);
    assert.equal(withTool.subagents[0].detail, "read");
    assert.equal(withTool.subagents[0].description, "Check parser");
    assert.equal(withTool.subagents[0].since_unix_ms, withScout.subagents[0].since_unix_ms);
    assert.equal(withTool.subagents[0].model, "fallback-model");
    assert.ok(!JSON.stringify(withTool).includes("private"));
    progress(bus, { ...scout, progress: { id: "Scout", description: "Check parser output" } });
    const betweenCalls = await next("done");
    assert.equal(betweenCalls.subagents[0].description, "Check parser output");
    assert.equal(betweenCalls.subagents[0].detail, "read", "the last action remains visible between calls");

    emitChild("tool_approval_requested", { toolCallId: "permission", toolName: "write" });
    const withWaiting = await next("done");
    assert.equal(withWaiting.subagents[0].state, "waiting");
    assert.equal(withWaiting.subagents[0].detail, "write");
    assert.deepEqual(withWaiting.activity, [scoutEvent("waiting", { title: "write" })]);

    native(bus, "Scout", {
        type: "tool_execution_start", toolCallId: "read-1", toolName: "read",
        args: { credential: "PRIVATE_ARGUMENT" }, intent: "PRIVATE_INTENT",
    });
    native(bus, "Scout", {
        type: "message_update", message: { role: "assistant", model: "PRIVATE_DELTA_MODEL", content: [{ type: "text", text: "PRIVATE_DELTA" }] },
    });
    native(bus, "Scout", {
        type: "tool_execution_update", partialResult: { content: [{ type: "text", text: "PRIVATE_PARTIAL" }] },
    });
    native(bus, "Scout", {
        type: "message_end", message: { role: "user", model: "PRIVATE_USER_MODEL", content: [{ type: "text", text: "PRIVATE_PROMPT" }] },
    });
    native(bus, "Scout", {
        type: "message_end", message: {
            role: "assistant",
            content: [
                { type: "thinking", thinking: "PRIVATE_REASONING", text: "PRIVATE_REASONING" },
                { type: "text", text: "Found a parser issue", textSignature: "PRIVATE_SIGNATURE" },
                { type: "toolCall", name: "read", arguments: { key: "PRIVATE_TOOL_CALL" } },
                { type: "text", text: "Checking the edge case" },
            ],
        },
    });
    native(bus, "Scout", {
        type: "tool_execution_end", toolName: "read", toolCallId: "read-1", isError: false,
        result: {
            content: [{ type: "image", data: "PRIVATE_IMAGE" }, { type: "text", text: "x".repeat(2048) + "PRIVATE_OVERFLOW" }],
            details: { token: "PRIVATE_METADATA" },
        },
    });
    native(bus, "Scout", {
        type: "tool_execution_end", toolName: "edit", toolCallId: "edit-1", isError: true,
        result: { content: [{ type: "text", text: "Context did not match" }] },
    });
    native(bus, "Scout", {
        type: "tool_execution_end", toolName: "ask", toolCallId: "ask-1",
        result: { content: [{ type: "text", text: "PRIVATE_ASK_ANSWER" }] },
    });
    native(bus, "Scout", { type: "tool_execution_start", toolName: "wait", toolCallId: "wait-1" });
    native(bus, "Scout", { type: "agent_pause", awaiting: false, messages: ["PRIVATE_CONTINUATION"] });
    native(bus, "Scout", { type: "agent_pause", awaiting: true, messages: ["PRIVATE_PAUSE_CONTEXT"] });
    native(bus, "Scout", { type: "agent_end", messages: ["PRIVATE_END_CONTEXT"] });
    const nativeActivity = [];
    let toolResults;
    while (nativeActivity.length < 8) {
        toolResults = await next("done");
        nativeActivity.push(...(toolResults.activity ?? []));
    }
    assert.deepEqual(nativeActivity, [
        scoutEvent("tool_started", { title: "read" }),
        scoutEvent("message", { text: "Found a parser issue\nChecking the edge case" }),
        scoutEvent("tool_finished", { title: "read", text: "x".repeat(2048) }),
        scoutEvent("tool_failed", { title: "edit", text: "Context did not match" }),
        scoutEvent("tool_finished", { title: "ask" }),
        scoutEvent("tool_started", { title: "wait" }),
        scoutEvent("waiting", { title: "wait" }),
        scoutEvent("waiting"),
    ]);
    assert.equal(toolResults.subagents[0].state, "waiting", "native events do not replace the roster's approval state");
    assert.equal(toolResults.subagents[0].model, "fallback-model", "missing model fields preserve the last report");
    assert.ok(!JSON.stringify(await events()).includes("PRIVATE_"));

    const nested = { id: "Scout.Deep", status: "started", agent: "scout", parentToolCallId: "call-2" };
    lifecycle(childExtension.bus, nested);
    const withNested = await next("done");
    assert.deepEqual(withNested.subagents.map(({ id, state }) => [id, state]), [["Scout", "waiting"], ["Scout.Deep", "working"]]);
    const nestedSince = withNested.subagents[1].since_unix_ms;
    native(bus, nested.id, {
        type: "message_end", message: { role: "assistant", model: "PRIVATE_WRONG_MODEL", content: [{ type: "text", text: "PRIVATE_WRONG_PARENT" }] },
    });
    native(childExtension.bus, nested.id, {
        type: "message_end", message: { role: "assistant", model: "nested-model", content: [{ type: "text", text: "Nested result" }] },
    });
    const nestedResult = await next("done");
    assert.deepEqual(nestedResult.activity, [
        { id: nested.id, since_unix_ms: nestedSince, kind: "message", text: "Nested result" },
    ]);
    assert.deepEqual(nestedResult.subagents.map(({ model }) => model), ["fallback-model", "nested-model"]);
    emitChild("tool_approval_resolved", { toolCallId: "permission" });
    assert.equal((await next("done")).subagents[0].detail, "read");
    lifecycle(bus, { ...scout, status: "completed" });
    const completed = await next("done");
    assert.deepEqual(completed.subagents.map(({ id }) => id), ["Scout.Deep"]);
    assert.deepEqual(completed.activity, [scoutEvent("completed")]);
    lifecycle(childExtension.bus, { ...nested, status: "aborted" });
    const aborted = await next("done");
    assert.equal(aborted.subagents, undefined);
    assert.deepEqual(aborted.activity, [{ id: nested.id, since_unix_ms: nestedSince, kind: "aborted" }]);
    native(bus, "Scout", {
        type: "message_end", message: { role: "assistant", model: "PRIVATE_SETTLED_MODEL", content: [] },
    });
    lifecycle(bus, { ...scout, status: "started", parentToolCallId: "follow-up" });
    const followup = await next("done");
    assert.equal(followup.subagents[0].id, "Scout");
    assert.ok(followup.subagents[0].since_unix_ms > since);
    assert.equal(followup.activity[0].since_unix_ms, followup.subagents[0].since_unix_ms);
    assert.equal(followup.subagents[0].model, undefined, "a reused id starts with no confirmed model");

    emit("before_agent_start", { reason: "prompt", prompt: "Next" });
    await next("working");
    emit("message_end", { message: { role: "assistant", stopReason: "aborted", content: [] } });
    emit("agent_end");
    assert.equal((await next("idle")).prompt, "Next");
    emit("before_agent_start", { reason: "prompt", prompt: "Switch soon" });
    native(bus, "Scout", {
        type: "message_end", message: { role: "assistant", content: [{ type: "text", text: "PRIVATE_QUEUED_OLD_SESSION" }] },
    });
    id = "second";
    emit("session_switch");
    let switched = await next();
    if (switched.state === "working") {
        assert.equal(switched.session_id, "first");
        switched = await next("idle");
    }
    assert.equal(switched.state, "idle");
    assert.equal(switched.session_id, "second");
    assert.equal(switched.prompt, undefined);
    assert.equal(switched.subagents, undefined);
    assert.equal(switched.activity, undefined);
    lifecycle(childExtension.bus, nested); // Old cascading event must not enter the new roster.
    native(childExtension.bus, nested.id, { type: "tool_execution_start", toolName: "PRIVATE_STALE_NESTED" });
    await emitChild("session_shutdown");

    const batch = Array.from({ length: 17 }, (_, index) => ({ id: `Worker${index}`, status: "started", agent: "scout" }));
    for (const item of batch) lifecycle(bus, item);
    // The previous child-process write can still be in flight after its output is visible.
    const firstRoster = await next("idle");
    const roster = firstRoster.subagents.length === 1 ? await next("idle") : firstRoster;
    assert.deepEqual(roster.subagents.map(({ id }) => id), batch.slice(0, 16).map(({ id }) => id));
    const starts = firstRoster === roster ? roster.activity : [...firstRoster.activity, ...roster.activity];
    assert.deepEqual(starts.filter(({ kind }) => kind === "started").map(({ id }) => id), batch.map(({ id }) => id));
    lifecycle(bus, { ...batch[0], status: "failed" });
    const failed = await next("idle");
    assert.deepEqual(failed.subagents.map(({ id }) => id), batch.slice(1).map(({ id }) => id));
    assert.equal(failed.activity[0].kind, "failed");

    // The first event occupies the bridge; the remaining roster updates coalesce.
    // Lifecycle edges for runs that start and finish in that window must survive a burst.
    const quick = { id: "Quick", status: "started", parentToolCallId: "same-call" };
    const beforeBurst = seen;
    emit("before_agent_start", { reason: "prompt", prompt: "Busy bridge" });
    lifecycle(bus, quick);
    lifecycle(bus, { ...quick, status: "completed" });
    lifecycle(bus, quick);
    for (let index = 0; index < 200; index++) {
        native(bus, "Quick", { type: "tool_execution_start", toolName: "read", toolCallId: String(index) });
    }
    lifecycle(bus, { ...quick, status: "failed" });
    await emit("session_shutdown");
    const rows = await events();
    const burst = rows.slice(beforeBurst, -1).filter((row) => row.activity);
    assert.ok(burst.every(({ state, subagents, activity }) => state === "working" &&
        subagents.every(({ id }) => id !== "Quick") && activity.length <= 32));
    const activity = burst.flatMap((row) => row.activity);
    assert.equal(activity.length, 128);
    const edges = activity.filter(({ kind }) => kind !== "tool_started");
    assert.deepEqual(edges.map(({ id, kind }) => [id, kind]), [
        ["Quick", "started"], ["Quick", "completed"], ["Quick", "started"], ["Quick", "failed"],
    ]);
    assert.equal(edges[0].since_unix_ms, edges[1].since_unix_ms);
    assert.equal(edges[2].since_unix_ms, edges[3].since_unix_ms);
    assert.ok(edges[2].since_unix_ms > edges[0].since_unix_ms);
    assert.ok(!JSON.stringify(rows).includes("PRIVATE_"));
    seen = rows.length - 1;
    assert.equal(rows.at(-1).state, "exited", "shutdown waits for delivery");
    assert.equal((await next("exited")).session_id, "second");
});
