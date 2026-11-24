// Quadra for VS Code / Cursor: starts `quadra lsp` and connects it to the editor.
const path = require("path");
const fs = require("fs");
const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

let client;

function findServer() {
  const configured = vscode.workspace.getConfiguration("quadra").get("serverPath");
  if (configured) return configured;
  for (const folder of vscode.workspace.workspaceFolders || []) {
    // the workspace can be the repository or a folder inside it (e.g. examples/extended)
    let dir = folder.uri.fsPath;
    for (let i = 0; i < 4; i++) {
      for (const profile of ["release", "debug"]) {
        const candidate = path.join(dir, "target", profile, "quadra");
        if (fs.existsSync(candidate)) return candidate;
      }
      dir = path.dirname(dir);
    }
  }
  return "quadra";
}

function activate(context) {
  const command = findServer();
  const serverOptions = { command, args: ["lsp"] };
  const clientOptions = {
    documentSelector: [
      { scheme: "file", language: "quadra" },
      { scheme: "file", language: "qdi" },
    ],
    // the interface files change with the code: tell the server when they change on disk
    synchronize: { fileEvents: vscode.workspace.createFileSystemWatcher("**/*.{qd,qdi}") },
    outputChannelName: "Quadra",
  };
  client = new LanguageClient("quadra", "Quadra", serverOptions, clientOptions);
  client.start().catch((err) => {
    vscode.window.showErrorMessage(
      `Quadra: cannot start the language server (${command}). Build it with \`cargo build\` ` +
        `or set "quadra.serverPath". ${err}`
    );
  });
  context.subscriptions.push({ dispose: () => client && client.stop() });
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
