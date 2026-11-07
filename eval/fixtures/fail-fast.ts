// Promise.all rejects on its first failure. The failure is fast and second in
// submission order, so slower calls are running or queued when the guest
// returns its error. The host collects dispatched calls and cancels queued ones;
// how much work each policy started before the error is a finding, not a check.
const latencies: (number | null)[] = [120, null, 120, 120, 120, 120, 120, 120];
await Promise.all(
  latencies.map((ms, index) =>
    ms === null
      ? tools.call("mock.fail", { message: "fast failure" })
      : tools.call("mock.wait", { ms, value: index })
  ),
);
return "unreachable";
