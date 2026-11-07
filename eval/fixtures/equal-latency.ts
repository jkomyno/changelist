// Eight equal calls submitted together. With a cap of 4, a batch barrier and a
// rolling refill should produce the same schedule, so no difference is expected.
const latencies = [50, 50, 50, 50, 50, 50, 50, 50];
return await Promise.all(
  latencies.map((ms, index) => tools.call("mock.wait", { ms, value: index })),
);
