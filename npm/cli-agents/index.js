const os = require("os");

const PLATFORMS = {
  "darwin-arm64": "@cueframe/cli-agents-darwin-arm64",
  "darwin-x64": "@cueframe/cli-agents-darwin-x64",
  "linux-x64": "@cueframe/cli-agents-linux-x64",
  "linux-arm64": "@cueframe/cli-agents-linux-arm64",
  "win32-x64": "@cueframe/cli-agents-win32-x64",
};

function binaryPath() {
  const platform = os.platform();
  const key = `${platform}-${os.arch()}`;
  const pkg = PLATFORMS[key];
  if (!pkg) {
    throw new Error(
      `Unsupported platform: ${key}. Supported: ${Object.keys(PLATFORMS).join(", ")}`
    );
  }
  const bin = platform === "win32" ? "bin/cli-agents.exe" : "bin/cli-agents";
  try {
    return require.resolve(`${pkg}/${bin}`);
  } catch {
    throw new Error(
      `Package ${pkg} is not installed. ` +
      `Try: npm install @cueframe/cli-agents`
    );
  }
}

module.exports = { binaryPath };
