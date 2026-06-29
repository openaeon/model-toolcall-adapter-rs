pub const INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Model Tool Call Adapter Setup</title>
  <style>
    :root {
      color-scheme: dark;
      --bg: #0b0f14;
      --panel: #121820;
      --panel2: #0f151d;
      --line: #28313d;
      --text: #eef4fb;
      --muted: #9aa8b7;
      --accent: #1f8a70;
      --accent2: #4f7cff;
      --ok: #42c77b;
      --bad: #ff6b66;
      --warn: #e6b85c;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      min-height: 100vh;
      background: var(--bg);
      color: var(--text);
      font: 14px/1.48 ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }
    main {
      min-height: 100vh;
      display: grid;
      grid-template-columns: 300px minmax(0, 1fr);
    }
    aside {
      border-right: 1px solid var(--line);
      background: var(--panel);
      padding: 22px;
    }
    section {
      padding: 28px;
      max-width: 980px;
      width: 100%;
    }
    h1 { margin: 0 0 6px; font-size: 22px; }
    h2 { margin: 0 0 12px; font-size: 18px; }
    p { margin: 0 0 12px; color: var(--muted); }
    label { display: block; margin: 12px 0 6px; color: var(--muted); font-size: 12px; }
    input, select, textarea {
      width: 100%;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: var(--panel2);
      color: var(--text);
      padding: 10px 11px;
      outline: none;
      font: inherit;
    }
    textarea {
      min-height: 120px;
      resize: vertical;
      font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      font-size: 12px;
    }
    pre {
      overflow: auto;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: var(--panel2);
      padding: 12px;
      white-space: pre-wrap;
      word-break: break-word;
      color: #d8e5f2;
      font: 12px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    }
    button {
      border: 1px solid transparent;
      border-radius: 6px;
      background: var(--accent);
      color: white;
      padding: 10px 13px;
      cursor: pointer;
      font-weight: 700;
    }
    button.secondary { background: transparent; border-color: var(--line); color: var(--text); }
    button.blue { background: var(--accent2); }
    button:disabled { opacity: .55; cursor: not-allowed; }
    .steps { display: grid; gap: 10px; margin-top: 22px; }
    .step-nav {
      border: 1px solid var(--line);
      border-radius: 8px;
      padding: 12px;
      color: var(--muted);
      background: rgba(255,255,255,.02);
    }
    .step-nav.active { color: var(--text); border-color: var(--accent); background: rgba(31,138,112,.12); }
    .step { display: none; }
    .step.active { display: block; }
    .panel {
      border: 1px solid var(--line);
      border-radius: 8px;
      background: var(--panel);
      padding: 18px;
      margin-bottom: 14px;
    }
    .grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px; }
    .row { display: flex; gap: 10px; align-items: center; flex-wrap: wrap; }
    .status { min-height: 20px; margin-top: 10px; color: var(--muted); font-size: 12px; }
    .status.ok { color: var(--ok); }
    .status.bad { color: var(--bad); }
    .status.warn { color: var(--warn); }
    .kv { display: grid; grid-template-columns: 160px minmax(0, 1fr); gap: 8px; padding: 6px 0; border-bottom: 1px solid rgba(40,49,61,.55); }
    .kv:last-child { border-bottom: 0; }
    .key { color: var(--muted); }
    .value { word-break: break-all; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 12px; }
    @media (max-width: 820px) {
      main { grid-template-columns: 1fr; }
      aside { border-right: 0; border-bottom: 1px solid var(--line); }
      section { padding: 18px; }
      .grid { grid-template-columns: 1fr; }
      .kv { grid-template-columns: 1fr; }
    }
  </style>
</head>
<body>
  <main>
    <aside>
      <h1>Adapter Setup</h1>
      <p>把不支持工具调用的模型桥接成 OpenAI 风格接口。</p>
      <div class="steps">
        <div id="nav1" class="step-nav active">1. 选择供应商</div>
        <div id="nav2" class="step-nav">2. 登录 DeepSeek</div>
        <div id="nav3" class="step-nav">3. 启动桥接</div>
      </div>
    </aside>
    <section>
      <div id="step1" class="step active">
        <div class="panel">
          <h2>选择模型供应商</h2>
          <p>这个选择会写入本地配置文件。后续请求仍然可以用 header 覆盖。</p>
          <label>供应商</label>
          <select id="provider">
            <option value="deepseek-web">DeepSeek Web</option>
            <option value="openai-compatible">OpenAI-compatible</option>
          </select>
          <div class="row" style="margin-top:14px">
            <button id="saveProvider">保存并继续</button>
            <button id="reloadState" class="secondary">刷新状态</button>
          </div>
          <div id="providerStatus" class="status"></div>
        </div>
        <div class="panel">
          <h2>当前配置</h2>
          <div id="stateView"></div>
        </div>
      </div>

      <div id="step2" class="step">
        <div class="panel">
          <h2>DeepSeek Web 登录</h2>
          <p>adapter 会启动一个独立浏览器 profile，并开启 DevTools 调试端口。只读取这个受控浏览器里的 DeepSeek session。</p>
          <div class="row">
            <button id="startBrowser">打开受控浏览器登录</button>
            <button id="captureSession" class="blue">已登录，捕获 Session</button>
            <button id="skipLogin" class="secondary">跳过</button>
          </div>
          <div id="browserStatus" class="status"></div>
        </div>
        <div class="panel">
          <h2>手动 fallback</h2>
          <p>如果浏览器调试端口不可用，可以粘贴 Session JSON 或 Cookie 保存。</p>
          <textarea id="manualSession" placeholder='{"cookie":"ds_session=...; ...","bearer":"optional-token"}'></textarea>
          <div class="row" style="margin-top:10px">
            <button id="saveManualSession" class="secondary">保存手动 Session</button>
          </div>
          <div id="manualStatus" class="status"></div>
        </div>
      </div>

      <div id="step3" class="step">
        <div class="panel">
          <h2>桥接接口已准备</h2>
          <p>把下面的 Base URL 和 Key 配到你的客户端。工具由你的 runtime 执行，adapter 只输出标准工具调用。</p>
          <div id="bridgeView"></div>
          <div class="row" style="margin-top:14px">
            <button id="copyKey">复制 Key</button>
            <button id="fetchModels" class="secondary">验证模型列表</button>
            <button id="configureCodex" class="blue">一键配置 Codex</button>
          </div>
          <div id="bridgeStatus" class="status"></div>
        </div>
        <div class="grid">
          <div class="panel">
            <h2>Codex 配置</h2>
            <pre id="codexExample"></pre>
          </div>
          <div class="panel">
            <h2>Responses 示例</h2>
            <pre id="responsesExample"></pre>
          </div>
          <div class="panel">
            <h2>Chat Completions 示例</h2>
            <pre id="chatExample"></pre>
          </div>
        </div>
      </div>
    </section>
  </main>

  <script>
    const $ = (id) => document.getElementById(id);
    const app = { state: null };

    function adapterUrl(path) {
      return `${location.origin}${path}`;
    }

    function authHeaders() {
      const key = app.state?.setup?.adapter_api_key || "";
      const headers = { "content-type": "application/json" };
      if (key) headers.authorization = `Bearer ${key}`;
      return headers;
    }

    function setupHeaders() {
      return { "content-type": "application/json" };
    }

    function setStep(step) {
      [1, 2, 3].forEach((n) => {
        $(`step${n}`).classList.toggle("active", n === step);
        $(`nav${n}`).classList.toggle("active", n === step);
      });
    }

    function setStatus(id, text, kind = "") {
      const el = $(id);
      el.textContent = text;
      el.className = `status ${kind}`;
    }

    async function loadState() {
      const res = await fetch(adapterUrl("/setup/state"));
      const body = await res.json();
      if (!res.ok) throw new Error(body?.error?.message || res.statusText);
      app.state = body;
      $("provider").value = body.setup.provider || "deepseek-web";
      renderState();
      renderBridge();
      return body;
    }

    function renderState() {
      const s = app.state?.setup || {};
      const session = app.state?.deepseek_session || {};
      $("stateView").innerHTML = [
        kv("Config File", s.config_file || ""),
        kv("Provider", s.provider || ""),
        kv("Adapter Key", s.adapter_api_key || ""),
        kv("Session", session.configured ? `${session.source} · ${session.format} · ${session.bytes} bytes` : "missing")
      ].join("");
    }

    function renderBridge() {
      const s = app.state?.setup || {};
      const provider = s.provider || "deepseek-web";
      const model = provider === "deepseek-web" ? "deepseek-web/reasoner" : (s.upstream_model || "local-model");
      $("bridgeView").innerHTML = [
        kv("Base URL", s.openai_base_url || `${location.origin}/v1`),
        kv("Adapter Key", s.adapter_api_key || ""),
        kv("Provider", provider),
        kv("Model", model)
      ].join("");
      $("codexExample").textContent = `[model_providers.ModelToolCallAdapter]
name = "ModelToolCallAdapter"
base_url = "${s.openai_base_url || `${location.origin}/v1`}"
wire_api = "responses"
requires_openai_auth = true

# auth.json:
{ "OPENAI_API_KEY": "${s.adapter_api_key || "YOUR_KEY"}" }`;
      $("responsesExample").textContent = `curl ${location.origin}/v1/responses \\
  -H 'content-type: application/json' \\
  -H 'authorization: Bearer ${s.adapter_api_key || "YOUR_KEY"}' \\
  -d '${JSON.stringify({
    model,
    input: "需要外部信息时先发起工具调用",
    tools: [{ type: "function", name: "search_web", description: "Search by query", parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] } }]
  }, null, 2)}'`;
      $("chatExample").textContent = `curl ${location.origin}/v1/chat/completions \\
  -H 'content-type: application/json' \\
  -H 'authorization: Bearer ${s.adapter_api_key || "YOUR_KEY"}' \\
  -d '${JSON.stringify({
    model,
    messages: [{ role: "user", content: "查一下北京天气" }],
    tools: [{ type: "function", function: { name: "get_weather", description: "Get weather by city", parameters: { type: "object", properties: { city: { type: "string" } }, required: ["city"] } } }]
  }, null, 2)}'`;
    }

    function kv(key, value) {
      return `<div class="kv"><div class="key">${escapeHtml(key)}</div><div class="value">${escapeHtml(value)}</div></div>`;
    }

    async function saveProvider() {
      setStatus("providerStatus", "正在保存...");
      const provider = $("provider").value;
      const res = await fetch(adapterUrl("/setup/provider"), {
        method: "POST",
        headers: setupHeaders(),
        body: JSON.stringify({ provider })
      });
      const body = await res.json();
      if (!res.ok) throw new Error(body?.error?.message || res.statusText);
      await loadState();
      setStatus("providerStatus", "已保存", "ok");
      setStep(provider === "deepseek-web" ? 2 : 3);
    }

    async function startBrowser() {
      setStatus("browserStatus", "正在启动受控浏览器...");
      $("startBrowser").disabled = true;
      try {
        const res = await fetch(adapterUrl("/setup/deepseek-browser/start"), {
          method: "POST",
          headers: setupHeaders()
        });
        const body = await res.json();
        if (!res.ok) throw new Error(body?.error?.message || res.statusText);
        setStatus("browserStatus", `浏览器已打开。登录后回来点击捕获 Session。调试端口 ${body.port}`, "ok");
        await loadState();
      } catch (err) {
        setStatus("browserStatus", err.message || String(err), "bad");
      } finally {
        $("startBrowser").disabled = false;
      }
    }

    async function captureSession() {
      setStatus("browserStatus", "正在从受控浏览器捕获 Session...");
      $("captureSession").disabled = true;
      try {
        const res = await fetch(adapterUrl("/setup/deepseek-browser/capture"), {
          method: "POST",
          headers: setupHeaders(),
          body: JSON.stringify({})
        });
        const body = await res.json();
        if (!res.ok) throw new Error(body?.error?.message || res.statusText);
        await loadState();
        setStatus("browserStatus", `已保存 Session：${body.session_file}`, "ok");
        setStep(3);
      } catch (err) {
        setStatus("browserStatus", err.message || String(err), "bad");
      } finally {
        $("captureSession").disabled = false;
      }
    }

    async function saveManualSession() {
      const session = $("manualSession").value.trim();
      if (!session) return setStatus("manualStatus", "请先粘贴 Session JSON 或 Cookie", "warn");
      setStatus("manualStatus", "正在保存...");
      const res = await fetch(adapterUrl("/deepseek-web/session"), {
        method: "POST",
        headers: authHeaders(),
        body: JSON.stringify({ session })
      });
      const body = await res.json();
      if (!res.ok) throw new Error(body?.error?.message || res.statusText);
      await loadState();
      setStatus("manualStatus", `已保存：${body.session_file}`, "ok");
      setStep(3);
    }

    async function fetchModels() {
      setStatus("bridgeStatus", "正在验证 /v1/models...");
      const provider = app.state?.setup?.provider || "deepseek-web";
      const headers = authHeaders();
      headers["x-upstream-provider"] = provider;
      const res = await fetch(adapterUrl("/v1/models"), { headers });
      const body = await res.json();
      if (!res.ok) return setStatus("bridgeStatus", body?.error?.message || res.statusText, "bad");
      const models = (body.data || []).map((m) => m.id).join(", ");
      setStatus("bridgeStatus", `模型可用：${models}`, "ok");
    }

    async function copyKey() {
      const key = app.state?.setup?.adapter_api_key || "";
      await navigator.clipboard.writeText(key);
      setStatus("bridgeStatus", "Key 已复制", "ok");
    }

    async function configureCodex() {
      setStatus("bridgeStatus", "正在写入 ~/.codex/config.toml 和 auth.json...");
      $("configureCodex").disabled = true;
      try {
        const res = await fetch(adapterUrl("/setup/codex/apply"), {
          method: "POST",
          headers: setupHeaders()
        });
        const body = await res.json();
        if (!res.ok) throw new Error(body?.error?.message || res.statusText);
        const backups = (body.backups || []).length ? `；备份：${body.backups.join(", ")}` : "";
        setStatus("bridgeStatus", `Codex 已配置。重启 Codex 后使用 ${body.provider} / ${body.model}${backups}`, "ok");
      } catch (err) {
        setStatus("bridgeStatus", err.message || String(err), "bad");
      } finally {
        $("configureCodex").disabled = false;
      }
    }

    function escapeHtml(value) {
      return String(value).replace(/[&<>"']/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch]));
    }

    $("saveProvider").addEventListener("click", () => saveProvider().catch((err) => setStatus("providerStatus", err.message || String(err), "bad")));
    $("reloadState").addEventListener("click", () => loadState().catch((err) => setStatus("providerStatus", err.message || String(err), "bad")));
    $("startBrowser").addEventListener("click", startBrowser);
    $("captureSession").addEventListener("click", captureSession);
    $("skipLogin").addEventListener("click", () => setStep(3));
    $("saveManualSession").addEventListener("click", () => saveManualSession().catch((err) => setStatus("manualStatus", err.message || String(err), "bad")));
    $("fetchModels").addEventListener("click", fetchModels);
    $("copyKey").addEventListener("click", copyKey);
    $("configureCodex").addEventListener("click", configureCodex);

    loadState()
      .then((state) => setStep(state.setup.provider === "deepseek-web" && !state.deepseek_session.configured ? 2 : 1))
      .catch((err) => setStatus("providerStatus", err.message || String(err), "bad"));
  </script>
</body>
</html>
"#;
