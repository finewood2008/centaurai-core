import fs from "node:fs";
import path from "node:path";

const baseUrl = process.env.AIONCORE_BASE_URL || "http://127.0.0.1:51446";
const password = process.env.STRESS_PASSWORD;
if (!password) throw new Error("STRESS_PASSWORD is required");

const durationMs = Number(process.env.STRESS_DURATION_SECS || 600) * 1000;
const intervalMs = Number(process.env.STRESS_INTERVAL_SECS || 60) * 1000;
const pollIntervalMs = Number(process.env.STRESS_POLL_SECS || 6) * 1000;
const turnTimeoutMs = Number(process.env.STRESS_TURN_TIMEOUT_SECS || 180) * 1000;
const aionPid = Number(process.env.AIONCORE_PID || 0);
const gatewayPid = Number(process.env.OPENCLAW_GATEWAY_PID || 0);
const gatewayCgroup = process.env.OPENCLAW_CGROUP || "/user.slice/user-1000.slice/user@1000.service/app.slice/openclaw-gateway.service";
const resultPath = process.env.STRESS_RESULT || "/tmp/aioncore-real-stress-result.json";

const users = [
  ["admin", "hermes"],
  ...Array.from({ length: 9 }, (_, index) => [`real-user-${index + 1}`, index % 2 === 0 ? "openclaw" : "hermes"]),
];

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const ipFor = (index) => `10.254.0.${index + 10}`;
const jsonHeaders = (session) => ({
  Authorization: `Bearer ${session.token}`,
  "X-CSRF-Token": session.csrf,
  Cookie: `aionui-csrf-token=${session.csrf}`,
  "X-Forwarded-For": ipFor(session.index),
});

function csrfFromHeaders(headers) {
  const values = typeof headers.getSetCookie === "function" ? headers.getSetCookie() : [headers.get("set-cookie") || ""];
  const cookie = values.find((value) => value.includes("aionui-csrf-token=")) || "";
  return cookie.match(/aionui-csrf-token=([^;]+)/)?.[1] || "";
}

async function request(url, options = {}) {
  const response = await fetch(`${baseUrl}${url}`, options);
  let body = null;
  try { body = await response.json(); } catch {}
  return { response, body };
}

async function login(index, username) {
  const headers = { "X-Forwarded-For": ipFor(index) };
  const status = await request("/api/auth/status", { headers });
  const csrf = csrfFromHeaders(status.response.headers);
  const loginResponse = await request("/login", {
    method: "POST",
    headers: { ...headers, "Content-Type": "application/json" },
    body: JSON.stringify({ username, password }),
  });
  if (!loginResponse.response.ok || !loginResponse.body?.token) {
    throw new Error(`login failed for ${username}: ${loginResponse.response.status}`);
  }
  return { index, username, backend: users[index][1], token: loginResponse.body.token, csrf };
}

async function createConversation(session) {
  const agentId = session.backend === "hermes" ? "55f3ed1c" : "b7e8a9c4";
  const created = await request("/api/conversations", {
    method: "POST",
    headers: { ...jsonHeaders(session), "Content-Type": "application/json" },
    body: JSON.stringify({
      type: "acp",
      name: `Real ${session.backend} user-${session.index}`,
      extra: { backend: session.backend, agent_id: agentId },
    }),
  });
  if (!created.response.ok || !created.body?.data?.id) {
    throw new Error(`conversation creation failed for ${session.username}: ${created.response.status}`);
  }
  session.conversationId = created.body.data.id;
}

async function sendTurn(session, metrics, startedAt) {
  const marker = `${session.backend.toUpperCase()}_${session.index}_${session.sequence + 1}`;
  const requestStarted = performance.now();
  const result = await request(`/api/conversations/${session.conversationId}/messages`, {
    method: "POST",
    headers: { ...jsonHeaders(session), "Content-Type": "application/json" },
    body: JSON.stringify({
      content: `容量压测 ${marker}。只回复 ${marker}_OK；不要调用任何工具，不要读取或写入文件。`,
      files: [], inject_skills: [], hidden: false,
    }),
  });
  const latency = performance.now() - requestStarted;
  metrics.maxHttpMs = Math.max(metrics.maxHttpMs, latency);
  if (result.response.status !== 202 || !result.body?.data?.turn_id) {
    metrics.rejections.push({ user: session.username, status: result.response.status, error: result.body?.error, code: result.body?.code });
    session.nextDue = Date.now() + intervalMs;
    return;
  }
  session.sequence += 1;
  session.outstanding = { turnId: result.body.data.turn_id, sentAt: Date.now(), marker };
  metrics.accepted += 1;
  metrics.byBackend[session.backend].accepted += 1;
  if (!metrics.firstAcceptedAt) metrics.firstAcceptedAt = startedAt;
}

async function pollSession(session, metrics) {
  if (!session.outstanding) return;
  const runs = await request("/api/agent-runs", { headers: jsonHeaders(session) });
  if (!runs.response.ok) {
    metrics.pollErrors += 1;
    return;
  }
  const active = (runs.body?.data || []).some((run) => run.turn_id === session.outstanding.turnId);
  if (active) {
    if (Date.now() - session.outstanding.sentAt > turnTimeoutMs && !session.outstanding.cancelRequested) {
      session.outstanding.cancelRequested = true;
      metrics.cancelRequested += 1;
      await request(`/api/conversations/${session.conversationId}/cancel`, {
        method: "POST",
        headers: { ...jsonHeaders(session), "Content-Type": "application/json" },
        body: JSON.stringify({ turn_id: session.outstanding.turnId }),
      });
    }
    return;
  }
  metrics.observedTerminal += 1;
  metrics.byBackend[session.backend].completedObserved += 1;
  session.outstanding = null;
  session.nextDue = startedAtGlobal + session.sequence * intervalMs;
}

function readProcessSnapshot() {
  const processes = [];
  for (const entry of fs.readdirSync("/proc")) {
    if (!/^\d+$/.test(entry)) continue;
    const pid = Number(entry);
    try {
      const status = fs.readFileSync(`/proc/${pid}/status`, "utf8");
      const ppid = Number(status.match(/^PPid:\s+(\d+)/m)?.[1] || 0);
      const rssKb = Number(status.match(/^VmRSS:\s+(\d+) kB/m)?.[1] || 0);
      const cmdline = fs.readFileSync(`/proc/${pid}/cmdline`, "utf8").replaceAll("\0", " ").trim();
      const rollup = fs.readFileSync(`/proc/${pid}/smaps_rollup`, "utf8");
      const pssKb = Number(rollup.match(/^Pss:\s+(\d+) kB/m)?.[1] || 0);
      processes.push({ pid, ppid, rssKb, pssKb, cmdline });
    } catch {}
  }
  const byPid = new Map(processes.map((process) => [process.pid, process]));
  const descendants = (rootPid) => processes.filter((process) => {
    let current = process;
    while (current && current.pid !== rootPid) current = byPid.get(current.ppid);
    return process.pid !== rootPid && current?.pid === rootPid;
  });
  const children = aionPid ? descendants(aionPid) : [];
  const sum = (items, field) => items.reduce((total, item) => total + item[field], 0);
  const hermes = children.filter((process) => /hermes/i.test(process.cmdline));
  const openclaw = children.filter((process) => /openclaw/i.test(process.cmdline));
  const aion = byPid.get(aionPid);
  let gatewayCgroupBytes = null;
  try { gatewayCgroupBytes = Number(fs.readFileSync(`/sys/fs/cgroup${gatewayCgroup}/memory.current`, "utf8")); } catch {}
  const meminfo = fs.readFileSync("/proc/meminfo", "utf8");
  const totalKb = Number(meminfo.match(/^MemTotal:\s+(\d+) kB/m)?.[1] || 0);
  const availableKb = Number(meminfo.match(/^MemAvailable:\s+(\d+) kB/m)?.[1] || 0);
  return {
    memoryUsedPercent: totalKb ? ((totalKb - availableKb) / totalKb) * 100 : null,
    memoryAvailableMb: availableKb / 1024,
    aionPssMb: (aion?.pssKb || 0) / 1024,
    aionRssMb: (aion?.rssKb || 0) / 1024,
    hermesPssMb: sum(hermes, "pssKb") / 1024,
    openclawAcpPssMb: sum(openclaw, "pssKb") / 1024,
    gatewayPssMb: ((gatewayPid && byPid.get(gatewayPid)?.pssKb) || 0) / 1024,
    gatewayCgroupMb: gatewayCgroupBytes === null ? null : gatewayCgroupBytes / 1024 / 1024,
    childCount: children.length,
    hermesProcessCount: hermes.length,
    openclawProcessCount: openclaw.length,
  };
}

let startedAtGlobal = 0;
const metrics = {
  accepted: 0,
  observedTerminal: 0,
  cancelRequested: 0,
  pollErrors: 0,
  maxHttpMs: 0,
  maxActive: 0,
  maxQueued: 0,
  maxEffectiveLimit: 0,
  rejections: [],
  firstAcceptedAt: null,
  byBackend: { hermes: { accepted: 0, completedObserved: 0 }, openclaw: { accepted: 0, completedObserved: 0 } },
  peak: {},
};

function updatePeak(snapshot) {
  for (const [key, value] of Object.entries(snapshot)) {
    if (typeof value !== "number" || !Number.isFinite(value)) continue;
    metrics.peak[key] = Math.max(metrics.peak[key] || 0, value);
  }
}

async function sampleRuntime(sessions) {
  const runtime = await request("/api/admin/agent-runtime-status", {
    headers: { Authorization: `Bearer ${sessions[0].token}`, "X-Forwarded-For": "10.254.0.1" },
  });
  if (runtime.response.ok) {
    const data = runtime.body?.data || {};
    metrics.maxActive = Math.max(metrics.maxActive, Number(data.active_count || 0));
    metrics.maxQueued = Math.max(metrics.maxQueued, Number(data.queued_count || 0));
    metrics.maxEffectiveLimit = Math.max(metrics.maxEffectiveLimit, Number(data.effective_active_limit || 0));
  }
  const snapshot = readProcessSnapshot();
  updatePeak(snapshot);
  return snapshot;
}

const sessions = await Promise.all(users.map(([username], index) => login(index, username)));
await Promise.all(sessions.map(createConversation));
for (const session of sessions) {
  session.sequence = 0;
  session.outstanding = null;
  session.nextDue = 0;
}
startedAtGlobal = Date.now();
for (const session of sessions) session.nextDue = startedAtGlobal;

console.log(JSON.stringify({ event: "real_pressure_started", duration_secs: durationMs / 1000, interval_secs: intervalMs / 1000, users: sessions.map(({ username, backend }) => ({ username, backend })) }));
let lastReport = 0;
let drainDeadline = null;
while (Date.now() < (drainDeadline || startedAtGlobal + durationMs) || sessions.some((session) => session.outstanding)) {
  const now = Date.now();
  if (!drainDeadline && now >= startedAtGlobal + durationMs) drainDeadline = now + turnTimeoutMs + 30_000;
  await Promise.all(sessions.map((session) => pollSession(session, metrics)));
  if (!drainDeadline) {
    await Promise.all(sessions.filter((session) => !session.outstanding && Date.now() >= session.nextDue).map((session) => sendTurn(session, metrics, startedAtGlobal)));
  }
  const snapshot = await sampleRuntime(sessions);
  if (now - lastReport >= 30_000) {
    lastReport = now;
    console.log(JSON.stringify({ event: "real_pressure_progress", elapsed_secs: Math.round((now - startedAtGlobal) / 1000), accepted: metrics.accepted, completed_observed: metrics.observedTerminal, active: metrics.maxActive, queued: metrics.maxQueued, memory_used_percent: snapshot.memoryUsedPercent, gateway_cgroup_mb: snapshot.gatewayCgroupMb, hermes_pss_mb: snapshot.hermesPssMb, openclaw_acp_pss_mb: snapshot.openclawAcpPssMb }));
  }
  await sleep(pollIntervalMs);
}

const result = {
  duration_secs: Math.round((Date.now() - startedAtGlobal) / 1000),
  accepted: metrics.accepted,
  completed_observed: metrics.observedTerminal,
  cancel_requested: metrics.cancelRequested,
  poll_errors: metrics.pollErrors,
  unexpected_rejections: metrics.rejections,
  max_http_ms: Math.round(metrics.maxHttpMs),
  max_active: metrics.maxActive,
  max_queued: metrics.maxQueued,
  max_effective_limit: metrics.maxEffectiveLimit,
  by_backend: metrics.byBackend,
  peak: metrics.peak,
  conversations: sessions.map(({ username, backend, conversationId, sequence }) => ({ username, backend, conversation_id: conversationId, turns_sent: sequence })),
};
fs.mkdirSync(path.dirname(resultPath), { recursive: true });
fs.writeFileSync(resultPath, `${JSON.stringify(result, null, 2)}\n`);
console.log(JSON.stringify({ event: "real_pressure_finished", ...result }));
