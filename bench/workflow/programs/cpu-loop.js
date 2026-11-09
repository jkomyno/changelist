// CPU loop with no tool calls and no await: only an engine-enforced budget can stop it.
let i = 0;
while (true) {
  i = (i + 1) % 1000003;
}
