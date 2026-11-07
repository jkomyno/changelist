// Guest program for the cancellation check: one slow Pi tool call that the
// host aborts while it is running.
return await tools.call("demo_wait", { ms: 30000, value: "too late" });
