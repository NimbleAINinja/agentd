// agentd-integration agentd-v1.1 harness=opencode
// Written by `agentd integrate install opencode`; `agentd integrate uninstall
// opencode` removes it. Local edits are replaced on the next install.
//
// opencode has no command hooks, so this plugin translates opencode events
// into Agentd hook events. Each hook runs as a direct child of the opencode
// process, which is how Agentd resolves the agent it belongs to.

const AGENTD = __AGENTD_EXECUTABLE__;
const ACTIVE_REPEAT_MS = 2000;

let lastActiveAt = 0;
let lastEvent = null;
const busySessions = new Set();

function fire(event, active) {
  const now = Date.now();
  if (active) {
    // Tool calls and status updates arrive in bursts; one claim per window
    // is enough to keep the activity age fresh.
    if (lastActiveAt !== 0 && now - lastActiveAt < ACTIVE_REPEAT_MS) {
      return;
    }
    lastActiveAt = now;
  } else {
    // opencode reports the end of a turn as both session.status and
    // session.idle; one claim is enough.
    if (event === "Idle" && lastEvent === "Idle") return;
    lastActiveAt = 0;
  }
  lastEvent = event;
  try {
    Bun.spawn(
      [AGENTD, "hook", "--integration", "agentd-v1.1", "--harness", "opencode", "--event", event],
      { stdin: "ignore", stdout: "ignore", stderr: "ignore" },
    ).unref();
  } catch {
    // Agentd is best-effort; never disturb the session.
  }
}

function sessionBusy(sessionID) {
  if (sessionID) busySessions.add(sessionID);
  fire("Busy", true);
}

function sessionIdle(sessionID) {
  if (sessionID) busySessions.delete(sessionID);
  // A subagent session finishing does not make the process idle.
  if (busySessions.size === 0) fire("Idle", false);
}

export const AgentdIntegration = async () => ({
  "chat.message": async () => fire("PromptSubmit", true),
  "tool.execute.before": async () => fire("ToolBefore", true),
  event: async ({ event }) => {
    const properties = event?.properties ?? {};
    switch (event?.type) {
      case "session.status":
        if (properties.status?.type === "busy") sessionBusy(properties.sessionID);
        else if (properties.status?.type === "idle") sessionIdle(properties.sessionID);
        break;
      case "session.idle":
        sessionIdle(properties.sessionID);
        break;
      case "permission.asked":
      case "permission.updated":
        fire("PermissionAsked", false);
        break;
      case "question.asked":
        fire("QuestionAsked", false);
        break;
      case "permission.replied":
      case "question.replied":
      case "question.rejected":
        fire("Replied", false);
        break;
    }
  },
});
