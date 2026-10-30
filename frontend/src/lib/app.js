// UniDNS 前端共享层：API 封装、工具、toast、modal、登录守卫

// 注册 Service Worker（仅静态资源缓存）；HTTP 等非安全上下文注册失败时静默忽略
if ("serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js").catch(() => {});
  });
}

export function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

export function fmtTime(s) {
  if (!s) return "—";
  // 后端存 UTC（RFC3339），转换为浏览器本地时间显示
  const d = new Date(s);
  if (isNaN(d)) return String(s).replace("T", " ").replace(/([+-]\d\d:\d\d|Z)$/, "");
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

export async function api(path, options = {}) {
  const opts = { headers: {}, ...options };
  if (opts.body && typeof opts.body !== "string") {
    opts.body = JSON.stringify(opts.body);
    opts.headers["Content-Type"] = "application/json";
  }
  const res = await fetch(path, opts);
  if (res.status === 401 && !path.startsWith("/api/v1/auth/")) {
    location.href = "/login";
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

// ---------- Toast ----------

let toastTimer = null;
export function toast(msg, kind = "") {
  let el = document.querySelector(".toast");
  if (!el) {
    el = document.createElement("div");
    el.className = "toast";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.className = "toast show " + kind;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove("show"), 3400);
}

// ---------- 复制 ----------

/**
 * 复制文本到剪贴板，成功/失败均给出 toast。
 * 优先 navigator.clipboard.writeText（需安全上下文 HTTPS/localhost）；
 * HTTP 等非安全上下文回退隐藏 textarea + execCommand('copy')。
 * 返回是否复制成功。
 */
export async function copyText(text) {
  const value = String(text ?? "");
  let ok = false;
  if (navigator.clipboard?.writeText) {
    try { await navigator.clipboard.writeText(value); ok = true; } catch { /* 走回退 */ }
  }
  if (!ok) {
    try {
      const ta = document.createElement("textarea");
      ta.value = value;
      ta.setAttribute("readonly", "");
      ta.style.cssText = "position:fixed;left:-9999px;top:0;opacity:0";
      document.body.appendChild(ta);
      ta.select();
      ta.setSelectionRange(0, value.length);
      ok = document.execCommand("copy");
      ta.remove();
    } catch { ok = false; }
  }
  if (ok) toast("已复制到剪贴板", "ok");
  else toast("复制失败，请手动选择复制", "err");
  return ok;
}

// ---------- Modal ----------

let scrollLocked = false;
let savedBodyPaddingRight = "";

// 弹窗打开期间锁定背景滚动；补偿滚动条宽度避免桌面端内容横向跳动
function lockBackgroundScroll() {
  if (scrollLocked) return;
  scrollLocked = true;
  const scrollbarGap = window.innerWidth - document.documentElement.clientWidth;
  savedBodyPaddingRight = document.body.style.paddingRight;
  if (scrollbarGap > 0) document.body.style.paddingRight = `${scrollbarGap}px`;
  document.body.style.overflow = "hidden";
}

function unlockBackgroundScroll() {
  if (!scrollLocked) return;
  scrollLocked = false;
  document.body.style.overflow = "";
  document.body.style.paddingRight = savedBodyPaddingRight;
}

// 弹窗可能通过底部按钮、遮罩、ESC 或直接 remove() 关闭，统一在最后一个弹窗移除后解锁
new MutationObserver(() => {
  if (!document.querySelector(".modal-mask")) unlockBackgroundScroll();
}).observe(document.body, { childList: true });

/**
 * 打开弹窗。content 为 HTML 字符串；返回 mask 元素。
 * modal({ title, body, footer }) 结构化为 .modal-header/.modal-body/.modal-footer。
 */
export function modal(html) {
  const mask = document.createElement("div");
  mask.className = "modal-mask";
  mask.innerHTML = `<div class="modal">${html}</div>`;
  mask.addEventListener("click", (e) => { if (e.target === mask) mask.remove(); });
  document.body.appendChild(mask);
  lockBackgroundScroll();
  const firstInput = mask.querySelector("input, select");
  if (firstInput) setTimeout(() => firstInput.focus(), 50);
  return mask;
}

export function modalFrame(title, bodyHtml, footerHtml = "", showClose = true) {
  return modal(`
    <div class="modal-header"><h2>${esc(title)}</h2>
      ${showClose ? '<button class="btn ghost small" data-close aria-label="关闭">✕</button>' : ""}</div>
    <div class="modal-body">${bodyHtml}</div>
    ${footerHtml ? `<div class="modal-footer">${footerHtml}</div>` : ""}
  `);
}

// 点击关闭按钮
document.addEventListener("click", (e) => {
  const btn = e.target.closest("[data-close]");
  if (btn) btn.closest(".modal-mask")?.remove();
});
// ESC 关闭最上层弹窗
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    const masks = document.querySelectorAll(".modal-mask");
    masks[masks.length - 1]?.remove();
  }
});

// ---------- 登录守卫 ----------

/**
 * 页面加载时校验登录态。
 * 返回 { initialized, authenticated }；未初始化跳 /setup，未登录跳 /login。
 */
export async function requireAuth() {
  const status = await api("/api/v1/auth/status");
  if (!status.initialized) { location.replace("/setup"); throw new Error("redirect"); }
  if (!status.authenticated) { location.replace("/login"); throw new Error("redirect"); }
  // 校验通过后再显示主布局，避免未登录时的闪烁
  document.getElementById("app-shell")?.style.setProperty("visibility", "visible");
  return status;
}

export async function logout() {
  try { await api("/api/v1/auth/logout", { method: "POST" }); } catch { /* 忽略 */ }
  location.href = "/login";
}

// ---------- 平台常量 ----------

export const PROVIDERS = {
  aliyun: { label: "阿里云", fields: [
    { key: "access_key_id", label: "AccessKey ID", hint: "阿里云控制台创建的 AccessKey ID" },
    { key: "access_key_secret", label: "AccessKey Secret", hint: "仅要求云解析 DNS 权限" },
  ]},
  dnspod: { label: "腾讯云 DNSPod（国内站）", fields: secretFields("腾讯云国内站 API 密钥") },
  dnspod_intl: { label: "腾讯云 DNSPod（国际站）", fields: secretFields("腾讯云国际站（tencentcloud.com）API 密钥") },
  edgeone: { label: "EdgeOne（国内站）", fields: secretFields("EdgeOne 国内站 API 密钥", true) },
  edgeone_intl: { label: "EdgeOne（国际站）", fields: secretFields("EdgeOne 国际站（edgeone.ai）API 密钥", true) },
  cloudflare: { label: "CloudFlare", fields: [
    { key: "api_token", label: "API Token", hint: "Zone.DNS 编辑权限的 API Token" },
  ]},
  volcengine: { label: "火山引擎", fields: [
    { key: "access_key_id", label: "AccessKey ID", hint: "火山引擎 IAM 创建的 AccessKey ID" },
    { key: "access_key_secret", label: "Secret Access Key", hint: "仅要求云解析 DNS 权限" },
  ]},
};

function secretFields(hint, isEdgeOne = false) {
  return [
    { key: "secret_id", label: "SecretId", hint },
    { key: "secret_key", label: "SecretKey", hint: isEdgeOne ? "要求 EdgeOne（teo）读写权限，站点需为 NS 接入" : "仅要求解析权限" },
  ];
}

export const RECORD_TYPES = ["A", "AAAA", "CNAME", "MX", "TXT", "NS", "SRV", "CAA"];

/**
 * 统一各平台域名状态的展示：归一化为中文徽章。
 * 各平台原始值：active（CloudFlare/EdgeOne）、ENABLE（阿里云/DNSPod）、pending 等。
 */
export function zoneStatusBadge(status) {
  const s = String(status || "").trim().toLowerCase();
  if (!s) return '<span class="badge outline">—</span>';
  if (["active", "enable", "enabled"].includes(s)) {
    return '<span class="badge ok"><span class="dot"></span>正常</span>';
  }
  if (s === "pending") {
    return '<span class="badge warn"><span class="dot"></span>待生效</span>';
  }
  if (["pause", "paused", "disable", "disabled"].includes(s)) {
    return '<span class="badge warn"><span class="dot"></span>已暂停</span>';
  }
  // 未知状态：原样小写展示，避免误导
  return `<span class="badge outline">${esc(s)}</span>`;
}

/**
 * 轮询后台同步任务直到结束（success/failed）。
 * onDone(result) 在结束时回调；最长等待 3 分钟。
 */
export async function pollRun(runId, onDone, intervalMs = 2000, maxTimes = 90) {
  for (let i = 0; i < maxTimes; i++) {
    const r = await api(`/api/v1/sync/runs/${runId}`);
    if (r.status !== "running") {
      onDone?.(r);
      return r;
    }
    await new Promise((res) => setTimeout(res, intervalMs));
  }
  throw new Error("同步任务超时（超过 3 分钟未完成），可稍后在同步历史中查看结果");
}

// 按钮忙碌态辅助：await busy(btn, fn)
export async function busy(btn, fn) {
  if (!btn) return fn();
  const old = btn.textContent;
  btn.classList.add("loading");
  btn.disabled = true;
  try {
    return await fn();
  } finally {
    btn.classList.remove("loading");
    btn.disabled = false;
    btn.textContent = old;
  }
}

// ---------- 分页控件 ----------

/**
 * 生成分页控件 HTML：总条数、上一页/下一页、页码窗口（当前页前后各 2 页 + 首末页，超出用省略号）、
 * 每页条数切换（10/20/50/100）。仅一页时仍渲染（保留条数切换）。页码点击用 bindPager(onPage, onPageSize) 绑定。
 */
export function pager({ total, page, page_size }) {
  const pages = Math.max(1, Math.ceil(total / page_size));
  const SIZES = [10, 20, 50, 100];
  const items = [];
  const marks = new Set([1, pages]);
  for (let i = page - 2; i <= page + 2; i++) {
    if (i >= 1 && i <= pages) marks.add(i);
  }
  const ordered = [...marks].sort((a, b) => a - b);
  let prev = 0;
  for (const n of ordered) {
    if (n - prev > 1) items.push(`<span class="pager-ellipsis">…</span>`);
    items.push(
      n === page
        ? `<span class="pager-num current">${n}</span>`
        : `<a href="javascript:void(0)" class="pager-num" data-page="${n}">${n}</a>`,
    );
    prev = n;
  }
  const nav = (p, label, disabled) =>
    disabled
      ? `<span class="pager-num disabled">${label}</span>`
      : `<a href="javascript:void(0)" class="pager-num" data-page="${p}">${label}</a>`;
  return `
    <div class="pager">
      <span class="pager-total">共 ${total} 条</span>
      <span class="pager-pages">
        ${nav(page - 1, "上一页", page <= 1)}
        ${items.join("")}
        ${nav(page + 1, "下一页", page >= pages)}
      </span>
      <span class="pager-size">
        每页
        <select data-pager-size>
          ${SIZES.map((s) => `<option value="${s}" ${s === page_size ? "selected" : ""}>${s}</option>`).join("")}
        </select>
        条
      </span>
    </div>`;
}

/** 绑定分页控件交互：页码/上一页/下一页 → onPage(page)；每页条数变化 → onPageSize(size)。 */
export function bindPager(onPage, onPageSize) {
  document.querySelectorAll(".pager [data-page]").forEach((el) => {
    el.addEventListener("click", () => {
      const p = Number(el.dataset.page);
      if (p >= 1) onPage(p);
    });
  });
  document.querySelectorAll(".pager [data-pager-size]").forEach((el) => {
    el.addEventListener("change", () => {
      const s = Number(el.value);
      if (s >= 1 && onPageSize) onPageSize(s);
    });
  });
}
