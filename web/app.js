(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = { config: null, source: null, log: [], reconnectTimer: null, retryDelay: 500 };
  const phaseLabels = { idle: "Idle", starting: "Starting", recording: "Recording", stopping: "Stopping" };

  function setConnection(connected) {
    $("connection-dot").parentElement.classList.toggle("connected", connected);
    $("connection-text").textContent = connected ? "实时连接" : "连接断开";
  }

  function message(text, error = false) {
    const element = $("config-message");
    element.textContent = text;
    element.style.color = error ? "var(--danger)" : "var(--ok)";
  }

  async function request(path, options = {}) {
    const response = await fetch(path, { headers: { "Content-Type": "application/json", ...(options.headers || {}) }, ...options });
    if (!response.ok) {
      let detail = `HTTP ${response.status}`;
      try { detail = (await response.json()).error.message || detail; } catch (_) { /* non-JSON response */ }
      throw new Error(detail);
    }
    return response.status === 204 ? null : response.json();
  }

  function renderSnapshot(snapshot) {
    snapshot = snapshot?.payload || snapshot;
    const phase = snapshot.phase || "idle";
    const phaseName = (typeof phase === "string" ? phase : Object.keys(phase)[0] || "idle").toLowerCase();
    const session = typeof phase === "object" ? phase[phaseName]?.session_id : null;
    $("phase").textContent = phaseLabels[phaseName] || phaseName;
    $("session-detail").textContent = session ? `Session ${session}` : (snapshot.last_error?.message || "等待开始录音");
    $("start-button").disabled = phaseName !== "idle";
    $("stop-button").disabled = phaseName === "idle";
    const device = snapshot.device || {};
    $("device-name").textContent = device.available ? (device.name || "ReSpeaker") : "未检测到设备";
    $("device-detail").textContent = device.error || "WASAPI 独占 / 16 kHz / 6 ch";
    const recording = snapshot.recording || {};
    $("frame-count").textContent = Number(recording.captured_frames || 0).toLocaleString();
    $("duration").textContent = `${Number(recording.elapsed_secs || 0).toFixed(1)} s`;
    $("config-warning").textContent = snapshot.config_warning || "";
    if (snapshot.pipeline) {
      $("pipeline-state").textContent = snapshot.pipeline.enabled ? "Pipeline 已启用" : "Pipeline 未启用";
    }
  }

  function renderDoa(data) {
    data = data?.payload || data;
    const tracked = Number(data.tracked_angle_deg);
    const raw = Number(data.raw_angle_deg);
    $("tracked-angle").textContent = Number.isFinite(tracked) ? `${tracked.toFixed(1)}°` : "--.-°";
    $("raw-angle").textContent = Number.isFinite(raw) ? `${raw.toFixed(1)}°` : "--.-°";
    $("doa-confidence").textContent = Number.isFinite(Number(data.confidence)) ? Number(data.confidence).toFixed(2) : "--";
    $("doa-status").textContent = data.status || "searching";
    if (Number.isFinite(tracked)) $("doa-needle").style.transform = `translate(-50%, -100%) rotate(${180 + tracked}deg)`;
  }

  function renderConfig(config) {
    state.config = config;
    $("config-editor").value = JSON.stringify(config, null, 2);
    $("duration-seconds").value = config.recording?.duration_seconds ?? 0;
    $("out-dir").value = config.recording?.out_dir ?? "";
    $("recording-prefix").value = config.recording?.prefix ?? "";
    $("pipeline-enabled").checked = Boolean(config.pipeline_enabled);
    const modules = Array.isArray(config.pipeline?.modules) ? config.pipeline.modules : [];
    const doa = modules.find((module) => module.type === "doa") || {};
    const bf = modules.find((module) => module.type === "beamformer") || {};
    $("doa-enabled").checked = Boolean(doa.enabled);
    $("doa-csv").checked = Boolean(doa.csv);
    $("doa-angle-offset").value = doa.angle_offset_deg ?? 0;
    $("doa-clockwise").checked = Boolean(doa.clockwise);
    $("doa-acquire-confidence").value = doa.acquire_confidence ?? 0.55;
    $("doa-update-confidence").value = doa.update_confidence ?? 0.35;
    $("bf-enabled").checked = Boolean(bf.enabled);
    $("bf-algorithm").value = bf.algorithm ?? "robust_superdirective";
    $("bf-direction-source").value = bf.direction_source ?? "doa";
    $("bf-fixed-angle").value = bf.fixed_internal_angle_deg ?? 0;
    $("bf-fallback-angle").value = bf.fallback_internal_angle_deg ?? 0;
    $("bf-smoothing").value = bf.direction_smoothing_ms ?? 64;
    $("bf-output-gain").value = bf.output_gain_db ?? -3;
    $("bf-wav").checked = bf.wav !== false;
    $("bf-compare-wav").checked = Boolean(bf.compare_wav);
    $("bf-enable-drc").checked = bf.enable_drc !== false;
  }

  async function loadConfig() {
    const response = await request("/api/config");
    renderConfig(response.draft);
  }

  async function loadProfiles() {
    const response = await request("/api/profiles");
    const select = $("profile-select");
    select.replaceChildren();
    for (const name of response.profiles || []) {
      const option = document.createElement("option");
      option.value = name;
      option.textContent = name;
      select.append(option);
    }
    select.disabled = !select.options.length;
    $("load-profile-button").disabled = select.disabled;
  }

  async function loadRecordings() {
    const response = await request("/api/recordings");
    const container = $("recordings");
    container.replaceChildren();
    const recordings = [...(response.recordings || []), ...(response.trash || []).map((entry) => ({ ...entry, trashed: true }))];
    if (!recordings.length) {
      container.innerHTML = '<div class="empty-state">还没有录音记录。</div>';
      return;
    }
    for (const recording of recordings) container.append(renderRecording(recording));
  }

  function renderRecording(recording) {
    const card = document.createElement("article");
    card.className = "recording";
    const head = document.createElement("div");
    head.className = "recording-head";
    head.innerHTML = `<span class="recording-id" title="${escapeHtml(recording.id)}">${escapeHtml(recording.prefix)}</span><span class="recording-status">${escapeHtml(recording.status)}</span>`;
    card.append(head);
    const manifest = recording.manifest;
    const frames = manifest ? Number(manifest.captured_frames || 0).toLocaleString() : "legacy";
    const started = manifest?.started_at || "历史文件";
    const meta = document.createElement("div");
    meta.className = "recording-meta";
    meta.textContent = `${started} / ${frames} frames`;
    card.append(meta);
    const links = document.createElement("div");
    links.className = "file-links";
    const playable = [];
    for (const file of recording.files || []) {
      if (recording.trashed) continue;
      if (!file.exists) continue;
      const href = `/api/recordings/${encodeURIComponent(recording.id)}/files/${encodeURIComponent(file.kind)}`;
      const link = document.createElement("a");
      link.href = href;
      link.textContent = file.kind;
      link.download = "";
      links.append(link);
      if (["algo", "ref", "bf"].includes(file.kind)) playable.push(href);
    }
    if (playable.length) {
      const audio = document.createElement("audio");
      audio.className = "recording-audio";
      audio.controls = true;
      audio.preload = "none";
      audio.src = playable[0];
      card.append(audio);
    }
    const action = document.createElement("button");
    action.textContent = recording.trashed ? "恢复" : "移入回收站";
    action.addEventListener("click", async () => {
      try {
        const operation = recording.trashed ? "restore" : "trash";
        await request(`/api/recordings/${encodeURIComponent(recording.id)}/${operation}`, { method: "POST" });
        await loadRecordings();
      }
      catch (error) { message(error.message, true); }
    });
    links.append(action);
    card.append(links);
    return card;
  }

  function escapeHtml(value) {
    return String(value).replace(/[&<>"']/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[character]));
  }

  function addLog(type, data) {
    const line = `[${new Date().toLocaleTimeString()}] ${type} ${typeof data === "string" ? data : JSON.stringify(data)}`;
    state.log.unshift(line);
    state.log = state.log.slice(0, 80);
    $("event-log").textContent = state.log.join("\n");
  }

  function connectEvents() {
    const source = new EventSource("/api/events");
    state.source = source;
    source.onopen = () => {
      setConnection(true);
      state.retryDelay = 500;
      request("/api/status").then(renderSnapshot).catch(() => {});
    };
    source.onerror = () => {
      setConnection(false);
      source.close();
      if (!state.reconnectTimer) {
        const delay = state.retryDelay;
        state.retryDelay = Math.min(state.retryDelay * 2, 10_000);
        state.reconnectTimer = window.setTimeout(() => {
          state.reconnectTimer = null;
          connectEvents();
        }, delay);
      }
    };
    for (const type of ["state_snapshot", "device_status", "recording_progress", "pipeline_status", "doa", "bf_stats", "error", "session_finished"]) {
      source.addEventListener(type, (event) => {
        let data;
        try { data = JSON.parse(event.data); } catch (_) { data = event.data; }
        addLog(type, data);
        if (type === "state_snapshot") renderSnapshot(data);
        if (type === "doa") renderDoa(data);
        if (type === "bf_stats") {
          const stats = data.payload || data;
          $("bf-frames").textContent = `${Number(stats.output_frames || 0).toLocaleString()} frames`;
          $("bf-clipped").textContent = Number(stats.clipped_samples || 0).toLocaleString();
        }
        if (type === "pipeline_status") {
          const pipeline = data.payload || data;
          $("pipeline-state").textContent = pipeline.degraded ? `Pipeline 已降级: ${pipeline.error || "worker error"}` : "Pipeline 正常";
        }
        if (type === "recording_progress" && data.payload) {
          const progress = data.payload;
          $("frame-count").textContent = Number(progress.captured_frames || 0).toLocaleString();
          $("duration").textContent = `${Number(progress.elapsed_secs || 0).toFixed(1)} s`;
        }
        if (type === "session_finished") loadRecordings().catch((error) => message(error.message, true));
      });
    }
  }

  async function applyConfig() {
    try {
      const config = JSON.parse($("config-editor").value);
      config.recording = config.recording || {};
      config.recording.duration_seconds = Number($("duration-seconds").value || 0);
      config.recording.out_dir = $("out-dir").value;
      config.recording.prefix = $("recording-prefix").value.trim() || null;
      config.pipeline_enabled = $("pipeline-enabled").checked;
      config.pipeline = config.pipeline || { version: 1, modules: [] };
      config.pipeline.modules = Array.isArray(config.pipeline.modules) ? config.pipeline.modules : [];
      const updateModule = (type, values, enabled) => {
        const index = config.pipeline.modules.findIndex((module) => module.type === type);
        if (index >= 0) config.pipeline.modules[index] = { ...config.pipeline.modules[index], ...values };
        else if (enabled) config.pipeline.modules.push({ type, ...values });
      };
      const doaEnabled = $("doa-enabled").checked;
      updateModule("doa", {
        enabled: doaEnabled,
        csv: $("doa-csv").checked,
        angle_offset_deg: Number($("doa-angle-offset").value || 0),
        clockwise: $("doa-clockwise").checked,
        acquire_confidence: Number($("doa-acquire-confidence").value || 0),
        update_confidence: Number($("doa-update-confidence").value || 0),
      }, doaEnabled);
      const bfEnabled = $("bf-enabled").checked;
      updateModule("beamformer", {
        enabled: bfEnabled,
        algorithm: $("bf-algorithm").value,
        direction_source: $("bf-direction-source").value,
        fixed_internal_angle_deg: Number($("bf-fixed-angle").value || 0),
        fallback_internal_angle_deg: Number($("bf-fallback-angle").value || 0),
        direction_smoothing_ms: Number($("bf-smoothing").value || 0),
        output_gain_db: Number($("bf-output-gain").value || 0),
        wav: $("bf-wav").checked,
        compare_wav: $("bf-compare-wav").checked,
        enable_drc: $("bf-enable-drc").checked,
      }, bfEnabled);
      const doaIndex = config.pipeline.modules.findIndex((module) => module.type === "doa");
      const bfIndex = config.pipeline.modules.findIndex((module) => module.type === "beamformer");
      if (doaEnabled && bfEnabled && $("bf-direction-source").value === "doa" && doaIndex > bfIndex) {
        const [doaModule] = config.pipeline.modules.splice(doaIndex, 1);
        config.pipeline.modules.splice(bfIndex, 0, doaModule);
      }
      const response = await request("/api/config", { method: "PUT", body: JSON.stringify(config) });
      renderConfig(response.draft);
      message("draft 已应用");
    } catch (error) { message(error.message, true); }
  }

  async function action(path, label) {
    try { await request(path, { method: "POST" }); message(label); }
    catch (error) { message(error.message, true); }
  }

  $("start-button").addEventListener("click", () => action("/api/recordings/start", "录音启动中"));
  $("stop-button").addEventListener("click", () => action("/api/recordings/stop", "录音停止中"));
  $("apply-button").addEventListener("click", applyConfig);
  $("save-button").addEventListener("click", () => action("/api/config/save", "配置已保存"));
  $("reset-button").addEventListener("click", async () => { await action("/api/config/reset", "已恢复默认"); await loadConfig(); });
  $("import-button").addEventListener("click", () => $("import-file").click());
  $("import-file").addEventListener("change", async (event) => {
    const file = event.target.files?.[0];
    if (!file) return;
    try {
      const text = await file.text();
      const response = await request("/api/config/import", { method: "POST", headers: { "Content-Type": "text/plain" }, body: text });
      renderConfig(response.draft);
      message("TOML 已导入");
    } catch (error) { message(error.message, true); }
    event.target.value = "";
  });
  $("export-button").addEventListener("click", () => { window.location.href = "/api/config/export"; });
  $("refresh-button").addEventListener("click", () => loadRecordings().catch((error) => message(error.message, true)));
  $("clear-log-button").addEventListener("click", () => { state.log = []; $("event-log").textContent = ""; });
  $("load-profile-button").addEventListener("click", async () => {
    try { const response = await request(`/api/profiles/${encodeURIComponent($("profile-select").value)}`); renderConfig(response.draft); message("profile 已载入"); }
    catch (error) { message(error.message, true); }
  });
  $("save-profile-button").addEventListener("click", async () => {
    const name = $("profile-name").value.trim();
    try { const config = JSON.parse($("config-editor").value); await request(`/api/profiles/${encodeURIComponent(name)}`, { method: "PUT", body: JSON.stringify(config) }); await loadProfiles(); message("profile 已保存"); }
    catch (error) { message(error.message, true); }
  });
  document.querySelectorAll("[data-preset]").forEach((button) => button.addEventListener("click", async () => {
    try { const response = await request(`/api/profiles/${button.dataset.preset}`); renderConfig(response.draft); message(`${button.textContent} preset 已载入`); }
    catch (error) { message(error.message, true); }
  }));

  Promise.all([loadConfig(), loadProfiles(), loadRecordings(), request("/api/status").then(renderSnapshot)])
    .catch((error) => message(error.message, true));
  connectEvents();
})();
