// Event-driven OMP producer for zj-radar. OMP loads this file as an extension.
// @ts-nocheck

import { spawn } from "node:child_process";

type Status = "idle" | "running" | "pending" | "done" | "gone";

const paneId = process.env.ZELLIJ_PANE_ID;
let tail = Promise.resolve();

function args(status: Status): string[] {
  if (status === "gone") {
    return ["notify", "generic", "--gone", "--source", "omp"];
  }

  const result = ["notify", "generic", "--status", status, "--source", "omp"];
  if (status === "running") result.push("--msg", "working");
  if (status === "pending") result.push("--msg", "needs you");
  if (status === "done") result.push("--msg", "done");
  return result;
}

function send(status: Status): Promise<void> {
  if (!paneId) return Promise.resolve();

  return new Promise(resolve => {
    let child;
    try {
      child = spawn("zj-radar", args(status), { stdio: "ignore", env: process.env });
      child.unref();
    } catch {
      resolve();
      return;
    }

    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      resolve();
    };
    const timeout = setTimeout(() => {
      child.kill();
      finish();
    }, 5000);
    timeout.unref();
    child.once("error", finish);
    child.once("close", finish);
  });
}

function publish(status: Status): Promise<void> {
  tail = tail.then(() => send(status), () => send(status));
  return tail;
}

export default function (pi) {
  if (!paneId) return;

  let root = false;
  let active = false;
  let pending = 0;

  const activate = ctx => {
    if (root) return true;
    if (ctx?.hasUI !== true) return false;
    root = true;
    publish("idle");
    return true;
  };
  const publishState = () => publish(pending > 0 ? "pending" : active ? "running" : "done");

  pi.on("session_start", (_event, ctx) => activate(ctx));
  pi.on("agent_start", (_event, ctx) => {
    if (!activate(ctx)) return;
    active = true;
    pending = 0;
    publishState();
  });
  pi.on("agent_end", event => {
    if (!root || event?.willContinue === true) return;
    active = false;
    pending = 0;
    publishState();
  });
  pi.on("tool_approval_requested", (_event, ctx) => {
    if (!activate(ctx)) return;
    pending += 1;
    publishState();
  });
  pi.on("tool_approval_resolved", (_event, ctx) => {
    if (!activate(ctx)) return;
    pending = Math.max(0, pending - 1);
    publishState();
  });
  pi.on("session_shutdown", () => {
    if (root) return publish("gone");
  });
}
