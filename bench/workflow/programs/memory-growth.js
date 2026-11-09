// Unbounded memory growth: only an engine memory limit, a host heap limit, or a deadline stops it.
const xs = [];
while (true) {
  xs.push("x".repeat(1024) + xs.length);
}
