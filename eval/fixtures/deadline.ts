// Twelve calls against the shorter run deadline set in eval/fixtures.json. A long
// call leads and the rest are short. The deadline falls midway between call
// completions, so each policy splits outcomes differently between succeeded,
// unknown (dispatched but unobserved), and cancelled_before_dispatch.
const latencies = [700, 200, 200, 200, 200, 200, 200, 200, 200, 200, 200, 200];
return await Promise.all(
  latencies.map((ms, index) => tools.call("mock.wait", { ms, value: index })),
);
