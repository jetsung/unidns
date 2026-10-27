/* unidns 前端：无构建链的原生 JS 单页应用（hash 路由 + REST API 消费） */
"use strict";

const $app = document.getElementById("app");
const $topbar = document.getElementById("topbar");
const $toast = document.getElementById("toast");

// ---------- 基础工具 ----------

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

let toastTimer = null;
function toast(msg, kind = "") {
  $toast.textContent = msg;
  $toast.className = "toast " + kind;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => $toast.classList.add("hidden"), 3200);
}

async function api(path, options = {}) {
  const opts = { headers: {}, ...options };
  if (opts.body && typeof opts.body !== "string") {
    opts.body = JSON.stringify(opts.body);
    opts.headers["Content-Type"] = "application/json";
  }
  const res = await fetch(path, opts);
  if (res.status === 401 && !path.startsWith("/api/v1/auth/")) {
    location.hash = "#/login";
    throw new Error("未登录或会话已过期");
  }
  let body = null;
  try { body = await res.json(); } catch { /* 无响应体 */ }
  if (!res.ok) {
    const message = body?.error?.message || `请求失败（HTTP ${res.status}）`;
    const err = new Error(message);
    err.code = body?.error?.code;
    err.status = res.status;
    throw err;
  }
  return body;
}

function modal(html) {
  const mask = document.createElement("div");
  mask.className = "modal-mask";
  mask.innerHTML = `<div class="modal">${html}</div>`;
  mask.addEventListener("click", (e) => { if (e.target === mask) mask.remove(); });
  document.body.appendChild(mask);
  return mask;
}

function fmtTime(s) {
  if (!s) return "—";
  return String(s).replace("T", " ").replace(/([+-]\d\d:\d\d|Z)$/, "");
}

const PROVIDER_FIELDS = {
  aliyun: [
    { key: "access_key_id", label: "AccessKey ID", hint: "阿里云控制台创建的 AccessKey ID" },
    { key: "access_key_secret", label: "AccessKey Secret", hint: "仅要求云解析 DNS 只读/读写权限" },
  ],
  dnspod: [
    { key: "secret_id", label: "SecretId", hint: "腾讯云国内站 API 密钥" },
    { key: "secret_key", label: "SecretKey", hint: "仅要求 DNSPod 解析权限" },
  ],
  dnspod_intl: [
    { key: "secret_id", label: "SecretId", hint: "腾讯云国际站（tencentcloud.com）API 密钥" },
    { key: "secret_key", label: "SecretKey", hint: "仅要求 DNSPod 解析权限" },
  ],
  edgeone: [
    { key: "secret_id", label: "SecretId", hint: "EdgeOne 国内站 API 密钥" },
    { key: "secret_key", label: "SecretKey", hint: "要求 EdgeOne（teo）读写权限，站点需为 NS 接入" },
  ],
  edgeone_intl: [
    { key: "secret_id", label: "SecretId", hint: "EdgeOne 国际站（edgeone.ai）API 密钥" },
    { key: "secret_key", label: "SecretKey", hint: "要求 EdgeOne（teo）读写权限，站点需为 NS 接入" },
  ],
  cloudflare: [
    { key: "api_token", label: "API Token", hint: "Zone.DNS 编辑权限的 API Token" },
  ],
};

// ---------- 引导 / 登录 ----------

function renderSetup() {
  $topbar.classList.add("hidden");
  $app.innerHTML = `
    <div class="auth-wrap"><div class="auth-card">
      <h1>欢迎使用 unidns</h1>
      <div class="hint">首次使用，请设置管理员口令（至少 8 个字符）</div>
      <form class="stack" id="setup-form">
        <label>口令 <input type="password" name="password" required minlength="8" autocomplete="new-password"></label>
        <label>确认口令 <input type="password" name="password2" required minlength="8" autocomplete="new-password"></label>
        <button class="btn primary" type="submit">完成设置并进入</button>
      </form>
    </div></div>`;
  document.getElementById("setup-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const f = e.target;
    if (f.password.value !== f.password2.value) return toast("两次输入的口令不一致", "err");
    try {
      await api("/api/v1/auth/setup", { method: "POST", body: { password: f.password.value } });
      toast("口令设置成功", "ok");
      location.hash = "#/accounts";
    } catch (err) { toast(err.message, "err"); }
  });
}

function renderLogin() {
  $topbar.classList.add("hidden");
  $app.innerHTML = `
    <div class="auth-wrap"><div class="auth-card">
      <h1>unidns 登录</h1>
      <div class="hint">多平台域名解析集中管理</div>
      <form class="stack" id="login-form">
        <label>管理员口令 <input type="password" name="password" required autocomplete="current-password"></label>
        <button class="btn primary" type="submit">登录</button>
      </form>
    </div></div>`;
  document.getElementById("login-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      await api("/api/v1/auth/login", { method: "POST", body: { password: e.target.password.value } });
      toast("登录成功", "ok");
      location.hash = "#/accounts";
    } catch (err) { toast(err.message, "err"); }
  });
}

// ---------- 账户管理页 ----------

async function renderAccounts() {
  $topbar.classList.remove("hidden");
  setActiveNav("accounts");
  let settings = { interval_minutes: null };
  try { settings = await api("/api/v1/sync/settings"); } catch { /* 忽略 */ }
  const { items } = await api("/api/v1/accounts");

  $app.innerHTML = `
    <div class="page-head">
      <h1>平台账户</h1>
      <div class="grow"></div>
      <button class="btn" id="interval-btn">定时同步：每 ${esc(settings.interval_minutes)} 分钟</button>
      <button class="btn" id="sync-all-btn">全量同步</button>
      <button class="btn primary" id="add-account-btn">添加账户</button>
    </div>
    <div class="card">
      <table>
        <thead><tr>
          <th>平台</th><th>名称</th><th>凭据</th><th>状态</th><th>连通性验证</th><th>最近同步</th><th>操作</th>
        </tr></thead>
        <tbody>
          ${items.length === 0 ? `<tr><td colspan="7"><div class="empty">还没有平台账户，点击「添加账户」接入阿里云 / 腾讯云 / CloudFlare</div></td></tr>` : ""}
          ${items.map((a) => `
            <tr>
              <td><span class="tag provider">${esc(a.provider_name)}</span></td>
              <td>${esc(a.name)}</td>
              <td class="mono">${esc(a.credentials_mask)}</td>
              <td>${a.status === "enabled" ? '<span class="tag ok">启用</span>' : '<span class="tag warn">停用</span>'}</td>
              <td>${verifyTag(a)}</td>
              <td>${syncTag(a)}</td>
              <td class="actions">
                <button class="btn small" data-act="verify" data-id="${a.id}">验证</button>
                <button class="btn small" data-act="sync" data-id="${a.id}">同步</button>
                <button class="btn small" data-act="runs" data-id="${a.id}">历史</button>
                <button class="btn small" data-act="toggle" data-id="${a.id}" data-status="${a.status}">${a.status === "enabled" ? "停用" : "启用"}</button>
                <button class="btn small danger" data-act="delete" data-id="${a.id}" data-name="${esc(a.name)}">删除</button>
              </td>
            </tr>`).join("")}
        </tbody>
      </table>
    </div>`;

  document.getElementById("add-account-btn").onclick = showAddAccount;
  document.getElementById("sync-all-btn").onclick = async (e) => {
    e.target.disabled = true;
    e.target.textContent = "同步中…";
    try {
      const r = await api("/api/v1/sync", { method: "POST" });
      toast(`全量同步完成：${r.total} 个账户，失败 ${r.failed} 个`, r.failed ? "err" : "ok");
      renderAccounts();
    } catch (err) { toast(err.message, "err"); e.target.disabled = false; e.target.textContent = "全量同步"; }
  };
  document.getElementById("interval-btn").onclick = showIntervalSettings;

  $app.querySelectorAll("button[data-act]").forEach((btn) => {
    btn.addEventListener("click", () => accountAction(btn.dataset.act, btn.dataset.id, btn.dataset.name, btn.dataset.status));
  });
}

function verifyTag(a) {
  if (a.last_verified_at == null) return '<span class="tag">未验证</span>';
  if (a.last_verify_ok) return `<span class="tag ok">通过</span> <span class="muted">${esc(fmtTime(a.last_verified_at))}</span>`;
  return `<span class="tag err">失败</span> <span class="muted" title="${esc(a.last_verify_msg || "")}">${esc(fmtTime(a.last_verified_at))}</span>`;
}

function syncTag(a) {
  if (a.last_sync_at == null) return '<span class="tag">从未同步</span>';
  const cls = a.last_sync_status === "success" ? "ok" : "err";
  const label = a.last_sync_status === "success" ? "成功" : "失败";
  const err = a.last_sync_error ? ` title="${esc(a.last_sync_error)}"` : "";
  return `<span class="tag ${cls}"${err}>${label}</span> <span class="muted">${esc(fmtTime(a.last_sync_at))}</span>`;
}

function showAddAccount() {
  const mask = modal(`
    <h2>添加平台账户</h2>
    <form class="stack" id="add-account-form">
      <label>平台
        <select name="provider_type" id="provider-select">
          <option value="aliyun">阿里云</option>
          <option value="dnspod">腾讯云 DNSPod（国内站）</option>
          <option value="dnspod_intl">腾讯云 DNSPod（国际站）</option>
          <option value="edgeone">EdgeOne（国内站）</option>
          <option value="edgeone_intl">EdgeOne（国际站）</option>
          <option value="cloudflare">CloudFlare</option>
        </select>
      </label>
      <label>账户名称 <input name="name" required placeholder="例如：生产环境阿里云"></label>
      <div id="cred-fields"></div>
      <div class="modal-actions">
        <button type="button" class="btn" id="cancel-btn">取消</button>
        <button type="submit" class="btn primary">保存账户</button>
      </div>
    </form>`);

  const renderFields = (ptype) => {
    document.getElementById("cred-fields").innerHTML = PROVIDER_FIELDS[ptype]
      .map((f) => `<label>${esc(f.label)} <input name="${f.key}" required autocomplete="off"> <span class="muted" style="font-size:12px">${esc(f.hint)}</span></label>`)
      .join("");
  };
  renderFields("aliyun");
  document.getElementById("provider-select").onchange = (e) => renderFields(e.target.value);
  mask.querySelector("#cancel-btn").onclick = () => mask.remove();

  mask.querySelector("#add-account-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const fd = new FormData(e.target);
    const providerType = fd.get("provider_type");
    const credentials = {};
    PROVIDER_FIELDS[providerType].forEach((f) => { credentials[f.key] = fd.get(f.key); });
    try {
      await api("/api/v1/accounts", { method: "POST", body: { name: fd.get("name"), provider_type: providerType, credentials } });
      toast("账户已添加，建议先执行「验证」确认凭据可用", "ok");
      mask.remove();
      renderAccounts();
    } catch (err) { toast(err.message, "err"); }
  });
}

function showIntervalSettings() {
  const mask = modal(`
    <h2>定时同步间隔</h2>
    <form class="stack" id="interval-form">
      <label>全量自动同步间隔（分钟） <input type="number" name="interval_minutes" min="1" required></label>
      <div class="modal-actions">
        <button type="button" class="btn" id="cancel-btn">取消</button>
        <button type="submit" class="btn primary">保存</button>
      </div>
    </form>`);
  api("/api/v1/sync/settings").then((s) => { mask.querySelector("input").value = s.interval_minutes; });
  mask.querySelector("#cancel-btn").onclick = () => mask.remove();
  mask.querySelector("#interval-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      await api("/api/v1/sync/settings", { method: "PUT", body: { interval_minutes: Number(e.target.interval_minutes.value) } });
      toast("同步间隔已保存（下一个调度周期生效）", "ok");
      mask.remove();
      renderAccounts();
    } catch (err) { toast(err.message, "err"); }
  });
}

async function accountAction(act, id, name, status) {
  try {
    if (act === "verify") {
      const r = await api(`/api/v1/accounts/${id}/verify`, { method: "POST" });
      toast(r.ok ? "连通性验证通过" : `验证失败：${r.message}`, r.ok ? "ok" : "err");
      renderAccounts();
    } else if (act === "sync") {
      const r = await api(`/api/v1/accounts/${id}/sync`, { method: "POST" });
      toast(r.success ? `同步成功：${r.domains} 个域名 / ${r.records} 条记录` : `同步失败：${r.message}`, r.success ? "ok" : "err");
      renderAccounts();
    } else if (act === "toggle") {
      const target = status === "enabled" ? "disabled" : "enabled";
      await api(`/api/v1/accounts/${id}`, { method: "PATCH", body: { status: target } });
      toast(target === "enabled" ? "账户已启用" : "账户已停用（不再参与同步）", "ok");
      renderAccounts();
    } else if (act === "delete") {
      if (!confirm(`确认删除账户「${name}」？\n\n仅删除本地数据（账户、域名、解析记录），不会改动平台侧任何资源。`)) return;
      await api(`/api/v1/accounts/${id}`, { method: "DELETE" });
      toast("账户已删除", "ok");
      renderAccounts();
    } else if (act === "runs") {
      showSyncRuns(id, name);
    }
  } catch (err) { toast(err.message, "err"); }
}

function showSyncRuns(accountId, name) {
  const mask = modal(`<h2>同步历史 · ${esc(name)}</h2><div id="runs-body"><div class="loading">加载中…</div></div>
    <div class="modal-actions"><button class="btn" id="close-btn">关闭</button></div>`);
  mask.querySelector("#close-btn").onclick = () => mask.remove();
  api(`/api/v1/sync/runs?account_id=${accountId}`).then(({ items }) => {
    mask.querySelector("#runs-body").innerHTML = items.length === 0
      ? '<div class="empty">暂无同步记录</div>'
      : `<table><thead><tr><th>开始时间</th><th>结果</th><th>域名/记录</th><th>错误</th></tr></thead><tbody>
        ${items.map((r) => `<tr>
          <td class="mono">${esc(fmtTime(r.started_at))}</td>
          <td>${r.status === "success" ? '<span class="tag ok">成功</span>' : r.status === "failed" ? '<span class="tag err">失败</span>' : '<span class="tag">进行中</span>'}</td>
          <td class="muted">${r.stats ? `${r.stats.domains ?? "—"}/${r.stats.records ?? "—"}` : "—"}</td>
          <td class="muted">${esc(r.error || "—")}</td>
        </tr>`).join("")}</tbody></table>`;
  }).catch((err) => { mask.querySelector("#runs-body").innerHTML = `<div class="empty error-note">${esc(err.message)}</div>`; });
}

// ---------- 域名列表页 ----------

async function renderDomains() {
  $topbar.classList.remove("hidden");
  setActiveNav("domains");
  const params = new URLSearchParams(location.hash.split("?")[1] || "");
  const keyword = params.get("keyword") || "";
  const provider = params.get("provider") || "";
  const query = new URLSearchParams();
  if (keyword) query.set("keyword", keyword);
  if (provider) query.set("provider", provider);
  const { items } = await api("/api/v1/domains" + (query.toString() ? `?${query}` : ""));

  $app.innerHTML = `
    <div class="page-head">
      <h1>域名</h1>
      <div class="grow"></div>
      <button class="btn" id="sync-all-btn">全量同步</button>
    </div>
    <div class="filter-bar">
      <input id="f-keyword" placeholder="按域名关键字过滤" value="${esc(keyword)}" style="width:240px">
      <select id="f-provider">
        <option value="">全部平台</option>
        <option value="aliyun" ${provider === "aliyun" ? "selected" : ""}>阿里云</option>
        <option value="dnspod" ${provider === "dnspod" ? "selected" : ""}>腾讯云 DNSPod（国内站）</option>
        <option value="dnspod_intl" ${provider === "dnspod_intl" ? "selected" : ""}>腾讯云 DNSPod（国际站）</option>
        <option value="edgeone" ${provider === "edgeone" ? "selected" : ""}>EdgeOne（国内站）</option>
        <option value="edgeone_intl" ${provider === "edgeone_intl" ? "selected" : ""}>EdgeOne（国际站）</option>
        <option value="cloudflare" ${provider === "cloudflare" ? "selected" : ""}>CloudFlare</option>
      </select>
      <button class="btn" id="f-apply">筛选</button>
    </div>
    <div class="card">
      <table>
        <thead><tr><th>域名</th><th>所属平台</th><th>账户</th><th>记录数</th><th>状态</th><th>同步时间</th></tr></thead>
        <tbody>
          ${items.length === 0 ? `<tr><td colspan="6"><div class="empty">没有域名。请先在「平台账户」添加账户并执行同步。</div></td></tr>` : ""}
          ${items.map((d) => `
            <tr>
              <td><a class="link" href="#/domain/${d.id}">${esc(d.name)}</a></td>
              <td><span class="tag provider">${esc(providerLabel(d.provider_type))}</span></td>
              <td>${esc(d.account_name)}</td>
              <td>${d.record_count}</td>
              <td>${d.status ? `<span class="tag ${d.status === "active" || d.status === "Enable" || d.status === "ENABLE" ? "ok" : ""}">${esc(d.status)}</span>` : "—"}</td>
              <td class="muted">${esc(fmtTime(d.synced_at))}</td>
            </tr>`).join("")}
        </tbody>
      </table>
    </div>`;

  document.getElementById("f-apply").onclick = () => {
    const q = new URLSearchParams();
    const kw = document.getElementById("f-keyword").value.trim();
    const pv = document.getElementById("f-provider").value;
    if (kw) q.set("keyword", kw);
    if (pv) q.set("provider", pv);
    location.hash = "#/domains" + (q.toString() ? `?${q}` : "");
  };
  document.getElementById("f-keyword").addEventListener("keydown", (e) => { if (e.key === "Enter") document.getElementById("f-apply").click(); });
  document.getElementById("sync-all-btn").onclick = async (e) => {
    e.target.disabled = true;
    try {
      const r = await api("/api/v1/sync", { method: "POST" });
      toast(`全量同步完成：${r.total} 个账户，失败 ${r.failed} 个`, r.failed ? "err" : "ok");
      renderDomains();
    } catch (err) { toast(err.message, "err"); e.target.disabled = false; }
  };
}

function providerLabel(t) {
  return {
    aliyun: "阿里云",
    dnspod: "腾讯云 DNSPod（国内站）",
    dnspod_intl: "腾讯云 DNSPod（国际站）",
    edgeone: "EdgeOne（国内站）",
    edgeone_intl: "EdgeOne（国际站）",
    cloudflare: "CloudFlare",
  }[t] || t;
}

// ---------- 域名详情页（解析记录管理） ----------

async function renderDomainDetail(domainId) {
  $topbar.classList.remove("hidden");
  setActiveNav("domains");
  const domain = await api(`/api/v1/domains/${domainId}`);
  const params = new URLSearchParams(location.hash.split("?")[1] || "");
  const filter = { type: params.get("type") || "", keyword: params.get("keyword") || "", status: params.get("status") || "" };
  const q = new URLSearchParams();
  if (filter.type) q.set("type", filter.type);
  if (filter.keyword) q.set("keyword", filter.keyword);
  if (filter.status) q.set("status", filter.status);
  const data = await api(`/api/v1/domains/${domainId}/records` + (q.toString() ? `?${q}` : ""));

  $app.innerHTML = `
    <div class="page-head">
      <a class="link" href="#/domains">← 域名列表</a>
      <h1>${esc(domain.name)}</h1>
      <span class="tag provider">${esc(domain.provider_name)}</span>
      <span class="tag">${esc(domain.account_name)}</span>
      <div class="grow"></div>
      <button class="btn" id="resync-btn">重新同步</button>
      <button class="btn primary" id="add-record-btn">添加记录</button>
    </div>
    <div class="filter-bar">
      <select id="f-type">
        <option value="">全部类型</option>
        ${["A", "AAAA", "CNAME", "MX", "TXT", "NS", "SRV", "CAA"].map((t) =>
          `<option value="${t}" ${filter.type === t ? "selected" : ""}>${t}</option>`).join("")}
      </select>
      <input id="f-keyword" placeholder="按主机记录过滤" value="${esc(filter.keyword)}" style="width:200px">
      <select id="f-status">
        <option value="">全部状态</option>
        <option value="enabled" ${filter.status === "enabled" ? "selected" : ""}>启用</option>
        <option value="disabled" ${filter.status === "disabled" ? "selected" : ""}>停用</option>
      </select>
      <button class="btn" id="f-apply">筛选</button>
      <span class="muted">共 ${data.total} 条</span>
    </div>
    <div class="card">
      <table>
        <thead><tr><th>主机记录</th><th>类型</th><th>记录值</th><th>TTL</th><th>线路</th><th>状态</th><th>操作</th></tr></thead>
        <tbody>
          ${data.items.length === 0 ? `<tr><td colspan="7"><div class="empty">没有匹配的解析记录</div></td></tr>` : ""}
          ${data.items.map((r) => recordRow(domain, r)).join("")}
        </tbody>
      </table>
    </div>`;

  const applyFilter = () => {
    const q2 = new URLSearchParams();
    const t = document.getElementById("f-type").value;
    const kw = document.getElementById("f-keyword").value.trim();
    const st = document.getElementById("f-status").value;
    if (t) q2.set("type", t);
    if (kw) q2.set("keyword", kw);
    if (st) q2.set("status", st);
    location.hash = `#/domain/${domainId}` + (q2.toString() ? `?${q2}` : "");
  };
  document.getElementById("f-apply").onclick = applyFilter;
  document.getElementById("f-keyword").addEventListener("keydown", (e) => { if (e.key === "Enter") applyFilter(); });
  document.getElementById("add-record-btn").onclick = () => showRecordForm(domainId, domain);
  document.getElementById("resync-btn").onclick = async (e) => {
    e.target.disabled = true;
    try {
      const r = await api(`/api/v1/accounts/${domain.account_id}/sync`, { method: "POST" });
      toast(r.success ? `同步成功：${r.domains} 个域名 / ${r.records} 条记录` : `同步失败：${r.message}`, r.success ? "ok" : "err");
      renderDomainDetail(domainId);
    } catch (err) { toast(err.message, "err"); e.target.disabled = false; }
  };

  $app.querySelectorAll("button[data-ract]").forEach((btn) => {
    btn.addEventListener("click", () => recordAction(btn.dataset.ract, btn.dataset.rid, btn.dataset.host, btn.dataset.target, domainId, domain));
  });
}

function recordRow(domain, r) {
  const statusTag = r.status === "enabled"
    ? '<span class="tag ok">启用</span>'
    : '<span class="tag warn">停用</span>';
  const pending = r.pending_sync ? ' <span class="tag err" title="平台侧状态与本地不一致，下次同步将自动对齐">待同步</span>' : "";
  const proxied = r.proxied != null ? (r.proxied ? ' <span class="tag" title="CloudFlare 代理已开启">proxied</span>' : "") : "";
  return `
    <tr>
      <td class="mono">${esc(r.host)}</td>
      <td><span class="tag">${esc(r.rtype)}</span></td>
      <td class="mono" style="word-break:break-all">${esc(r.value)}${proxied}</td>
      <td>${r.ttl}</td>
      <td class="muted">${r.line ? esc(r.line) : "—"}</td>
      <td>${statusTag}${pending}</td>
      <td class="actions">
        <button class="btn small" data-ract="edit" data-rid="${r.id}" data-host="${esc(r.host)}">编辑</button>
        <button class="btn small" data-ract="toggle" data-rid="${r.id}" data-host="${esc(r.host)}" data-target="${r.status === "enabled" ? "disabled" : "enabled"}">${r.status === "enabled" ? "停用" : "启用"}</button>
        <button class="btn small danger" data-ract="delete" data-rid="${r.id}" data-host="${esc(r.host)}">删除</button>
      </td>
    </tr>`;
}

function showRecordForm(domainId, domain, existing) {
  const isEdit = !!existing;
  const fields = isEdit ? existing : { host: "", rtype: "A", value: "", ttl: 600, line: "", proxied: false };
  const isCf = domain.provider_type === "cloudflare";
  const mask = modal(`
    <h2>${isEdit ? `编辑记录 · ${esc(existing.host)}` : "添加解析记录"}</h2>
    <form class="stack" id="record-form">
      <label>主机记录 <input name="host" required value="${esc(fields.host)}" placeholder="子域名前缀，根记录填 @">
        <span class="muted" style="font-size:12px">完整记录：&lt;主机记录&gt;.${esc(domain.name)}</span></label>
      <div style="display:flex;gap:12px">
        <label style="flex:1">类型
          <select name="rtype">
            ${["A", "AAAA", "CNAME", "MX", "TXT", "NS", "SRV", "CAA"].map((t) =>
              `<option value="${t}" ${fields.rtype === t ? "selected" : ""}>${t}</option>`).join("")}
          </select>
        </label>
        <label style="flex:1">TTL（秒） <input type="number" name="ttl" min="1" max="86400" required value="${fields.ttl}"></label>
      </div>
      <label>记录值 <input name="value" required value="${esc(fields.value)}" placeholder="IP / CNAME 目标 / 文本"></label>
      ${isCf
        ? `<label class="inline" style="flex-direction:row;align-items:center;gap:8px"><input type="checkbox" name="proxied" ${fields.proxied ? "checked" : ""}> 开启 CloudFlare 代理（proxied）</label>`
        : `<label>解析线路 <input name="line" value="${esc(fields.line || "")}" placeholder="阿里云/DNSPod 线路，留空为默认"></label>`}
      <div class="modal-actions">
        <button type="button" class="btn" id="cancel-btn">取消</button>
        <button type="submit" class="btn primary">${isEdit ? "保存修改" : "创建并回写平台"}</button>
      </div>
    </form>`);
  mask.querySelector("#cancel-btn").onclick = () => mask.remove();
  mask.querySelector("#record-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const fd = new FormData(e.target);
    const body = {
      host: fd.get("host").trim(),
      rtype: fd.get("rtype"),
      value: fd.get("value").trim(),
      ttl: Number(fd.get("ttl")),
    };
    if (isCf) body.proxied = fd.get("proxied") === "on";
    else if (fd.get("line")) body.line = fd.get("line").trim();
    try {
      if (isEdit) {
        await api(`/api/v1/records/${existing.id}`, { method: "PATCH", body });
        toast("记录已修改并回写平台", "ok");
      } else {
        await api(`/api/v1/domains/${domainId}/records`, { method: "POST", body });
        toast("记录已创建并回写平台", "ok");
      }
      mask.remove();
      renderDomainDetail(domainId);
    } catch (err) { toast(err.message, "err"); }
  });
}

async function recordAction(act, recordId, host, targetStatus, domainId, domain) {
  try {
    if (act === "edit") {
      // 从当前列表重新取该记录的完整数据
      const { items } = await api(`/api/v1/domains/${domainId}/records?page_size=200`);
      const rec = items.find((r) => String(r.id) === String(recordId));
      if (!rec) return toast("记录不存在，可能已被同步移除", "err");
      showRecordForm(domainId, domain, rec);
    } else if (act === "toggle") {
      const enable = targetStatus === "enabled";
      await api(`/api/v1/records/${recordId}/status`, { method: "POST", body: { enabled: enable } });
      toast(`${enable ? "已启用" : "已停用"}记录 ${host}（已回写平台）`, "ok");
      renderDomainDetail(domainId);
    } else if (act === "delete") {
      if (!confirm(`确认删除记录「${host}」？\n\n将同时从平台删除该解析记录。`)) return;
      await api(`/api/v1/records/${recordId}`, { method: "DELETE" });
      toast(`记录 ${host} 已删除（已回写平台）`, "ok");
      renderDomainDetail(domainId);
    }
  } catch (err) { toast(err.message, "err"); }
}

// ---------- 路由 ----------

function setActiveNav(name) {
  document.querySelectorAll("#topbar nav a").forEach((a) => {
    a.classList.toggle("active", a.dataset.nav === name);
  });
}

async function route() {
  const hash = location.hash || "#/";
  try {
    if (hash.startsWith("#/setup")) return renderSetup();
    if (hash.startsWith("#/login")) return renderLogin();
    // 受保护页面：检查登录态
    const status = await api("/api/v1/auth/status");
    if (!status.initialized) { location.hash = "#/setup"; return; }
    if (!status.authenticated) { location.hash = "#/login"; return; }

    const mDomain = hash.match(/^#\/domain\/(\d+)/);
    if (mDomain) return await renderDomainDetail(Number(mDomain[1]));
    if (hash.startsWith("#/domains")) return await renderDomains();
    if (hash.startsWith("#/accounts")) return await renderAccounts();
    location.hash = "#/accounts";
  } catch (err) {
    $app.innerHTML = `<div class="empty error-note">${esc(err.message)}</div>`;
  }
}

document.getElementById("logout-btn").addEventListener("click", async () => {
  try { await api("/api/v1/auth/logout", { method: "POST" }); } catch { /* 忽略 */ }
  toast("已登出", "ok");
  location.hash = "#/login";
});

window.addEventListener("hashchange", route);
route();
