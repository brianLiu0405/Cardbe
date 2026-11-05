import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

// Match the loopback API's default Origin allowlist and isolate generated files.
const result = spawnSync("vp", ["dev"], {
  cwd: fileURLToPath(new URL("..", import.meta.url)),
  stdio: "inherit",
  env: { ...process.env, CARDBE_DEV_PORT: "1441" },
  shell: process.platform === "win32",
});
if (result.error) {
  console.error(
    "Could not start Vite+. Install the project's pinned toolchain first.",
  );
}
process.exit(result.status ?? 1);
